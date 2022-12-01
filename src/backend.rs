use std::fmt::Debug;
use time::OffsetDateTime;
use crate::PathInfo;

pub trait FileRepository: Debug + Send + Clone {
    fn read_dirs<T: AsRef<str>>(&self, dirs_relative_name: &[T]) -> crate::Result<Vec<PathInfo>>;

    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        self.read_dirs(&[relative_name])
    }

    fn read_file(
        &self,
        relative_name: &str,
        dst: &mut Vec<u8>,
    ) -> crate::Result<()>;

    fn write_file(
        &self,
        relative_name: &str,
        src: &[u8],
    ) -> crate::Result<()>;

    fn delete_file(&self, relative_name: &str) -> crate::Result<()>;

    fn set_modified_time(&self, relative_name: &str, modtime: OffsetDateTime) -> crate::Result<()>;

    fn info(&self, relative_name: &str) -> crate::Result<PathInfo>;
}