use std::{fmt::Debug, sync::Arc};
use async_trait::async_trait;
use time::OffsetDateTime;
use crate::PathInfo;

/// Implementations should ensure that the number of concurrent requests does not exceed
/// the capabilities of an underlying server. Preferably, this should be done using
/// `HttpClient::max_connections_per_host`. Alterantively, it could be implemented
/// manually using a Semaphore.
#[async_trait]
pub trait FileRepository: Debug + Send + Sync {
    async fn read_dirs(&self, dirs: Vec<String>) -> crate::Result<Vec<PathInfo>>;

    async fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        self.read_dirs(vec![relative_name.to_string()]).await
    }

    async fn read_file(
        &self,
        relative_name: &str
    ) -> crate::Result<Vec<u8>>;

    async fn write_file(
        &self,
        relative_name: &str,
        src: Arc<[u8]>,
    ) -> crate::Result<()>;

    async fn delete_file(&self, relative_name: &str) -> crate::Result<()>;

    async fn set_modified_time(&self, relative_name: &str, modtime: OffsetDateTime) -> crate::Result<()>;

    async fn info(&self, relative_name: &str) -> crate::Result<PathInfo>;
}