use crate::{local::file_hasher::FileHasher, FileInfo, PathInfo};
use futures::AsyncRead;
use std::fmt::Debug;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    #[error("{cpath} was not found")]
    NotFound { cpath: String },
    #[error("{cpath} not allowed")]
    NotAllowed { cpath: String },
    #[error("the object at the specified path is not a file {cpath}")]
    NotAFile { cpath: String },
    #[error("the object at is not a folder {cpath}")]
    NotAFolder { cpath: String },
    #[error(transparent)]
    Other(anyhow::Error),
}

pub trait AsyncReader: AsyncRead + Send {}
impl<T: AsyncRead + Send> AsyncReader for T {}

pub type Result<T> = std::result::Result<T, Error>;

/// Implementations should ensure that the number of concurrent requests does not exceed
/// the capabilities of an underlying server. This can be implemented with a semaphore.
pub trait FileRepository: Debug + Send + Sync {
    type Reader: AsyncReader;

    /// Return all files and folders contained in the directory at cpath. If cpath is not a directory,
    /// this returns Err(Error::NotADirectory(_)).
    /// depth specifies up to which depth children of the directory at cpath are returned (usually this involes recursion).
    /// If depth is 0, this returns an empty list. If depth is 1, this returns the immediate children of
    /// the directory at cpath. If depth is u32::MAX_VALUE, this returns all children.
    async fn read_dir(&self, cpath: &str, depth: u32) -> Result<Vec<PathInfo>>;

    async fn get_metadata(&self, cpath: &str) -> Result<PathInfo>;

    /// Read file with the given relative name. Return a reader and the file
    /// size in bytes.
    async fn read_file(&self, relative_name: &str) -> Result<(FileInfo, Self::Reader)>;

    /// Read the given `src` reader to completion and write its contents to the given relative name.
    async fn write_file<R: AsyncReader>(
        &self,
        cpath: &str,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        len: u64,
    ) -> Result<FileInfo>;

    async fn delete_file(&self, cpath: &str) -> Result<()>;

    async fn set_modified_time(&self, relative_name: &str, modtime: OffsetDateTime) -> Result<()>;
}

pub trait RemoteRepository: FileRepository {
    type Hasher: FileHasher;
}
