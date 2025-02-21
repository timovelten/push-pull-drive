use crate::paths::RPath;
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
                    Some(Error::NotFound | Error::NotAllowed) => Ok(None),
                    _ => Err(err),
                },
            }
        }
    }

    /// Read file with the given relative name.
    fn read_file(
        &self,
        path: &RPath,
    ) -> impl Future<Output = Result<(FileInfo, Self::Reader)>> + Send;

    /// Read the given `src` reader to completion and write its contents to the given path name.
    fn update_file<R: AsyncReader>(
        &self,
        path: &RPath,
        src: R,
        len: u64,
    ) -> impl Future<Output = Result<FileInfo>> + Send;

    fn create_file<R: AsyncReader>(
        &self,
        path: &RPath,
        src: R,
        len: u64,
    ) -> impl Future<Output = Result<FileInfo>> + Send;

    fn set_modified_time(
        &self,
        path: &RPath,
        time: time::OffsetDateTime,
    ) -> impl Future<Output = Result<()>> + Send;

    /// This function may or may not return an error if the directory already exists.
    fn mkdir(&self, path: &RPath) -> impl Future<Output = Result<()>> + Send;

    /// Delete a file or a folder at `path` (including all subfolders, if necessary).
    fn delete(&self, path: &RPath) -> impl Future<Output = Result<()>> + Send;
}
