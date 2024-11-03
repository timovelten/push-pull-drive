use crate::db::Tracked;
use crate::filetree::Patch;
use crate::paths::{RPath, RPathBuf};
use crate::{FileInfo, PathInfo};
use futures::AsyncRead;
use std::fmt::Debug;
use std::future::Future;
use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum Error {
    #[error("{rpath:?} was not found")]
    NotFound { rpath: RPathBuf },
    #[error("{rpath:?} not allowed")]
    NotAllowed { rpath: RPathBuf },
    #[error("Expected a non-directory, got a directory. {rpath:?}.")]
    IsADirectory { rpath: RPathBuf },
    #[error("Expected a directory, got something else. {rpath:?}.")]
    NotADirectory { rpath: RPathBuf },
    #[error(transparent)]
    Other(anyhow::Error),
}

pub trait AsyncReader: AsyncRead + Send {}
impl<T> AsyncReader for T where T: AsyncRead + Send {}

pub type Result<T> = std::result::Result<T, Error>;

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
        relative_name: &RPath,
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

    /// Delete a file or a folder at `rpath` (including all subfolders, if necessary).
    fn delete_file(&self, path: &RPath) -> impl Future<Output = Result<()>> + Send;

    fn determine_changes(&self, path: &RPath, all_files: Vec<Tracked>) -> impl Future<Output = Result<Vec<Patch>>> + Send;

    /// Deletes a directory at path (and all subdirectories it contains) if it contains no files. See [`Self::delete_file`].
    fn delete_orphaned_directory(&self, path: &RPath) -> impl Future<Output = Result<bool>> + Send {
        async {
            let files_in_folder = self
                .list_files(path, u32::MAX)
                .await?
                .into_iter()
                .filter(|d| matches!(d, PathInfo::File(_)))
                .count();

            log::debug!("delete_oprhanded_directory found {} files", files_in_folder);

            if files_in_folder == 0 {
                self.delete_file(path).await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}