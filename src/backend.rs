use std::fmt::Debug;
use async_trait::async_trait;
use futures::AsyncRead;
use time::OffsetDateTime;
use crate::PathInfo;

/// Implementations should ensure that the number of concurrent requests does not exceed
/// the capabilities of an underlying server. This can be implemented with a semaphore.
#[async_trait]
pub trait FileRepository: Debug + Send + Sync {
    type Reader: AsyncRead + Send;

    async fn read_dirs(&self, dirs: Vec<String>) -> crate::Result<Vec<PathInfo>>;

    async fn read_file(
        &self,
        relative_name: &str
    ) -> crate::Result<(Self::Reader, u64)>;

    async fn write_file<R: AsyncRead + Send>(
        &self,
        relative_name: &str,
        src: R,
        len: u64
    ) -> crate::Result<()>;

    async fn delete_file(&self, relative_name: &str) -> crate::Result<()>;

    async fn set_modified_time(&self, relative_name: &str, modtime: OffsetDateTime) -> crate::Result<()>;

    async fn info(&self, relative_name: &str) -> crate::Result<PathInfo>;

    //
    // Provided methods
    //

    async fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        self.read_dirs(vec![relative_name.to_string()]).await
    }

    /// List all of the files and directories contained in a given directory. This does not include the
    /// directory itself.
    /// The results are guaranteed to be sorted by relative_name.
    async fn read_dir_all(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        let mut ret: Vec<PathInfo> = Vec::new();

        let mut next_directories = vec![relative_name.to_string()];

        while next_directories.len() > 0 {
            let files = self.read_dirs(next_directories).await?;
            next_directories = Vec::new();

            for path_info in files {
                if let PathInfo::Dir(dir) = &path_info {
                    next_directories.push(dir.relative_name.to_string());
                }
                ret.push(path_info);
            }
        }

        ret.sort_by(|x, y| x.relative_name().cmp(y.relative_name()));

        Ok(ret)
    }
}