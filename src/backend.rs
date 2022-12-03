use std::fmt::Debug;
use time::OffsetDateTime;
use crate::PathInfo;

/// Implementations should ensure that the number of concurrent requests does not exceed
/// the capabilities of an underlying server. Preferably, this should be done using
/// `HttpClient::max_connections_per_host`. Alterantively, it could be implemented
/// manually using a Semaphore.
pub trait FileRepository: Debug + Send + Sync {
    fn read_dirs(&self, dirs: Vec<String>) -> crate::Result<Vec<PathInfo>>;

    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        self.read_dirs(vec![relative_name.to_string()])
    }

    fn read_file(
        &self,
        relative_name: &str
    ) -> crate::Result<Vec<u8>>;

    fn write_file<R: AsRef<[u8]>>(
        &self,
        relative_name: &str,
        src: R,
    ) -> crate::Result<()>;

    fn delete_file(&self, relative_name: &str) -> crate::Result<()>;

    fn set_modified_time(&self, relative_name: &str, modtime: OffsetDateTime) -> crate::Result<()>;

    fn info(&self, relative_name: &str) -> crate::Result<PathInfo>;
}