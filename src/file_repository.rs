use crate::paths::{RPath, RPathBuf};
use crate::{DirectoryInfo, FileInfo, PathInfo};
use futures::AsyncRead;
use std::fmt::Debug;
use std::future::Future;

#[derive(Debug, strum::Display)]
pub enum Error {
    NotFound,
    NotAllowed,
    IsADirectory,
    NotADirectory,
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

    /// Return all files and folder such that the given path is a subpath of their paths. That is,
    ///
    /// - If path is a directory this returns the directory itself and all of its children up to the given depth (depth 0 is just the directory itself,
    ///   depth 1 includes the immediate children and so on)
    /// - If path is a file, then this returns just that file and the given depth is irrelevant
    ///
    /// # Errors
    /// - If path does not exist, this returns [`Error::NotFound`]
    fn list_files(
        &self,
        path: &RPath,
        depth: u32,
    ) -> impl Future<Output = Result<Vec<PathInfo>>> + Send;

    /// Read file with the given relative name. Return a reader and the file size in bytes.
    ///
    /// # Errors
    ///
    /// - If the file at path is a directory, this returns [`Error::IsADirectory`]
    /// - If a subpath of `path` is a file, this returns either [`Error::NotADirectory`] or [`Error::NotFound`] (up to the implementation).
    fn read_file(
        &self,
        path: &RPath,
    ) -> impl Future<Output = Result<(FileInfo, Self::Reader)>> + Send;

    /// Read the given `src` reader to completion and write its contents to the given path name. Do note the following behavior (which more or less mimicks the behavior
    /// of `mkdir -p rpath_dir` and then writing the file on Linux):
    ///
    /// - If a file exists at path, then it is overwritten by the contents of src.
    /// - If rpath is of the form base/filename AND base is the rpath of a directory, a file named filename is created
    ///    with the contents src inside of that directory.
    ///
    /// # Errors
    ///
    /// - If rpath is a directory, this returns [`Error::IsADirectory`]. Consider deleting the directory in this case and trying again. Also
    ///   see [`Self::delete_orphaned_directory`]. This particular case is very important. We rely on this behavior in push_pull.
    /// - If a subpath of `path` is a file itself, this returns either [`Error::NotADirectory`] or [`Error::NotFound`] (up to the implementation).
    /// - If the directory part of `path` does not exist, this returns [`Error::NotFound`].
    fn write_file<R: AsyncReader>(
        &self,
        path: &RPath,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> impl Future<Output = Result<FileInfo>> + Send;

    fn mkdir(&self, path: &RPath) -> impl Future<Output = Result<DirectoryInfo>> + Send;

    /// Delete a file or a folder at `path` (including all subfolders, if necessary).
    fn delete(&self, path: &RPath) -> impl Future<Output = Result<()>> + Send;
}

#[derive(Debug, Clone, PartialEq, Hash, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    FileDeleted,
    DirDeleted,
    FileUpdated,
    DirCreated,
    FileCreated
}

#[derive(Debug, Clone, PartialEq, Hash, Eq, PartialOrd, Ord)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: RPathBuf
}
