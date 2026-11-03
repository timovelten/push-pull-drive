use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use async_recursion::async_recursion;
use drive_api::{DriveApi, File, FilesReq, GoogleCredential, MoreFileFields};
use futures::{stream::FuturesUnordered, AsyncRead, StreamExt as _, TryFutureExt};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::iso8601::Config;
use tokio::task::JoinSet;

use crate::{
    file_repository::{self, Error},
    filetree::FileTree,
    paths::{RPath, RPathBuf},
    FileInfo, PathInfo,
};
use cached::{Cached, ConcurrentCacheTtl, ExpiringCache, ShardedTtlCache, TtlCache};
use std::fmt::Write;

use super::service::{DriveService, DriveServiceImpl};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    pub use_trash: bool,
    pub cache_time: Duration,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            use_trash: true,
            cache_time: Duration::from_secs(45),
        }
    }
}

#[derive(Debug)]
/// The clients primary job is to map between the path-based model of a filesystem we work with
/// to the id-based system that Google Drive uses.
///
/// Conceptually this is a singleton. There should be just one instance of this, wrapped in an Arc.
pub(super) struct GDriveClient<S: DriveService = DriveServiceImpl> {
    service: S,
    path_cache: Mutex<TtlCache<RPathBuf, String>>,
    preferences: Preferences,
}

/// Google does not really like too many concurrent connections. In my personal testing, the limit seemed to be somewhere below 10.
pub const CONCURRENCY_LIMIT: usize = 8;

impl GDriveClient {
    pub fn with_default_service_from_config(cred: GoogleCredential, preferences: Preferences) -> Self {
        Self::from_config(DriveServiceImpl::new(cred), preferences)
    }
}

fn to_pathinfo(file: &drive_api::File, path: RPathBuf) -> Option<PathInfo> {
    match &file.more {
        Some(MoreFileFields::BinaryFile { size, .. }) => {
            let info = FileInfo {
                path: path.to_owned(),
                size: *size,
                modified: file.modified_time,
            };

            Some(PathInfo::File(info))
        }
        None if file.is_folder() => Some(PathInfo::Dir(path.to_owned())),
        _ => None,
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum Resolve {
    Directory,
    File,
    Any,
}

impl<Service: DriveService> GDriveClient<Service> {
    pub fn from_config(service: Service, preferences: Preferences) -> Self {
        let path_cache = TtlCache::new(preferences.cache_time);
        
        Self {
            service,
            path_cache: Mutex::new(path_cache),
            preferences,
        }
    }

    async fn resolve(&self, path: &RPath, resolve: Resolve) -> anyhow::Result<String> {
        let id = self
            .maybe_resolve(path, resolve)
            .await?
            .ok_or(Error::NotFound)
            .with_context(|| format!("Failed to resolve path {path:?} to google drive id"))?;

        Ok(id)
    }

    async fn maybe_resolve(
        &self,
        path: &RPath,
        resolve: Resolve,
    ) -> anyhow::Result<Option<String>> {
        self.raw_resolve(path, resolve).await
    }

    #[async_recursion]
    async fn raw_resolve(&self, path: &RPath, resolve: Resolve) -> anyhow::Result<Option<String>> {
        // We could probaly rewrite this iteratively and avoid async_recursion..
        if let Some(id) = self.path_cache.lock().unwrap().cache_get(path) {
            return Ok(Some(id.to_owned()));
        }

        let Some((name, base)) = path.split_right() else {
            return Ok(Some(self.root_id().await?));
        };

        // For the parent of anything, we only care about directories..
        let parent_id = match self.raw_resolve(base, Resolve::Directory).await? {
            Some(id) => id,
            None => return Ok(None),
        };

        let contents = self.service.read_dir(&parent_id).await?;
        let canon_contents = canonicalize(contents);

        let mut paths = self.path_cache.lock().unwrap();

        let mut ret = Ok(None);
        for file in canon_contents {
            let file_path = base.join_single(&file.name);
            let file_id = file.file.id.to_owned();

            paths.cache_set(file_path, file_id);

            if file.name == name {
                // We will only ever enter this if once (canonical contents do not contain duplicate names)
                ret = match (resolve, file.file.is_folder()) {
                    (Resolve::Directory, true) | (Resolve::File, false) | (Resolve::Any, _) => {
                        Ok(Some(file.file.id))
                    }
                    (Resolve::Directory, false) => bail!(Error::NotADirectory),
                    (Resolve::File, true) => bail!(Error::IsADirectory),
                };

                // Don't return early here so that we can remember all those paths we see here
            }
        }

        ret
    }

    async fn root_id(&self) -> anyhow::Result<String> {
        Ok(self.service.get_file("root").await?.id)
    }

    pub async fn mkdir(&self, path: &RPath) -> anyhow::Result<()> {
        let Some((name, parent)) = path.split_right() else {
            bail!(Error::Exists)
        };

        let folder_id = self.resolve(parent, Resolve::Directory).await?;

        self.service.mkdir(&folder_id, name).await?;

        Ok(())
    }

    pub async fn metadata(&self, path: &RPath) -> anyhow::Result<PathInfo> {
        let file_id = self.resolve(path, Resolve::Any).await?;
        let file = self.service.get_file(&file_id).await?;
        let ret = to_pathinfo(&file, path.to_owned()).ok_or(Error::NotAllowed)?;

        Ok(ret)
    }

    pub async fn read_file(
        &self,
        path: &RPath,
    ) -> anyhow::Result<(FileInfo, Box<dyn AsyncRead + Send>)> {
        let file_id = self.resolve(path, Resolve::File).await?;

        let file = self.service.get_file(&file_id).await?;
        let info = to_pathinfo(&file, path.to_owned()).ok_or(Error::NotAllowed)?;
        match info {
            PathInfo::Dir(_) => Err(anyhow!(Error::IsADirectory)),
            PathInfo::File(info) => {
                let Some(MoreFileFields::BinaryFile {
                    head_revision_id, ..
                }) = file.more
                else {
                    unreachable!();
                };

                let body = self
                    .service
                    .get_body(&file.id, Some(&head_revision_id))
                    .await?;

                Ok((info, body))
            }
        }
    }

    pub async fn create_file<R: AsyncRead + Send>(
        &self,
        path: &RPath,
        src: R,
        _len: u64,
    ) -> anyhow::Result<FileInfo> {
        if self.maybe_resolve(path, Resolve::Any).await?.is_some() {
            bail!(Error::Exists);
        }

        let (filename, parent) = path.split_right().ok_or(Error::IsADirectory)?;
        let parent_id = self.resolve(parent, Resolve::Directory).await?;

        let info = self.service.create_file(&parent_id, filename, src).await?;
        let file_info = to_pathinfo(&info, path.to_owned())
            .and_then(|f| f.into_fileinfo())
            .ok_or_else(|| anyhow!("Expected a valid file response"))?;

        Ok(file_info)
    }

    pub async fn update_file<R: crate::file_repository::AsyncReader>(
        &self,
        path: &RPath,
        src: R,
        _len: u64,
    ) -> anyhow::Result<FileInfo> {
        let id = self.resolve(path, Resolve::File).await?;

        let info = self.service.update_body(&id, src).await?;
        let file_info = to_pathinfo(&info, path.to_owned())
            .and_then(|f| f.into_fileinfo())
            .ok_or_else(|| anyhow!("Expected a valid file response"))?;

        Ok(file_info)
    }

    pub async fn set_modified_time(
        &self,
        path: &RPath,
        time: time::OffsetDateTime,
    ) -> anyhow::Result<()> {
        let id = self.resolve(path, Resolve::File).await?;
        self.service.set_modified_time(&id, time).await?;
        Ok(())
    }

    pub async fn delete(&self, path: &RPath) -> anyhow::Result<()> {
        let id = self.resolve(path, Resolve::Any).await?;
        self.service.delete(&id, self.preferences.use_trash).await?;

        // There is simply no way to cleanly track deletions. Just invalidate the whole cache.
        // Delete should happen at the start anyway.
        self.path_cache.lock().unwrap().cache_clear();

        Ok(())
    }

    pub async fn list_files(
        self: &Arc<Self>,
        path: &RPath,
        max_depth: usize,
    ) -> anyhow::Result<Vec<PathInfo>> {
        // This is actually by far the most ux critical function. Listing the files is what takes the longest on
        // google drive due to the sheer amount of rest requests it uses. Paralellizing these
        // and minimizing the number of calls is very important.
        let id = self.resolve(path, Resolve::Directory).await?;

        // All directories of a certain depth. We want to deepen the search tree by expanding these
        let mut queue = vec![(path.to_owned(), id)];

        let mut ret = vec![PathInfo::Dir(path.to_owned())];

        for d in 0..max_depth {
            // Invariant: All directories in queue are of depth d, and all files and folders
            // of depth <= d are in ret.
            if queue.is_empty() {
                break;
            }

            let mut queue_items = queue.into_iter();

            // Try to paralelize this as much as possible and with as few requests as possible
            // 1) read_dirs retrieves the contents of a small (here: 12) number of directories in a single request
            // 2) We can run multiple such requests concurrently

            let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY_LIMIT));
            let mut fts = JoinSet::new();

            loop {
                let (batch_paths, batch_ids): (Vec<_>, Vec<_>) =
                    (&mut queue_items).take(12).unzip();
                if batch_ids.is_empty() {
                    break;
                }

                let this = self.clone();
                let sem = sem.clone();

                fts.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    let files = this.service.read_dirs(&batch_ids).await?;
                    Ok::<_, anyhow::Error>((batch_paths, files))
                });
            }

            queue = Vec::new();
            while let Some(next) = fts.join_next().await {
                let (paths, contents) = next.unwrap()?;
                for (folder_path, files) in paths.into_iter().zip(contents) {
                    let files = canonicalize(files);

                    for CanonicalFile { file, name } in files {
                        let file_path = folder_path.join_single(&name);

                        if file.is_folder() {
                            queue.push((file_path.clone(), file.id.clone()))
                        }

                        // Canonicalize should filter out all the files for which this fails, so unwrap here
                        let info = to_pathinfo(&file, file_path).unwrap();
                        ret.push(info);
                    }
                }
            }

            // At this point, all directories of depth <= d + 1 are in ret
        }

        Ok(ret)
    }
}

#[derive(Debug, Clone)]
struct CanonicalFile {
    name: String,
    file: drive_api::File,
}

fn canonicalize(mut files: Vec<drive_api::File>) -> Vec<CanonicalFile> {
    let mut ret = Vec::new();
    files.sort_by(|a, b| a.name.cmp(&b.name));

    // First, filter out files we do not want to be there. Notably, filter out all files for
    // which to_pathinfo would fail
    let mut files = files
        .into_iter()
        .filter(|f| match f.more {
            Some(MoreFileFields::BinaryFile { .. }) => true,
            None if f.is_folder() => true,
            _ => false,
        })
        .peekable();

    // Now disambiguate files that have the same name. Only take one of them. Note
    // that if two files have the same name, then they are adjacent (because of the sorting)
    while let Some(file) = files.next() {
        let mut candiates = vec![file];
        while let Some(next_file) = files.next_if(|next_file| next_file.name == candiates[0].name) {
            candiates.push(next_file);
        }

        // Pick the file with the smallest id
        let chosen_file = candiates
            .into_iter()
            .min_by(|a, b| a.id.cmp(&b.id))
            .unwrap();

        ret.push(CanonicalFile {
            name: chosen_file.name.to_owned(),
            file: chosen_file,
        })
    }

    ret
}

#[cfg(test)]
mod test {
    use futures::io::Cursor;

    use super::*;
    use crate::gdrive::service::fake::FakeDriveService;

    fn setup() -> Arc<GDriveClient<FakeDriveService>> {
        let service = FakeDriveService::default();
        Arc::new(GDriveClient::from_config(
            service,
            Preferences {
                cache_time: Duration::from_nanos(1),
                ..Default::default()
            },
        ))
    }

    #[tokio::test]
    async fn filenames_are_unique_in_a_folder() {
        let client = setup();
        let id = client.service.mkdir("root", "Ordner").await.unwrap().id;
        let id = client
            .service
            .mkdir(&id, "Noch ein ordner")
            .await
            .unwrap()
            .id;

        for i in 0..4 {
            client
                .service
                .create_file(&id, "test_file", futures::io::empty())
                .await
                .unwrap();
        }

        let path = "Ordner".into();

        let retrieved = client.list_files(path, usize::MAX).await.unwrap();

        // Should contain two folders, and one file
        assert_eq!(retrieved.len(), 3);
    }
}
