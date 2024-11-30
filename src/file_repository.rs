use crate::filetree::FileTree;
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
    Exists
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

    /// Return a file tree, rooted at `Rpath::empty()` such that all nodes of
    /// of this tree are comparable to the filter path. For the purposes of this method, that means that
    /// any returned path is either a child or a parent (or both) of the filter path.
    ///
    /// In particular, this method should never error with "not found". The caller has to determine
    /// whether the file was found or not from the result.
    ///
    /// The depth parameter can be used to limit the height of returned tree.
    fn file_tree(
        &self,
        filter: &RPath,
        depth: usize,
    ) -> impl Future<Output = Result<FileTree<PathInfo>>> + Send;

    /// List all files and folders that are children of the given path. Returns not found if the
    /// given path does not exist.
    fn list_files(
        &self,
        path: &RPath,
        depth: usize,
    ) -> impl Future<Output = Result<Vec<PathInfo>>> + Send {
        async move {
            // Avoid overflow when depth is already usize::MAX
            let depth = depth.saturating_add(path.len());

            let tree = self.file_tree(path, depth).await?;
            match tree.reroot(path) {
                Ok(ret) => Ok(ret.into_vec()),
                Err(tree) => {
                    log::debug!("Failed to re-root tree to '{path:?}': {tree:?}");
                    Err(Error::NotFound.into())
                }
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
