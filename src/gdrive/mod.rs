#![allow(unused)]

pub mod config;
mod credential;
mod drive_api;
mod extensions;
pub mod oauth;
use anyhow::Context;
use anyhow::{anyhow, bail};
pub use config::*;
use futures::stream::FuturesUnordered;
use itertools::Itertools;
use rusqlite::ffi::SQLITE_CANTOPEN_CONVPATH;

use crate::gdrive::drive_api::MoreFileFields;
use crate::DirectoryInfo;
use crate::Result;
use async_recursion::async_recursion;
pub use credential::{FatAccessToken, GoogleCredential};
use futures::AsyncRead;
use std::collections::VecDeque;
use std::fmt::Write;
use thiserror::Error;
use tokio::{sync::Semaphore, task::block_in_place};

use crate::file_repository::Error as FileRepositoryError;
use crate::file_repository::Result as FileRepositoryResult;
use crate::{
    append_to_relative_name,
    file_repository::{FileRepository, RemoteRepository},
    gdrive::drive_api::DriveApi,
    local::file_hasher::Sha1Hasher,
    pop_cpath,
    util::SemaphoreGuarded,
    FileInfo, PathInfo,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use self::drive_api::FilesReq;

#[derive(Debug, Error)]
pub(super) enum Error {
    /// Returned if an HTTP request succeeded, but failed because of a non 2xx status code
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
    id_cache: Arc<IdCache>,
    credential: Arc<GoogleCredential>,
}

impl FileRepository for GDriveBackend {
    type Reader = Box<dyn AsyncRead + Send + Unpin>;

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

            // Take up to BATCH_SIZE directories, and explore them using a single API call.
            // But: We can make several of these API calls concurrently, so let's do that :)
            for chunk in queue.chunks(BATCH_SIZE) {
                let next = self
                    .read_dirs(chunk)
                    .await
                    .map_err(|err| FileRepositoryError::Other(err))?;

                for (next_id, next_info) in next {
                    if let PathInfo::Dir(info) = &next_info {
                        next_queue.push((next_id, info.clone()));
                    }
                    ret.push(next_info);
                }
            }

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

impl RemoteRepository for GDriveBackend {
    type Hasher = Sha1Hasher;
}

impl GDriveBackend {
    pub fn new(credential: GoogleCredential) -> Self {
        let client = reqwest::Client::builder().build().unwrap();

        Self {
            sem: Arc::new(Semaphore::new(8)),
            mkdir_lock: tokio::sync::Mutex::new(()),
            credential: Arc::new(credential),
            client,
            id_cache: Default::default(),
        }
    }

    async fn make_drive_api(&self) -> anyhow::Result<SemaphoreGuarded<DriveApi>> {
        let permit = self.sem.clone().acquire_owned().await.unwrap();
        let token = block_in_place(|| self.credential.get_or_refresh_token())?;
        let inner = DriveApi::new(self.client.clone(), token);

        Ok(SemaphoreGuarded { permit, inner })
    }

    /// Try and find the Google-Drive-ID of the given relative name (path). If use_cache is set,
    /// the results may come from a cache. This cache is never invalidated for the lifetime
    /// of the object Self, since it is assumed that this object is generally short lived.
    /// If only_folders is set, this returns the Error::NotAFolder if *something* exists at the given
    /// relative path, but it is not a folder.
    #[async_recursion]
    async fn get_drive_id_advanced(
        &self,
        cpath: &str,
        use_cache: bool,
        only_folders: bool,
    ) -> anyhow::Result<String> {
        // Trust me: It's not worth it to do this non-recursively, even if we then could
        // avoid the heap allocation thats caused by async_recursion.

        if use_cache {
            if let Some(x) = self.id_cache.get(cpath) {
                return Ok(x.clone());
            }
        }

        // In Google Drive, the root folder has an actual id (e.g. 0AGqh_9UFaLFoUk9PVA) and the "virtual" id
        // 'root' by which it can be accessed almost anywhere. Unfortunately, we need the actual id,
        // since the 'parents' field of a file contains the actual id (and not 'root'). We use this
        // behavior in the read_dir function, that concurrently fetches multiple directory contents
        // in a single request.
        // See also: https://developers.google.com/drive/api/guides/folder
        if cpath.is_empty() {
            let f = self.make_drive_api().await?.files_get("root").await?;
            debug_assert_eq!(f.parents, None);

            self.id_cache.update(cpath, &f.id);
            return Ok(f.id);
        }

        // If not, find our parent, get its id, list its contents. We should be in that list.
        let (parent_cpath, _) = pop_cpath(cpath);
        let parent_id = self
            .get_drive_id_advanced(parent_cpath, use_cache, only_folders)
            .await?;

        // It would be possible here to only list the folders first, and check if
        // we are looking for a folder. This would only really be worth it when one assumes that
        // there are far less folders in a folder than files. I am unsure if that is the case in
        // general or if thats worth it.
        let mut ret = None;

        let files = self
            .make_drive_api()
            .await?
            .file_list_all(&format!("'{parent_id}' in parents and trashed=false"))
            .await?;

        for file in files {
            let this_relative_name = append_to_relative_name(parent_cpath, &file.name);
            // Use the opportunity to remember a lot of the ids
            self.id_cache.update(&this_relative_name, &file.id);

            if this_relative_name == cpath {
                if ret.is_some() {
                    println!(
                        "Warning: Folder {} [{}] contains two children with the same name",
                        parent_cpath, parent_id
                    );
                }

                if only_folders && !file.is_folder() {
                    bail!(FileRepositoryError::NotAFolder {
                        cpath: cpath.to_owned()
                    });
                }

                ret = Some(file.id)
            }
        }

        ret.ok_or_else(|| {
            anyhow!(FileRepositoryError::NotFound {
                cpath: cpath.to_string()
            })
        })
    }

    async fn get_drive_id(&self, relative_name: &str) -> anyhow::Result<String> {
        self.get_drive_id_advanced(relative_name, true, false).await
    }

    #[async_recursion]
    async fn mkdir_all_impl(&self, relative_name: &str) -> anyhow::Result<String> {
        // Base case: If this already exists, just return its id
        match self.get_drive_id_advanced(relative_name, true, true).await {
            Ok(id) => return Ok(id),
            Err(err) => match err.downcast_ref::<FileRepositoryError>() {
                Some(FileRepositoryError::NotFound { .. }) => {
                    // Path does not yet exist, so we find our parent and create a new folder there.
                    let (parent, name) = pop_cpath(relative_name);

                    let parent_id = self.mkdir_all_impl(parent).await?;
                    let req = drive_api::FilesReq {
                        name: Some(name.to_string()),
                        mime_type: Some("application/vnd.google-apps.folder".to_string()),
                        parents: Some(vec![parent_id]),
                        ..Default::default()
                    };

                    let id = self.make_drive_api().await?.files_create(req).await?.id;
                    self.id_cache.update(relative_name, &id);

                    Ok(id)
                }
                _ => bail!(err),
            },
        }
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
        log::debug!("Google drive query string is {query}");

        let files = self.make_drive_api().await?.file_list_all(&query).await?;

        for file in files {
            let parent_id = &file
                .parents
                .as_ref()
                .expect("Google drive files we request have a parent")[0];

            let parent_cpath = targets
                .iter()
                .find(|(id, _)| false)
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

        self.id_cache.update(cpath, id);

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
