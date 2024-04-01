use crate::{FileInfo, PathInfo};
use futures::AsyncRead;
use std::fmt::Debug;
use std::future::Future;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum Error {
    #[error("{cpath} was not found")]
    NotFound { cpath: String },
    #[error("{cpath} not allowed")]
    NotAllowed { cpath: String },
    #[error("Expected a non-directory, got a directory. {cpath}.")]
    IsADirectory { cpath: String },
    #[error("Expected a directory, got something else. {cpath}.")]
    NotADirectory { cpath: String },
    #[error("Something else modified files and invalidated our state.")]
    ConcurrentModification,
    #[error(transparent)]
    Other(anyhow::Error),
}

pub trait AsyncReader: AsyncRead + Send {}
impl<T: AsyncRead + Send> AsyncReader for T {}

pub type Result<T> = std::result::Result<T, Error>;

/// Implementations should ensure that the number of concurrent requests does not exceed
/// the capabilities of an underlying server. This can be implemented with a semaphore.
/// 
/// Implementations should be cheaply cloneable, potentially by being wrapped inside of an Arc.
pub trait FileRepository: Debug + Send + Sync + Clone + 'static {
    type Reader: AsyncReader;

    /// Return all files and folders contained in the directory at `cpath` up to the specified `depth`. Note that if called
    /// with `depth` 0, this should return an empty list. With `depth` 1, this returns all the immediate children (but not the directory itself!).
    /// Use `u32::MAX` as `depth` to return all files and subfolders.
    ///
    /// # Errors
    /// - If `cpath` is a file this returns the error [`Error::NotADirectory`].
    fn read_dir(
        &self,
        cpath: &str,
        depth: u32,
    ) -> impl Future<Output = Result<Vec<PathInfo>>> + Send;

    fn get_metadata(&self, cpath: &str) -> impl Future<Output = Result<PathInfo>> + Send;

    /// Read file with the given relative name. Return a reader and the file size in bytes.
    ///
    /// # Errors
    ///
    /// - If the file at cpath is a directory, this returns [`Error::IsADirectory`]
    /// - If a subpath of `cpath` is a file, this returns either [`Error::NotADirectory`] or [`Error::NotFound`] (up to the implementation).
    fn read_file(
        &self,
        relative_name: &str,
    ) -> impl Future<Output = Result<(FileInfo, Self::Reader)>> + Send;

    /// Read the given `src` reader to completion and write its contents to the given cpath name. Do note the following behavior (which more or less mimicks the behavior
    // of `mkdir -p cpath_dir` and then writing the file on Linux):
    ///
    /// - If a file exists at cpath, then it is overwritten by the contents of src.
    /// - If cpath is of the form base/filename AND base is the cpath of a directory, a file named filename is created
    ///    with the contents src inside of that directory.
    ///
    /// # Errors
    ///
    /// - If cpath is a directory, this returns [`Error::IsADirectory`]. Consider deleting the directory in this case and trying again. Also
    ///   see [`Self::delete_orphaned_directory`]. This particular case is very important. We rely on this behavior in push_pull.
    /// - If a subpath os `cpath` is a file itself, this returns either [`Error::NotADirectory`] or [`Error::NotFound`] (up to the implementation).
    /// - If the directory part of `cpath` does not exist, this returns [`Error::NotFound`].
    fn write_file<R: AsyncReader>(
        &self,
        cpath: &str,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> impl Future<Output = Result<FileInfo>> + Send;

    /// Delete a file or a folder at `cpath` (including all subfolders, if necessary).
    fn delete_file(&self, cpath: &str) -> impl Future<Output = Result<()>> + Send;

    fn set_modified_time(
        &self,
        relative_name: &str,
        modtime: OffsetDateTime,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes a directory at cpath (and all subdirectories it contains) if it contains no files. See [`Self::delete_file`].
    fn delete_orphaned_directory(&self, cpath: &str) -> impl Future<Output = Result<bool>> + Send {
        async {
            let files_in_folder = self
                .read_dir(cpath, u32::MAX)
                .await?
                .into_iter()
                .filter(|d| matches!(d, PathInfo::File(_)))
                .count();

            if files_in_folder == 0 {
                self.delete_file(cpath).await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}
