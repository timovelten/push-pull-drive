#![allow(unused)]

pub mod config;
mod credential;
mod drive_api;
mod extensions;
pub mod oauth;
use crate::append_to_cpath_in_place;
use crate::file_repository::AsyncReader;
use crate::file_repository::Error as FileRepositoryError;
use crate::file_repository::Result as FileRepositoryResult;
use crate::gdrive::drive_api::MoreFileFields;
use crate::DirectoryInfo;
use crate::Result;
use crate::{
    append_to_relative_name,
    file_repository::FileRepository,
    gdrive::drive_api::DriveApi,
    pop_cpath,
    util::SemaphoreGuarded,
    FileInfo, PathInfo,
};
use anyhow::Context;
use anyhow::{anyhow, bail};
use async_recursion::async_recursion;
use cached::Cached;
use cached::TimedCache;
use cached::TimedSizedCache;
pub use config::*;
pub use credential::{FatAccessToken, GoogleCredential};
use futures::stream::FuturesUnordered;
use futures::AsyncRead;
use futures::StreamExt;
use std::collections::VecDeque;
use std::fmt::Write;
use std::pin::Pin;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use thiserror::Error;
use tokio::{sync::Semaphore, task::block_in_place};

use self::drive_api::FilesReq;

#[derive(Debug, Error)]
pub(super) enum Error {
    /// Returned if a HTTP request succeeded, but failed because of a non 2xx status code
    #[error("HTTP {method} {uri} failed with status code {code}")]
    HttpRequestFailed {
        method: http::Method,
        uri: http::Uri,
        code: http::StatusCode,
    },
}

#[derive(Debug)]
pub struct GDriveBackend {
    /// Google does not really like concurrent connections. In my personal testing,
    /// the limit seemed to be somewhere below 10.
    sem: Arc<tokio::sync::Semaphore>,

    /// There is a nasty race condition when using mkdirall, and this is the best way I can think of
    /// to not have this problem
    mkdir_lock: tokio::sync::Mutex<()>,

    client: reqwest::Client,

    /// Google drive does not use paths. Instead, it identifies files using ids.
    /// For example, there is no specific API to list all of the files in a directory (by path).
    /// If we know the id of a certain path, we therefore cache it here.
    /// For this, we assume that this object is generally short lived and pretend
    /// like concurrent modification is not an issue (someone could move the folder,
    /// for example. Then the cached ID would point to another path now.).
    id_cache: Arc<Mutex<TimedCache<String, Arc<String>>>>,

    credential: Arc<GoogleCredential>,
}

impl FileRepository for GDriveBackend {
    type Reader = Pin<Box<dyn AsyncReader>>;

    async fn read_dir(&self, cpath: &str, up_to_depth: u32) -> FileRepositoryResult<Vec<PathInfo>> {
        const BATCH_SIZE: usize = 12;

        // Another day, another (sort-of) BFS!
        let mut ret = Vec::new();

        let id = self.get_drive_id(cpath).await.unwrap();
        let mut queue = vec![(
            id,
            DirectoryInfo {
                cpath: cpath.to_owned(),
            },
        )];

        for depth in 0..up_to_depth {
            // Invariant: At this point, all directories in the queue are directories of depth `depth`
            // and depth <= up_to_depth.
            if queue.is_empty() {
                break;
            }

            let mut next_queue = Vec::new();
            let mut fts = FuturesUnordered::new();

            // Take up to BATCH_SIZE directories, and explore them using a single API call.
            // But: We can make several of these API calls concurrently, so let's do that :)
            for chunk in queue.chunks(BATCH_SIZE) {
                fts.push(async move {
                    let ret = self
                        .read_dirs(chunk)
                        .await
                        .map_err(FileRepositoryError::Other)?;

                    Ok(ret)
                });
            }

            while let Some(next) = fts.next().await {
                // TODO: Do not return immediately on error
                for (next_id, next_info) in next? {
                    if let PathInfo::Dir(info) = &next_info {
                        next_queue.push((next_id, info.clone()));
                    }
                    ret.push(next_info);
                }
            }

            drop(fts);

            queue = next_queue;
        }

        Ok(ret)
    }

    async fn get_metadata(&self, cpath: &str) -> FileRepositoryResult<PathInfo> {
        todo!()
    }

    async fn read_file(
        &self,
        relative_name: &str,
    ) -> FileRepositoryResult<(FileInfo, Self::Reader)> {
        todo!()
    }

    async fn write_file<R: crate::file_repository::AsyncReader>(
        &self,
        cpath: &str,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> FileRepositoryResult<FileInfo> {
        todo!()
    }

    async fn delete_file(&self, cpath: &str) -> FileRepositoryResult<()> {
        todo!()
    }

    async fn set_modified_time(
        &self,
        relative_name: &str,
        modtime: time::OffsetDateTime,
    ) -> FileRepositoryResult<()> {
        todo!()
    }
}

impl GDriveBackend {
    pub fn new(credential: GoogleCredential) -> Self {
        let client = reqwest::Client::builder().build().unwrap();

        Self {
            sem: Arc::new(Semaphore::new(8)),
            mkdir_lock: tokio::sync::Mutex::new(()),
            credential: Arc::new(credential),
            client,
            id_cache: Arc::new(Mutex::new(TimedCache::with_lifespan(60))),
        }
    }

    async fn make_drive_api(&self) -> anyhow::Result<SemaphoreGuarded<DriveApi>> {
        let permit = self.sem.clone().acquire_owned().await.unwrap();
        let token = block_in_place(|| self.credential.get_or_refresh_token())?;
        let inner = DriveApi::new(self.client.clone(), token);

        Ok(SemaphoreGuarded { permit, inner })
    }

    fn cache_id(&self, cpath: &str, id: &str) {
        log::debug!("Cache: path {cpath} has id {id}");
        self.id_cache
            .lock()
            .unwrap()
            .cache_set(cpath.to_owned(), Arc::new(id.to_owned()));
    }

    fn get_cached_id(&self, cpath: &str) -> Option<Arc<String>> {
        self.id_cache.lock().unwrap().cache_get(cpath).cloned()
    }

    /// Try and find the Google-Drive-ID of the given cpath. None of the results this function returns are cached.
    /// However, this function uses read_dirs under the hood, and therefore may update the cache when running.
    async fn get_drive_id(&self, cpath: &str) -> anyhow::Result<String> {
        // Trust me: It's not worth it to do this non-recursively, even if we then could
        // avoid the heap allocation thats caused by async_recursion.

        // In Google Drive, the root folder has an actual id (e.g. 0AGqh_9UFaLFoUk9PVA) and the "virtual" id
        // 'root' by which it can be accessed almost anywhere. Unfortunately, we need the actual id,
        // since the 'parents' field of a file contains the actual id (and not 'root'). We use this
        // behavior in the read_dir function, that concurrently fetches multiple directory contents
        // in a single request.
        // See also: https://developers.google.com/drive/api/guides/folder
        let root_file = self.make_drive_api().await?.files_get("root").await?;
        debug_assert_eq!(root_file.parents, None);
        self.cache_id(cpath, &root_file.id);

        if cpath.is_empty() {
            return Ok(root_file.id);
        }

        let mut current_cpath = "";
        let mut current_id = root_file.id;

        let sep_indices = cpath
            .match_indices('/')
            .map(|(i, _)| i)
            .chain(std::iter::once(cpath.len()));

        for i in sep_indices {
            let next_cpath = &cpath[..i];
            let files = self
                .read_dirs(&[(
                    current_id,
                    DirectoryInfo {
                        cpath: current_cpath.to_owned(),
                    },
                )])
                .await?;

            // Append the current part to cpath, to make the "target" cpath; the one we're looking for.
            let mut next_id = None;

            for (id, info) in files {
                if info.cpath() == next_cpath {
                    if next_id.is_some() {
                        // TODO: Warn that there are duplicate two files with the same name!
                    }
                    next_id = Some(id);
                }
            }

            if let Some(next_id) = next_id {
                current_id = next_id;
                current_cpath = next_cpath;
            } else {
                bail!(FileRepositoryError::NotFound {
                    cpath: next_cpath.to_owned()
                })
            }
        }

        log::debug!("Id of {cpath:?} is {current_id:?}");
        Ok(current_id)
    }

    #[async_recursion]
    async fn mkdir_all_impl(&self, relative_name: &str) -> anyhow::Result<String> {
        todo!()
    }

    /// Creates the given relative name and returns its id. If this path
    /// already exists and is the path of a file (and not a folder) this returns
    /// `Error::NotAFolder`.
    async fn mkdir_all(&self, relative_name: &str) -> anyhow::Result<String> {
        if relative_name.is_empty() {
            return self.get_drive_id("").await;
        }

        let _lock = self.mkdir_lock.lock().await;
        self.mkdir_all_impl(relative_name).await
    }

    async fn read_dirs(
        &self,
        targets: &[(String, DirectoryInfo)],
    ) -> anyhow::Result<Vec<(String, PathInfo)>> {
        let mut ret = Vec::new();

        // Build the google drive query string that looks for children of those ids.
        let query = {
            // Query string is of the form
            // trashed = false and ('abc' in parents or 'cdf' in parents)
            let mut q = String::from("trashed = false and (");
            for (i, (id, cpath)) in targets.iter().enumerate() {
                if i > 0 {
                    write!(q, " or ").unwrap();
                }
                write!(q, "'{}' in parents", id).unwrap();
            }
            // Don't forget the closing brace
            write!(q, ")").unwrap();

            q
        };

        log::debug!("Requesting files with query {query}");

        let files = self.make_drive_api().await?.file_list_all(&query).await?;
        for file in files {
            let parent_id = &file
                .parents
                .as_ref()
                .expect("Google drive files we request have a parent")[0];

            let parent_cpath = targets
                .iter()
                .find(|(id, _)| id == parent_id)
                .map(|(_, info)| &info.cpath)
                .with_context(|| format!("List of targets is {targets:?} and we recevied a file with parent id {parent_id:?}"))
                .expect("Every file we receive should have a parent we know of");

            // cpath of the file `file`
            let cpath = append_to_relative_name(parent_cpath, &file.name);

            let info = match file.more {
                Some(MoreFileFields::BinaryFile {
                    head_revision_id,
                    sha1_checksum,
                    size,
                }) => PathInfo::File(FileInfo {
                    cpath,
                    size,
                    modified: file.modified_time,
                    hash: hex::decode(sha1_checksum).expect("sha1checksum should be valid hex"),
                }),
                None if file.is_folder() => PathInfo::Dir(crate::DirectoryInfo { cpath }),
                _ => {
                    // TODO: We are most likely dealing with a Google docs file here. Skip those for now.
                    continue;
                }
            };

            ret.push((file.id, info));
        }

        Ok(ret)
    }

    async fn create_file<R: AsyncRead + Send>(&self, cpath: &str, src: R, len: u64) -> Result<()> {
        // We need to create the file. For that, we need to place it in the right path
        let (parent, filename) = pop_cpath(cpath);
        let parent_id = self.mkdir_all(parent).await?;

        let id = self
            .make_drive_api()
            .await?
            .files_create_upload(
                FilesReq {
                    name: Some(filename.to_string()),
                    parents: Some(vec![parent_id]),
                    ..Default::default()
                },
                src,
                len,
            )
            .await?
            .id;

        self.cache_id(cpath, &id);

        Ok(())
    }
}

#[derive(Default, Debug)]
struct IdCache {
    inner: Mutex<HashMap<String, String>>,
}

impl IdCache {
    pub fn update<K: ToString, V: ToString>(&self, key: K, val: V) {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), val.to_string());
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.inner.lock().unwrap().get(key).map(|x| x.to_owned())
    }

    pub fn delete(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }
}

const _: () = {
    const fn assert_send<T: Send>() {}
    const fn assert_sync<T: Send>() {}
    const fn assert_clone<T: Clone>() {}

    assert_send::<IdCache>();
    assert_sync::<IdCache>();

    assert_send::<GDriveBackend>();
};
