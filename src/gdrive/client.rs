use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context};
use async_recursion::async_recursion;
use cached::TimedCache;
use drive_api::{DriveApi, File, FilesReq, GoogleCredential, MoreFileFields};
use futures::{stream::FuturesUnordered, AsyncRead, StreamExt as _, TryFutureExt};
use tokio::task::JoinSet;

use crate::{
    file_repository::{self, Error},
    filetree::FileTree,
    paths::{RPath, RPathBuf},
    FileInfo, PathInfo,
};
use cached::Cached;
use std::fmt::Write;

use super::service::{DriveService, DriveServiceImpl};

#[derive(Debug)]
/// Conceptually this is a singleton. There should be just one instance of this, wrapped in an Arc.
pub(super) struct GDriveClient<S: DriveService = DriveServiceImpl> {
    service: S,
}

/// Google does not really like too many concurrent connections. In my personal testing, the limit seemed to be somewhere below 10.
pub const CONCURRENCY_LIMIT: usize = 8;

impl GDriveClient {
    pub fn new(cred: GoogleCredential) -> Self {
        Self {
            service: DriveServiceImpl::new(cred),
        }
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

impl<Service: DriveService> GDriveClient<Service> {
    #[async_recursion]
    async fn resolve(&self, path: &RPath) -> anyhow::Result<String> {
        // We could probaly rewrite this iteratively and avoid async_recursion...

        let Some((base, name)) = path.split_right() else {
            return Ok(self.root_id().await?);
        };

        let parent_id = self.resolve(base).await?;
        let contents = self.service.read_dir(&parent_id).await?;
        let canon_contents = canonicalize(contents);

        for file in canon_contents {
            if file.name == name {
                return Ok(file.file.id);
            }
        }

        bail!(Error::NotFound)
    }

    async fn root_id(&self) -> anyhow::Result<String> {
        Ok(self.service.get_file("root").await?.id)
    }

    pub async fn mkdir(&self, path: &RPath) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn metadata(&self, path: &RPath) -> anyhow::Result<PathInfo> {
        let file_id = self.resolve(path).await?;
        let file = self.service.get_file(&file_id).await?;
        let ret = to_pathinfo(&file, path.to_owned()).ok_or(Error::NotAllowed)?;

        Ok(ret)
    }

    pub async fn read_file(
        &self,
        path: &RPath,
    ) -> anyhow::Result<(FileInfo, Box<dyn AsyncRead + Send>)> {
        let file_id = self.resolve(path).await?;
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
        len: u64,
    ) -> anyhow::Result<FileInfo> {
        todo!()
    }

    pub async fn update_file<R: crate::file_repository::AsyncReader>(
        &self,
        path: &RPath,
        src: R,
        len: u64,
    ) -> anyhow::Result<FileInfo> {
        todo!()
    }

    pub async fn set_modified_time(
        &self,
        path: &RPath,
        time: time::OffsetDateTime,
    ) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn delete(&self, path: &RPath) -> anyhow::Result<()> {
        let id = self.resolve(path).await?;
        self.service.delete(&id).await?;
        Ok(())
    }

    pub async fn list_files(
        self: &Arc<Self>,
        path: &RPath,
        max_depth: usize,
    ) -> anyhow::Result<Vec<PathInfo>> {
        let id = self.resolve(path).await?;

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
                    for file in files {
                        let file_path = folder_path.join_single(&file.name);

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
