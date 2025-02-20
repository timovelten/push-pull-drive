use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context};
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

impl<Service: DriveService> GDriveClient<Service> {
    pub fn new(credential: GoogleCredential) -> Self {
        todo!()
    }

    async fn resolve(&self, path: &RPath) -> anyhow::Result<String> {
        todo!()
    }

    pub async fn mkdir(&self, path: &RPath) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn metadata(&self, path: &RPath) -> anyhow::Result<PathInfo> {
        let file_id = self.resolve(path).await?;
        let file = self.service.get_file(&file_id).await?;

        match file.more {
            Some(MoreFileFields::BinaryFile {
                head_revision_id,
                sha1_checksum,
                size,
            }) => {
                let info = FileInfo {
                    path: path.to_owned(),
                    size,
                    modified: file.modified_time,
                };

                Ok(PathInfo::File(info))
            }
            None if file.is_folder() => Ok(PathInfo::Dir(path.to_owned())),
            _ => Err(anyhow!(Error::NotAllowed)),
        }
    }

    pub async fn read_file(
        &self,
        path: &RPath,
    ) -> anyhow::Result<(FileInfo, Box<dyn AsyncRead + Send>)> {
        let file_id = self.resolve(path).await?;
        let file = self.service.get_file(&file_id).await?;

        match file.more {
            Some(MoreFileFields::BinaryFile {
                head_revision_id,
                sha1_checksum,
                size,
            }) => {
                let info = FileInfo {
                    path: path.to_owned(),
                    size,
                    modified: file.modified_time,
                };

                let body = self
                    .service
                    .get_body(&file.id, Some(&head_revision_id))
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
    ) -> anyhow::Result<drive_api::File> {
        todo!()
    }

    pub async fn write_file<R: crate::file_repository::AsyncReader>(
        &self,
        path: &RPath,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> anyhow::Result<FileInfo> {
        todo!()
    }

    pub async fn delete(&self, path: &RPath) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn list_files(
        self: &Arc<Self>,
        filter: &RPath,
        max_depth: usize,
    ) -> anyhow::Result<Vec<PathInfo>> {
        todo!()
    }
}
