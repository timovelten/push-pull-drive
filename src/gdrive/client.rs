use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context};
use cached::TimedCache;
use drive_api::{DriveApi, File, FilesReq, GoogleCredential, MoreFileFields};
use futures::{stream::FuturesUnordered, AsyncRead, StreamExt as _, TryFutureExt};
use tokio::task::JoinSet;

use crate::{
    file_repository::{self, Error, Result}, filetree::FileTree, paths::{RPath, RPathBuf}, FileInfo, PathInfo
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
        _ if file.is_folder() => PathInfo::Dir(path),
        _ => return (None, file.id),
    };

    (Some(info), file.id)
}

/// This is a somewhat opinated function that turns a google drive file into a FileInfo. Notably, it returns `Error::IsADirectory`
/// if the file is a directory, and `Error::NotAllowed`, if it is neither a binary file nor a directory (i.e. google docs document or something
/// like that)
fn to_fileinfo(file: drive_api::File, path: RPathBuf) -> Result<FileInfo> {
    let err = match to_pathinfo(file, path.clone()).0 {
        Some(PathInfo::File(info)) => return Ok(info),
        Some(PathInfo::Dir(_)) => anyhow!(Error::IsADirectory),
        None => anyhow!(Error::NotAllowed),
    };

    Err(err.context(path))
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
    /// Google does not really like too many concurrent connections. In my personal testing, the limit seemed to be somewhere below 10.
    pub const CONCURRENCY_LIMIT: usize = 8;

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

    async fn root_file(&self) -> Result<drive_api::File> {
        let root_file = self
            .api_client
            .files_get("root")
            .await?;

        Ok(root_file)
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
        let root_file = self.root_file().await?;

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
                return Err(anyhow!(Error::NotADirectory).context(current_path));
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
                return Err(anyhow!(Error::NotFound).context(next_rpath));
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
    pub async fn mkdir(&self, path: &RPath) -> Result<()> {
        let Some((parent, name)) = path.split_right() else {
            bail!(Error::Exists)
        };

        log::debug!("mkdir {:?}", path);

        let parent = self.drive_file_by_rpath_cached(parent).await?;
        let req = FilesReq::new_create_folder(name.to_owned(), parent.id.to_owned());

        let created_file = self.api_client
            .files_create(req)
            .await?;

        self.cache_file(path, created_file.clone());

        Ok(())
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
            .await?;

        return Ok(files);

        Ok(ret)
    }

    pub async fn get_metadata(&self, path: &RPath) -> Result<PathInfo> {
        let drive_id = &self.drive_file_by_rpath_cached(path).await?.id;
        let info = match self.api_client.files_get(drive_id).await {
            Ok(file) => match file.more {
                Some(MoreFileFields::BinaryFile {
                    sha1_checksum,
                    size,
                    ..
                }) => PathInfo::File(FileInfo {
                    size,
                    path: path.to_owned(),
                    modified: file.modified_time
                }),
                None if file.is_folder() => PathInfo::Dir(path.to_owned()),
                _ => {
                    bail!(Error::NotAllowed);
                }
            },
            Err(err) => bail!(err),
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
                    .await?;

                Ok((info, body))
            }
            None if file.is_folder() => Err(anyhow!(Error::IsADirectory)),
            _ => Err(anyhow!(Error::NotAllowed)),
        }
    }

    async fn create_file<R: AsyncRead + Send>(
        &self,
        path: &RPath,
        src: R,
        len: u64,
    ) -> Result<drive_api::File> {
        // We need to create the file. For that, we need to place it in the right path
        let (parent, filename) = path.split_right().ok_or(Error::IsADirectory)?;

        let parent = self.drive_file_by_rpath_cached(parent).await?;
        if !parent.is_folder() {
            bail!(Error::NotADirectory);
        }
        
        let file = self
            .api_client
            .files_create_upload(
                FilesReq {
                    name: Some(filename.to_string()),
                    parents: Some(vec![parent.id.clone()]),
                    ..Default::default()
                },
                src
            )
            .await?;

        self.cache_file(path, file.clone());

        Ok(file)
    }

    pub async fn write_file<R: crate::file_repository::AsyncReader>(
        &self,
        path: &RPath,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> Result<FileInfo> {
        let path = path.to_owned();
        let res = self.drive_file_by_rpath(&path).await.map_err(|err| err.downcast::<file_repository::Error>());

        let file = match res {
            Ok(file) => {
                log::debug!("Updating file at {}", path);

                self.api_client
                    .files_update_upload(&file.id, src)
                    .await?
            }
            Err(Ok(err)) => {
                // We were able to determine that this is a not found, so create the file
                log::debug!("Creating file at {}", path);

                self.create_file(&path, src, len).await?
            }
            Err(Err(err)) => return Err(err)
        };

        to_fileinfo(file, path)
    }

    pub async fn delete(&self, path: &RPath) -> Result<()> {
        let id = &self.drive_file_by_rpath_cached(path).await?.id;

        // Delete from cache early (what if the deletion happens, but still returns an error for whatever reason. Deleting from
        // cache does not break anything in any case...)
        self.delete_from_cache(path);
        self.api_client
            .files_delete(id)
            .await?;

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

    pub async fn file_tree(self: &Arc<Self>, filter: &RPath, max_depth: usize) -> Result<FileTree<PathInfo>> {
        const BATCH_SIZE: usize = 12;
        log::debug!("file_tree with filter '{filter:?}'");

        // Another day, another (sort-of) BFS!
        let mut ret = vec![
            PathInfo::root_dir()
        ];

        let root = self.root_file().await?;
        let mut queue = vec![(
            root.id, RPathBuf::empty()
        )];

        for depth in 0..max_depth {
            // Invariant: At this point, all directories in the queue are directories of depth
            // `depth` and depth <= max_depth
            if queue.is_empty() {
                break;
            }

            // Limit concurrency somewhat
            let sem = Arc::new(tokio::sync::Semaphore::new(Self::CONCURRENCY_LIMIT));
            let mut fts = JoinSet::new();
            
            // Only consider those queue items that match the current filter
            let mut queue_items = queue.into_iter();

            loop {
                let batch: Vec<_> = (&mut queue_items).take(BATCH_SIZE).collect();
                if batch.is_empty() {
                    break
                }

                let this = self.clone();
                let sem = sem.clone();
                fts.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();

                    let mut ret = Vec::new();
                    let files = this
                        .read_dirs(batch.iter().map(|(x, _)| x.as_str()))
                        .await?; 

                    for file in files {
                        let parent_id = &file
                            .parents
                            .as_ref()
                            .expect("Google drive files we request have a parent")[0];

                        let parent_path = batch
                            .iter()
                            .find(|(id, _)| id == parent_id)
                            .map(|(_, path)| path)
                            .with_context(|| format!("List of targets is {batch:?} and we recevied a file with parent id {parent_id:?}"))
                            .expect("Every file we receive should have a parent we know of");

                        let path = parent_path.join_single(&file.name);

                        let info = match file.more {
                            Some(MoreFileFields::BinaryFile {
                                sha1_checksum,
                                size,
                                ..
                            }) => PathInfo::File(FileInfo {
                                path: path.to_owned(),
                                size,
                                modified: file.modified_time
                            }),
                            None if file.is_folder() => PathInfo::Dir(path.to_owned()),
                            _ => {
                                // Ignore Google docs etc for now..
                                continue;
                            }
                        };

                        ret.push((file.id, info));
                    }

                    Ok::<_, anyhow::Error>(ret)
                });
            }

            // Extract folders that we found and queue them up
            queue = Vec::new();
            while let Some(next) = fts.join_next().await {
                let next = next.unwrap();

                for (next_id, next_info) in next? {
                    // Filter out paths that are not requested
                    if !next_info.path().is_related_to(filter) {
                        continue;
                    }

                    if let PathInfo::Dir(path) = &next_info {
                        queue.push((next_id, path.clone()));
                    }

                    ret.push(next_info);
                }
            }
        }

        Ok(ret.try_into().unwrap())
    }
}
