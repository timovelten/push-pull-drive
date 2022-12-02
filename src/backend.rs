use std::{fmt::Debug, io::{Read, Seek}};
use time::OffsetDateTime;
use crate::PathInfo;

pub trait ReadSeek : Read + Seek + Send {}
impl <T: Read + Seek + Send> ReadSeek for T {}

/// Implementations should ensure that the number of concurrent requests does not exceed
/// the capabilities of an underlying server. Preferably, this should be done using
/// `HttpClient::max_connections_per_host`. Alterantively, it could be implemented
/// manually using a Semaphore.
pub trait FileRepository: Debug + Send + Sync {
    type Reader: Read + Send;

    fn read_dirs<T: AsRef<str>>(&self, dirs_relative_name: &[T]) -> crate::Result<Vec<PathInfo>>;

    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        self.read_dirs(&[relative_name])
    }

    fn read_file(
        &self,
        relative_name: &str
    ) -> crate::Result<Self::Reader>;

    fn write_file<R: ReadSeek>(
        &self,
        relative_name: &str,
        src: R,
    ) -> crate::Result<()>;

    fn delete_file(&self, relative_name: &str) -> crate::Result<()>;

    fn set_modified_time(&self, relative_name: &str, modtime: OffsetDateTime) -> crate::Result<()>;

    fn info(&self, relative_name: &str) -> crate::Result<PathInfo>;
}