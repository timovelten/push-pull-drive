#![allow(unused)]

pub mod config;
mod credential;
mod drive_api;
mod extensions;
pub mod oauth;
use self::drive_api::FileId;
use self::drive_api::FilesReq;
use crate::append_to_cpath_in_place;
use crate::file_repository::AsyncReader;
use crate::file_repository::Error;
use crate::file_repository::Result;
use crate::gdrive::drive_api::MoreFileFields;
use crate::paths::filename;
use crate::paths::subpaths;
use crate::util::hex_must_decode;
use crate::DirectoryInfo;
use crate::{
    append_to_relative_name, file_repository::FileRepository, gdrive::drive_api::DriveApi,
    pop_cpath, util::SemaphoreGuarded, FileInfo, PathInfo,
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
use futures::stream::Next;
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

/// Google does not really like too many concurrent connections. In my personal testing,
/// the limit seemed to be somewhere below 10.
const MAX_CONCURRENT_CONNECTIONS: u32 = 8;

#[derive(Debug)]
pub struct GDriveBackend {
    /// See [`MAX_CONCURRENT_CONNECTIONS`]. To avoid nasty race conditions between concurrent deletions,
    /// make sure to always grab ALL available permits when performing a deletion of a directory.
    sem: Arc<tokio::sync::Semaphore>,

    client: reqwest::Client,

    /// Google drive does not use paths. Instead, it identifies files using ids.
    /// For example, there is no specific API to list all of the files in a directory (by path).
    /// If we know the id of a certain path, we therefore cache it here.
    /// For this, we assume that this object is generally short lived and pretend
    /// like concurrent modification is not an issue (someone could move the folder,
    /// for example. Then the cached ID would point to another path now.).
    id_cache: Mutex<TimedCache<String, Arc<String>>>,

    credential: GoogleCredential,
}

impl FileRepository for Arc<GDriveBackend> {
    type Reader = Pin<Box<dyn AsyncRead + Send>>;

    async fn read_dir(&self, cpath: &str, up_to_depth: u32) -> Result<Vec<PathInfo>> {
        const BATCH_SIZE: usize = 12;

        // Another day, another (sort-of) BFS!
        let mut ret = Vec::new();

        let id = self.get_drive_id(cpath, true).await?.as_ref().to_owned();
        let mut queue = vec![(
            id,
            DirectoryInfo {
                cpath: cpath.to_owned(),
            },
        )];

        for depth in 0..up_to_depth {
            log::debug!("Reading directory {}, depth = {}", cpath, depth);

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
                    let mut ret = Vec::new();
                    let files = self
                        .read_dirs(chunk.iter().map(|(x, _)| x.as_str()), &*self.make_drive_api(false).await?)
                        .await?;

                    for file in files {
                        let parent_id = &file
                            .parents
                            .as_ref()
                            .expect("Google drive files we request have a parent")[0];

                        let parent_cpath = chunk
                            .iter()
                            .find(|(id, _)| id == parent_id)
                            .map(|(_, info)| &info.cpath)
                            .with_context(|| format!("List of targets is {chunk:?} and we recevied a file with parent id {parent_id:?}"))
                            .expect("Every file we receive should have a parent we know of");

                        let cpath = append_to_relative_name(parent_cpath, &file.name);
                        let info = match file.more {
                            Some(MoreFileFields::BinaryFile {
                                sha1_checksum,
                                size,
                                ..
                            }) => PathInfo::File(FileInfo {
                                cpath: cpath.to_owned(),
                                size,
                                modified: file.modified_time,
                                hash: hex_must_decode(sha1_checksum),
                            }),
                            None if file.is_folder() => PathInfo::Dir(DirectoryInfo {
                                cpath: cpath.to_owned(),
                            }),
                            _ => {
                                // Ignore Google docs etc for now..
                                continue;
                            }
                        };

                        ret.push((file.id, info));
                    }

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

    async fn get_metadata(&self, cpath: &str) -> Result<PathInfo> {
        let api = self.make_drive_api(false).await?;

        let drive_id = self.get_drive_id(cpath, false).await?;

        let info = match api.files_get(&drive_id).await {
            Ok(file) => match file.more {
                Some(MoreFileFields::BinaryFile {
                    sha1_checksum,
                    size,
                    ..
                }) => PathInfo::File(FileInfo {
                    size,
                    cpath: cpath.to_owned(),
                    modified: file.modified_time,
                    hash: hex_must_decode(sha1_checksum),
                }),
                None if file.is_folder() => PathInfo::Dir(DirectoryInfo {
                    cpath: cpath.to_owned(),
                }),
                _ => {
                    return Err(Error::NotAllowed {
                        cpath: cpath.to_owned(),
                    })
                }
            },
            Err(err) => return Err(Error::Other(err.into())),
        };

        Ok(info)
    }

    async fn read_file(&self, cpath: &str) -> Result<(FileInfo, Self::Reader)> {
        let cpath = cpath.to_owned();
        log::debug!("Reading file at {cpath}");
        let file = self
            .drive_file_by_cpath(&cpath, &*self.make_drive_api(false).await?)
            .await?;
        log::debug!("Got file at {cpath}");
        match file.more {
            Some(MoreFileFields::BinaryFile {
                head_revision_id,
                sha1_checksum,
                size,
            }) => {
                let info = FileInfo {
                    cpath,
                    size,
                    modified: file.modified_time,
                    hash: hex_must_decode(sha1_checksum),
                };

                let body = self
                    .make_drive_api(false)
                    .await?
                    .files_get_alt_media(&file.id, Some(&head_revision_id))
                    .await
                    .map_err(|err| Error::Other(err.into()))?;

                Ok((info, Box::into_pin(body)))
            }
            None if file.is_folder() => Err(Error::IsADirectory { cpath }),
            _ => Err(Error::NotAllowed { cpath }),
        }
    }

    async fn write_file<R: crate::file_repository::AsyncReader>(
        &self,
        cpath: &str,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> Result<FileInfo> {
        let cpath = cpath.to_owned();
        let info = match self
            .drive_file_by_cpath(&cpath, &*self.make_drive_api(false).await?)
            .await
        {
            Ok(file) => as_fileinfo(file, cpath)?,
            Err(Error::NotFound { .. }) => {
                // No such file exists, create it!
                let file = self.create_file(&cpath, src, len).await?;
                as_fileinfo(file, cpath)?
            }
            Err(err) => return Err(err),
        };

        Ok(info)
    }

    async fn delete_file(&self, cpath: &str) -> Result<()> {
        // To be sure, we require deletions to have exclusive access (I'm not sure if weird things might
        // otherwise happen if we delete a directory and the files in it at the same time or something like that)
        let id = self.get_drive_id(cpath, false).await?;

        // Delete from cache early (what if the deletion happens, but still returns an error for whatever reason. Deleting from
        // cache does not break anything in any case...)
        self.delete_from_cache(cpath);
        self.make_drive_api(true)
            .await?
            .files_delete(&id)
            .await
            .map_err(|err| Error::Other(err.into()))?;

        Ok(())
    }

    async fn set_modified_time(&self, cpath: &str, modtime: time::OffsetDateTime) -> Result<()> {
        let id = self.get_drive_id(cpath, false).await?;

        self.make_drive_api(false)
            .await?
            .files_update(
                &id,
                FilesReq {
                    modified_time: Some(modtime),
                    ..Default::default()
                },
            )
            .await;

        Ok(())
    }
}

impl GDriveBackend {
    pub fn new(credential: GoogleCredential) -> Self {
        let client = reqwest::Client::builder().build().unwrap();

        Self {
            sem: Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS as usize)),
            credential,
            client,
            id_cache: Mutex::new(TimedCache::with_lifespan(60)),
        }
    }

    /// It is important to drop the type returned by this as fast as possible, and not keep it around.
    /// Otherwise, it is very easy to deadlock the program.
    async fn make_drive_api(&self, exclusive: bool) -> Result<SemaphoreGuarded<DriveApi>> {
        let permit = if exclusive {
            self.sem
                .clone()
                .acquire_many_owned(MAX_CONCURRENT_CONNECTIONS)
                .await
                .unwrap()
        } else {
            self.sem.clone().acquire_owned().await.unwrap()
        };
        let token = self
            .credential
            .get_or_refresh_token()
            .await
            .map_err(Error::Other)?;
        let inner = DriveApi::new(self.client.clone(), token);

        Ok(SemaphoreGuarded { permit, inner })
    }

    fn cache_id(&self, cpath: &str, id: &str) {
        log::trace!("Caching drive id: {id} => {cpath}");
        let val = Arc::new(id.to_owned());
        self.id_cache
            .lock()
            .unwrap()
            .cache_set(cpath.to_owned(), val);
    }

    fn get_cached_id(&self, cpath: &str) -> Option<Arc<String>> {
        self.id_cache.lock().unwrap().cache_get(cpath).cloned()
    }

    fn delete_from_cache(&self, cpath: &str) {
        self.id_cache.lock().unwrap().cache_remove(cpath);
    }

    async fn get_drive_id(&self, cpath: &str, force_refresh: bool) -> Result<Arc<String>> {
        if !force_refresh {
            // If we are not forced to refresh, we may return a cached result
            if let Some(ret) = self.get_cached_id(cpath) {
                return Ok(ret);
            }
        }

        // If we get here, we were either forced to refresh or did not find the id.
        let api = self.make_drive_api(false).await?;
        let ret = self.drive_file_by_cpath(cpath, &api).await?.id;
        Ok(Arc::new(ret))
    }

    /// Try and find a drive_file by cpath. The return value of this function is always computed "from scratch",
    /// i.e. we do not use the cache to compute anything here. However, running this function will update the cache
    /// with all the files and folders it discovers along the way.
    ///
    /// # Errors
    /// - If a subpath of of cpath could not be found, this will return `Error::NotFound { cpath: subpath }`
    /// - If a subpath of cpath is not a directory, this will return `Error::NotADirectory { cpath: subpath }`
    async fn drive_file_by_cpath(&self, cpath: &str, api: &DriveApi) -> Result<drive_api::File> {
        // TODO: Replace this by a recursive version that can efficiently use the cache (and e.g. not calculates the root_file every time...)

        // In Google Drive, the root folder has an actual id (e.g. '0AGqh_9UFaLFoUk9PVA') and the "virtual" id
        // 'root' by which it can be accessed almost anywhere. Unfortunately, we need the actual id,
        // since the 'parents' field of a file contains the actual id (and not 'root'). We use this
        // behavior in the read_dir function, that concurrently fetches multiple directory contents
        // in a single request.
        // See also: https://developers.google.com/drive/api/guides/folder
        let root_file = api
            .files_get("root")
            .await
            .map_err(|err| Error::Other(err.into()))?;
        debug_assert_eq!(root_file.parents, None);
        self.cache_id("", &root_file.id);

        if cpath.is_empty() {
            return Ok(root_file);
        }

        let mut current_cpath = "".to_owned();
        let mut current_file = root_file;

        for subcpath in subpaths(cpath).skip(1) {
            let next_file_name = filename(subcpath);

            // At this point, "{current_cpath}/next_file_name" is the file were looking for.
            if !current_file.is_folder() {
                return Err(Error::NotADirectory {
                    cpath: current_cpath,
                });
            }

            let files = self.read_dirs([current_file.id], api).await?;
            log::debug!("Looking for {} in {}", next_file_name, current_cpath);

            let mut next_file = None;

            for file in files {
                // Note that we know at this point that the cpath of file is current_cpath/{file.name}
                let file_cpath = append_to_relative_name(&current_cpath, &file.name);
                self.cache_id(&file_cpath, &file.id);

                if file.name == next_file_name {
                    if next_file.is_some() {
                        log::warn!(
                            "There are multiple items with name {} in {}",
                            file.name,
                            current_cpath
                        );
                    } else {
                        next_file = Some(file);
                    }
                }
            }

            let mut next_cpath = current_cpath;
            append_to_cpath_in_place(&mut next_cpath, next_file_name);

            if let Some(next_file) = next_file {
                current_cpath = next_cpath;
                current_file = next_file;
            } else {
                return Err(Error::NotFound { cpath: next_cpath });
            }

            // Can we talk about how cool it is that rust is able to see that we move the value "current_cpath" into new_cpath,
            // and then move it back into current_cpath so that current_cpath is available at the start of the loop?!
        }

        Ok(current_file)
    }

    /// Creates the given relative name and returns its id. If this path
    /// already exists and is the path of a file (and not a folder) this returns
    /// `Error::NotAFolder`.
    async fn mkdir_all(&self, cpath: &str) -> Result<String> {
        log::debug!("creating folder(s) {}", cpath);

        /// It is important that we take an exclusive lock here! We particularly want to avoid
        /// any other mkdir_all running at the same time as us (anything else would more or less probably work...)
        let api = self.make_drive_api(true).await?;
        loop {
            /// Try and build up the thing!
            match self.drive_file_by_cpath(cpath, &api).await {
                Ok(file) => {
                    if file.is_folder() {
                        return Ok(file.id);
                    } else {
                        return Err(Error::NotADirectory {
                            cpath: cpath.to_owned(),
                        });
                    }
                }
                Err(Error::NotFound { cpath: subpath }) => {
                    log::debug!("mkdir_all encountered a NotFound for {}", subpath);

                    let (parent, name) = pop_cpath(&subpath);
                    let parent_id = self.get_drive_id(parent, false).await?;

                    // subpath was not found, so create it!
                    let req =
                        FilesReq::new_create_folder(name.to_owned(), parent_id.as_str().to_owned());
                    let file_id = api
                        .files_create(req)
                        .await
                        .map_err(|err| Error::Other(err.into()))?;
                    log::debug!("Created folder {} with id {}", subpath, file_id.id);

                    self.cache_id(&subpath, &file_id.id);

                    // We could loop once more here and let the other match arm end the loop too, but that would only cost us
                    // one more api call...
                    if subpath == cpath {
                        return Ok(file_id.id);
                    }
                }
                // Other errors (i.e. if a subpath is a file or something like that) we cannot deal with
                Err(err) => return Err(err),
            }
        }
    }

    /// This is a helper method. It takes a list of targets, which are google drive ids of folders (or not,
    /// this will not error if they are not). It then returns a list of google drive files. Note that these have a parent id
    /// which can subsequently be used to determine to which of the targets the file belongs.
    /// Also note that "file" in this context can also be a folder.
    async fn read_dirs<S: AsRef<str>>(
        &self,
        targets: impl IntoIterator<Item = S>,
        api: &DriveApi,
    ) -> Result<Vec<drive_api::File>> {
        let mut ret = Vec::new();

        // Build the google drive query string that looks for children of those ids.
        let query = {
            // Query string is of the form
            // trashed = false and ('abc' in parents or 'cdf' in parents)
            let mut q = String::from("trashed = false and (");
            for (i, id) in targets.into_iter().enumerate() {
                if i > 0 {
                    write!(q, " or ").unwrap();
                }
                write!(q, "'{}' in parents", id.as_ref()).unwrap();
            }
            // Don't forget the closing brace
            write!(q, ")").unwrap();

            q
        };

        log::debug!("Requesting files with query {query}");

        let files = api
            .file_list_all(&query)
            .await
            .map_err(|err| Error::Other(err.into()))?;

        return Ok(files);

        Ok(ret)
    }

    async fn create_file<R: AsyncRead + Send>(
        &self,
        cpath: &str,
        src: R,
        len: u64,
    ) -> Result<drive_api::File> {
        // We need to create the file. For that, we need to place it in the right path
        let (parent, filename) = pop_cpath(cpath);
        let parent_id = self.mkdir_all(parent).await?;

        let file = self
            .make_drive_api(false)
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
            .await
            .map_err(|err| Error::Other(err.into()))?;

        self.cache_id(cpath, &file.id);

        Ok(file)
    }
}

fn as_pathinfo(file: drive_api::File, cpath: String) -> (Option<PathInfo>, String) {
    let info = match file.more {
        Some(MoreFileFields::BinaryFile {
            sha1_checksum,
            size,
            ..
        }) => PathInfo::File(FileInfo {
            cpath: cpath.to_owned(),
            size,
            modified: file.modified_time,
            hash: hex_must_decode(sha1_checksum),
        }),
        _ if file.is_folder() => PathInfo::Dir(DirectoryInfo {
            cpath: cpath.to_owned(),
        }),
        _ => return (None, file.id),
    };

    (Some(info), file.id)
}

/// This is a somewhat opinated function that turns a google drive file into a FileInfo. Notably, it returns `Error::IsADirectory`
/// if the file is a directory, and `Error::NotAllowed`, if it is neither a binary file nor a directory (i.e. google docs document or something
/// like that)
fn as_fileinfo(file: drive_api::File, cpath: String) -> Result<FileInfo> {
    match as_pathinfo(file, cpath.to_owned()).0 {
        Some(PathInfo::File(info)) => Ok(info),
        Some(PathInfo::Dir(DirectoryInfo { cpath })) => return Err(Error::IsADirectory { cpath }),
        None => {
            return Err(Error::NotAllowed {
                cpath: cpath.to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod test {
    use oauth2::{ClientId, ClientSecret, RefreshToken};

    use super::*;
    use test_log::test;

    // These are intended to be run with
    // cargo test gdrive::test --features=gdrive-tests -- --nocapture --test-threads=1

    fn setup() -> Arc<GDriveBackend> {
        let client_id = ClientId::new(std::env::var("GOOGLE_CID").unwrap());
        let refresh_token = RefreshToken::new(std::env::var("GOOGLE_RT").unwrap());
        let client_secret = ClientSecret::new(std::env::var("GOOGLE_SECRET").unwrap());

        let client = oauth::new_google_oauth_client(client_id, client_secret);
        let credential = GoogleCredential::new(client, refresh_token, None, |_| {
            println!("Recevied new token");
        });

        Arc::new(GDriveBackend::new(credential))
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "gdrive-tests"), ignore)]
    async fn list_files() {
        let backend = setup();
        let mut res = backend.read_dir("", 2).await.unwrap();
        res.sort_by_key(|info| info.cpath().to_owned());

        for info in res {
            println!("{}", info.cpath());
        }
    }

    #[test(tokio::test)]
    #[cfg_attr(not(feature = "gdrive-tests"), ignore)]
    async fn create_file() {
        let backend = setup();
        let content = "Hallo".as_bytes().to_owned();
        let len = content.len() as u64;
        let value = backend
            .create_file(
                "testdir/anotherdir/andanother/testfile",
                futures::io::Cursor::new(content),
                len,
            )
            .await
            .unwrap();
    }
}
