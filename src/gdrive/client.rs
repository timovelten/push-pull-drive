use std::sync::{Arc, Mutex};

use anyhow::Context;
use cached::TimedCache;
use drive_api::{DriveApi, File, FilesReq, GoogleCredential, MoreFileFields};
use futures::{stream::FuturesUnordered, AsyncRead, StreamExt as _};

use crate::{
    file_repository::{Error, Result},
    hex_must_decode,
    paths::{RPath, RPathBuf},
    DirectoryInfo, FileInfo, PathInfo,
};
use cached::Cached;
use std::fmt::Write;

fn to_pathinfo(file: drive_api::File, path: RPathBuf) -> (Option<PathInfo>, String) {
    let info = match file.more {
        Some(MoreFileFields::BinaryFile {
            sha1_checksum,
            size,
            ..
        }) => PathInfo::File(FileInfo {
            path,
            size,
            modified: file.modified_time
        }),
        _ if file.is_folder() => PathInfo::Dir(DirectoryInfo { path }),
        _ => return (None, file.id),
    };

    (Some(info), file.id)
}

/// This is a somewhat opinated function that turns a google drive file into a FileInfo. Notably, it returns `Error::IsADirectory`
/// if the file is a directory, and `Error::NotAllowed`, if it is neither a binary file nor a directory (i.e. google docs document or something
/// like that)
fn to_fileinfo(file: drive_api::File, path: RPathBuf) -> Result<FileInfo> {
    match to_pathinfo(file, path.clone()).0 {
        Some(PathInfo::File(info)) => Ok(info),
        Some(PathInfo::Dir(DirectoryInfo { path: rpath })) => Err(Error::IsADirectory { rpath }),
        None => Err(Error::NotAllowed { rpath: path }),
    }
}

#[derive(Debug)]
/// Conceptually this is a singleton. There should be just one instance of this, wrapped in an Arc.
pub(super) struct GDriveClient {
    /// Calling mkdir_all concurrently really does not work, so we need to prevent it.
    mkdir_lock: tokio::sync::Mutex<()>,
    api_client: DriveApi,

    /// Google drive does not use paths. Instead, it identifies files using ids.
    /// For example, there is no specific API to list all of the files in a directory (by path).
    /// If we know the id of a certain path, we therefore cache it here.
    /// For this, we assume that this object is generally short lived and pretend
    /// like concurrent modification is not an issue (someone could move the folder,
    /// for example. Then the cached ID would point to another path now.).
    file_cache: Mutex<TimedCache<RPathBuf, Arc<drive_api::File>>>,
}

impl GDriveClient {
    pub fn new(credential: GoogleCredential) -> Self {
        let api_client = DriveApi::new(Default::default(), credential);

        Self {
            mkdir_lock: tokio::sync::Mutex::new(()),
            api_client,
            file_cache: Mutex::new(TimedCache::with_lifespan(15)),
        }
    }

    fn cache_file(&self, path: &RPath, file: drive_api::File) -> Arc<drive_api::File> {
        log::trace!("Caching drive file: {} => {path}", file.id);
        let val = Arc::new(file);
        let ret = val.clone();
        self.file_cache
            .lock()
            .unwrap()
            .cache_set(path.to_owned(), val);

        ret
    }

    fn get_cached_file(&self, path: &RPath) -> Option<Arc<drive_api::File>> {
        self.file_cache.lock().unwrap().cache_get(path).cloned()
    }

    fn delete_from_cache(&self, path: &RPath) {
        self.file_cache.lock().unwrap().cache_remove(path);
    }

    pub async fn drive_file_by_rpath_cached(&self, path: &RPath) -> Result<Arc<drive_api::File>> {
        if let Some(ret) = self.get_cached_file(path) {
            Ok(ret)
        } else {
            let ret = self.drive_file_by_rpath(path).await?;
            Ok(Arc::new(ret))
        }
    }

    /// Try and find a drive_file by rpath. The return value of this function is always computed "from scratch",
    /// i.e. we do not use the cache to compute anything here. However, running this function will update the cache
    /// with all the files and folders it discovers along the way.
    ///
    /// # Errors
    /// - If a subpath of of rpath could not be found, this will return `Error::NotFound { rpath: subpath }`
    /// - If a subpath of rpath is not a directory, this will return `Error::NotADirectory { rpath: subpath }`
    pub async fn drive_file_by_rpath(&self, path: &RPath) -> Result<drive_api::File> {
        // In Google Drive, the root folder has an actual id (e.g. '0AGqh_9UFaLFoUk9PVA') and the "virtual" id
        // 'root' by which it can be accessed almost anywhere. Unfortunately, we need the actual id,
        // since the 'parents' field of a file contains the actual id (and not 'root'). We use this
        // behavior in the read_dir function, that concurrently fetches multiple directory contents
        // in a single request.
        // See also: https://developers.google.com/drive/api/guides/folder
        let root_file = self
            .api_client
            .files_get("root")
            .await
            .map_err(|err| Error::Other(err.into()))?;
        debug_assert_eq!(root_file.parents, None);
        self.cache_file(RPath::empty(), root_file.clone());

        if path.is_empty() {
            return Ok(root_file);
        }

        let mut current_path = RPathBuf::empty();
        let mut current_file = root_file;

        for subrpath in path.subpaths().skip(1) {
            let next_file_name = subrpath.filename().unwrap();

            // At this point, "{current_rpath}/next_file_name" is the file were looking for.
            if !current_file.is_folder() {
                return Err(Error::NotADirectory {
                    rpath: current_path,
                });
            }

            let files = self.read_dirs([current_file.id]).await?;
            let mut next_file = None;

            for file in files {
                // Note that we know at this point that the rpath of file is current_rpath/{file.name}
                let file_rpath = current_path.join_single(&file.name);
                self.cache_file(&file_rpath, file.clone());

                if file.name == next_file_name {
                    if next_file.is_some() {
                        log::warn!(
                            "There are multiple items with name {} in {}",
                            file.name,
                            current_path
                        );
                    } else {
                        next_file = Some(file);
                    }
                }
            }

            let mut next_rpath = current_path;
            next_rpath.push(next_file_name);

            if let Some(next_file) = next_file {
                current_path = next_rpath;
                current_file = next_file;
            } else {
                return Err(Error::NotFound { rpath: next_rpath });
            }
        }

        Ok(current_file)
    }

    /// Creates the given relative name and returns its id. If this path
    /// already exists and is the path of a file (and not a folder) this returns
    /// `Error::NotAFolder`.
    ///
    /// It is important that mkdir_all is NOT called concurrently with itself. For this reason,
    /// the caller should aquire all permits (`self.api_exclusive()`) to call this function.
    pub async fn mkdir_all(&self, rpath: &RPath) -> Result<Arc<drive_api::File>> {
        log::debug!("creating folder(s) {}", rpath);
        let _guard = self.mkdir_lock.lock().await;

        /// It is important that we take an exclusive lock here! We particularly want to avoid
        /// any other mkdir_all running at the same time as us (anything else would more or less probably work...)
        loop {
            /// Try and build up the thing!
            match self.drive_file_by_rpath_cached(rpath).await {
                Ok(file) => {
                    if file.is_folder() {
                        return Ok(file);
                    } else {
                        return Err(Error::NotADirectory {
                            rpath: rpath.to_owned(),
                        });
                    }
                }
                Err(Error::NotFound { rpath: subpath }) => {
                    // We rely on the correct error here. It should return the "smallest" possible subpath that was not found
                    // (i.e. after chopping off the last bit from that path, the remaining path should be valid)
                    log::debug!("mkdir_all encountered a NotFound for {}", subpath);

                    // This can only fail when subpath is empty. And the empty path should always be valid
                    let (parent, name) = subpath.split_right().expect("subpath is not empty");
                    let parent_id = &self.drive_file_by_rpath_cached(parent).await?.id;

                    // subpath was not found, so create it!
                    let req =
                        FilesReq::new_create_folder(name.to_owned(), parent_id.as_str().to_owned());
                    let created_file = self
                        .api_client
                        .files_create(req)
                        .await
                        .map_err(|err| Error::Other(err.into()))?;
                    log::debug!("Created folder {} with id {}", subpath, created_file.id);

                    self.cache_file(&subpath, created_file.clone());

                    // We could loop once more here and let the other match arm end the loop too, but that would only cost us
                    // one more api call...
                    if &*subpath == rpath {
                        return Ok(Arc::new(created_file));
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
    pub async fn read_dirs<S: AsRef<str>>(
        &self,
        targets: impl IntoIterator<Item = S>,
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

        let files = self
            .api_client
            .file_list_all(&query)
            .await
            .map_err(|err| Error::Other(err.into()))?;

        return Ok(files);

        Ok(ret)
    }

    pub async fn create_file<R: AsyncRead + Send>(
        &self,
        path: &RPath,
        src: R,
        len: u64,
    ) -> Result<Arc<drive_api::File>> {
        // We need to create the file. For that, we need to place it in the right path
        let (parent, filename) = path.split_right().ok_or_else(|| Error::IsADirectory {
            rpath: path.to_owned(),
        })?;
        
        let parent_file = self.mkdir_all(parent).await?;
        let file = self
            .api_client
            .files_create_upload(
                FilesReq {
                    name: Some(filename.to_string()),
                    parents: Some(vec![parent_file.id.clone()]),
                    ..Default::default()
                },
                src,
                len,
            )
            .await
            .map_err(|err| Error::Other(err.into()))?;

        let ret = self.cache_file(path, file);
        Ok(ret)
    }

    async fn update_file<R: AsyncRead + Send>(&self, file: File, src: R, len: u64) -> Result<File> {
        self.api_client
            .files_update_upload(&file.id, src, len)
            .await
            .map_err(|err| Error::Other(err.into()))
    }

    pub async fn get_metadata(&self, cpath: &RPath) -> Result<PathInfo> {
        let drive_id = &self.drive_file_by_rpath_cached(cpath).await?.id;
        let info = match self.api_client.files_get(drive_id).await {
            Ok(file) => match file.more {
                Some(MoreFileFields::BinaryFile {
                    sha1_checksum,
                    size,
                    ..
                }) => PathInfo::File(FileInfo {
                    size,
                    path: cpath.to_owned(),
                    modified: file.modified_time
                }),
                None if file.is_folder() => PathInfo::Dir(DirectoryInfo {
                    path: cpath.to_owned(),
                }),
                _ => {
                    return Err(Error::NotAllowed {
                        rpath: cpath.to_owned(),
                    })
                }
            },
            Err(err) => return Err(Error::Other(err.into())),
        };

        Ok(info)
    }

    pub async fn read_file(&self, path: &RPath) -> Result<(FileInfo, Box<dyn AsyncRead + Send>)> {
        let cpath = path.to_owned();
        log::info!("Reading file at {cpath}");
        let cache_size = self.file_cache.lock().unwrap().cache_size();
        log::info!("Cache size: {}", cache_size);
        let file = self.drive_file_by_rpath(&cpath).await?;
        match file.more {
            Some(MoreFileFields::BinaryFile {
                head_revision_id,
                sha1_checksum,
                size,
            }) => {
                let info = FileInfo {
                    path: cpath,
                    size,
                    modified: file.modified_time
                };

                let body = self
                    .api_client
                    .files_get_alt_media(&file.id, Some(&head_revision_id))
                    .await
                    .map_err(|err| Error::Other(err.into()))?;

                Ok((info, body))
            }
            None if file.is_folder() => Err(Error::IsADirectory { rpath: cpath }),
            _ => Err(Error::NotAllowed { rpath: cpath }),
        }
    }

    pub async fn write_file<R: crate::file_repository::AsyncReader>(
        &self,
        path: &RPath,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> Result<FileInfo> {
        let cpath = path.to_owned();
        let info = match self.drive_file_by_rpath(&cpath).await {
            Ok(file) => {
                // The file already exists, update it.
                let file = self.update_file(file, src, len).await?;
                to_fileinfo(file, cpath)?
            }
            Err(Error::NotFound { .. }) => {
                // No such file exists, create it!
                log::debug!(
                    "Tried to write to drive path {cpath} but it does not exist. Creating it."
                );

                let file = self.create_file(&cpath, src, len).await?.as_ref().clone();

                to_fileinfo(file, cpath)?
            }
            Err(err) => return Err(err),
        };

        Ok(info)
    }

    pub async fn delete_file(&self, path: &RPath) -> Result<()> {
        // To be sure, we require deletions to have exclusive access (I'm not sure if weird things might
        // otherwise happen if we delete a directory and the files in it at the same time or something like that)
        let id = &self.drive_file_by_rpath_cached(path).await?.id;

        // Delete from cache early (what if the deletion happens, but still returns an error for whatever reason. Deleting from
        // cache does not break anything in any case...)
        self.delete_from_cache(path);
        self.api_client
            .files_delete(id)
            .await
            .map_err(|err| Error::Other(err.into()))?;

        Ok(())
    }

    pub async fn set_modified_time(
        &self,
        path: &RPath,
        modtime: time::OffsetDateTime,
    ) -> Result<()> {
        let id = &self.drive_file_by_rpath_cached(path).await?.id;

        self.api_client
            .files_update(
                id,
                FilesReq {
                    modified_time: Some(modtime),
                    ..Default::default()
                },
            )
            .await;

        Ok(())
    }

    pub async fn list_files(&self, path: &RPath, up_to_depth: u32) -> Result<Vec<PathInfo>> {
        const BATCH_SIZE: usize = 12;

        // Another day, another (sort-of) BFS!
        let mut ret = Vec::new();

        // TODO: This needs to support the case where path is the path to a file
        let id = self.drive_file_by_rpath_cached(path).await?.id.to_owned();
        let mut queue = vec![(
            id,
            DirectoryInfo {
                path: path.to_owned(),
            },
        )];

        for depth in 0..up_to_depth {
            log::debug!("Reading directory {}, depth = {}", path, depth);

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
                        .read_dirs(chunk.iter().map(|(x, _)| x.as_str()))
                        .await?;

                    for file in files {
                        let parent_id = &file
                            .parents
                            .as_ref()
                            .expect("Google drive files we request have a parent")[0];

                        let parent_cpath = chunk
                            .iter()
                            .find(|(id, _)| id == parent_id)
                            .map(|(_, info)| &info.path)
                            .with_context(|| format!("List of targets is {chunk:?} and we recevied a file with parent id {parent_id:?}"))
                            .expect("Every file we receive should have a parent we know of");

                        let cpath = parent_cpath.join_single(&file.name);

                        let info = match file.more {
                            Some(MoreFileFields::BinaryFile {
                                sha1_checksum,
                                size,
                                ..
                            }) => PathInfo::File(FileInfo {
                                path: cpath.to_owned(),
                                size,
                                modified: file.modified_time
                            }),
                            None if file.is_folder() => PathInfo::Dir(DirectoryInfo {
                                path: cpath.to_owned(),
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
}
