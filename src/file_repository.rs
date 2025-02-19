use crate::paths::{RPath, RPathBuf};
use crate::{FileInfo, PathInfo};
use futures::AsyncRead;
use std::fmt::Debug;
use std::future::Future;

#[derive(Debug, strum::Display)]
pub enum Error {
    NotFound,
    NotAllowed,
    IsADirectory,
    NotADirectory,
    Exists,
}
impl std::error::Error for Error {}

pub trait AsyncReader: AsyncRead + Send {}
impl<T> AsyncReader for T where T: AsyncRead + Send {}

pub type Result<T> = anyhow::Result<T>;

/// Implementations should ensure that the number of concurrent requests does not exceed
/// the capabilities of an underlying server. This can be implemented with a semaphore.
///
/// Implementations should be cheaply cloneable, potentially by being wrapped inside of an Arc.
///
/// Note that this trait is not object safe.
pub trait FileRepository: Debug + Send + Sync + Clone + 'static {
    type Reader: AsyncReader;

    /// Implementations should indicate how many concurrent jobs they allow. This is needed because
    /// e.g. Google drive does definitely not allow arbitrarily many concurrent connections.
    /// Maybe we will make this more fine grained in the future. Callers should honor this value, at least
    /// on a "best effort" basis.
    const CONCURRENCY_LIMIT: usize;

    /// List all files and folders that are children of the given path. Returns [Error::NotFound]
    /// if the path does not exist.
    fn list_files(
        &self,
        path: &RPath,
        depth: usize,
    ) -> impl Future<Output = Result<Vec<PathInfo>>> + Send;

    fn metadata(&self, path: &RPath) -> impl Future<Output = Result<PathInfo>> + Send;

    fn maybe_metadata(
        &self,
        path: &RPath,
    ) -> impl Future<Output = Result<Option<PathInfo>>> + Send {
        async {
            match self.metadata(path).await {
                Ok(info) => Ok(Some(info)),
                Err(err) => match err.downcast_ref::<Error>() {
                    Some(Error::NotFound | Error::NotAllowed) => return Ok(None),
                    _ => return Err(err),
                },
            }
        }
    }

    /// Read file with the given relative name.
    fn read_file(
        &self,
        path: &RPath,
    ) -> impl Future<Output = Result<(FileInfo, Self::Reader)>> + Send;

    /// Read the given `src` reader to completion and write its contents to the given path name. If the file
    /// does not exist, it is created. This function does not create and directories.
    ///
    /// If at all possible this operation should be atomic: If the operation fails half way through, the original
    /// contents should still be intact.
    fn write_file<R: AsyncReader>(
        &self,
        path: &RPath,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> impl Future<Output = Result<FileInfo>> + Send;

    /// This function may or may not return an error if the directory already exists.
    fn mkdir(&self, path: &RPath) -> impl Future<Output = Result<()>> + Send;

    /// Delete a file or a folder at `path` (including all subfolders, if necessary).
    fn delete(&self, path: &RPath) -> impl Future<Output = Result<()>> + Send;
}

#[derive(Debug, Clone, PartialEq, Hash, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    FileDeleted,
    DirDeleted,
    FileUpdated,
    DirCreated,
    FileCreated,
}

#[derive(Debug, Clone, PartialEq, Hash, Eq, PartialOrd, Ord)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: RPathBuf,
}

#[cfg(test)]
pub mod util {
    use super::*;
    use crate::filetree::FileTree;
    use futures::io::Cursor;
    use anyhow::anyhow;

    #[derive(Clone, Debug)]
    pub struct MockRepository {
        tree: FileTree<PathInfo>,
    }

    impl MockRepository {
        pub fn new(tree: FileTree<PathInfo>) -> MockRepository {
            Self { tree }
        }
    }

    impl FileRepository for MockRepository {
        type Reader = Cursor<Vec<u8>>;

        const CONCURRENCY_LIMIT: usize = 4;

        async fn list_files(
            &self,
            path: &crate::paths::RPath,
            depth: usize,
        ) -> super::Result<Vec<PathInfo>> {
            let ret = self
                .tree
                .all_children(path)
                .ok_or(Error::NotFound)?
                .filter(|other| other.path().len() - path.len() <= depth)
                .cloned()
                .collect();

            Ok(ret)
        }

        /// Get metadata of the file at the specified path.
        async fn metadata(&self, path: &RPath) -> Result<PathInfo> {
            match self.tree.get(path) {
                Some(info) => Ok(info.clone()),
                None => return Err(anyhow!(Error::NotFound)),
            }
        }

        async fn read_file(
            &self,
            _path: &crate::paths::RPath,
        ) -> super::Result<(crate::FileInfo, Self::Reader)> {
            unimplemented!()
        }

        async fn write_file<R: super::AsyncReader>(
            &self,
            _path: &crate::paths::RPath,
            _modified_time: Option<time::OffsetDateTime>,
            _src: R,
            _len: u64,
        ) -> Result<crate::FileInfo> {
            unimplemented!()
        }

        async fn mkdir(&self, _path: &crate::paths::RPath) -> super::Result<()> {
            unimplemented!()
        }

        async fn delete(&self, _path: &crate::paths::RPath) -> super::Result<()> {
            unimplemented!()
        }
    }
}
