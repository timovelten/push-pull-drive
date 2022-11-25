use std::fmt::Debug;
use time::OffsetDateTime;
use crate::{walker::ReadDir, FileInfo, PathInfo};

pub const KEY_BACKEND: &str = "backend";

pub trait FileRepository: Debug {
    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>>;

    /// Read the file contents into dst. Returns some file metdata. Note that some backends
    /// may not always return all metdata here. In particular, the LocalBackend does not compute the
    /// hash here.
    fn read_file(
        &self,
        relative_name: &str,
        dst: &mut Vec<u8>,
    ) -> crate::Result<FileInfo>;

    /// Write the content from src into the file with the relative name info.relative_name.
    /// If given, also changes that files modified time.
    fn write_file(
        &self,
        relative_name: &str,
        src: &[u8],
    ) -> crate::Result<()>;

    fn delete_file(&self, relative_name: &str) -> crate::Result<()>;

    /// Set the modification time of the specified file.
    fn set_modified_time(&self, relative_name: &str, modtime: OffsetDateTime) -> crate::Result<()>;

    fn info(&self, relative_name: &str) -> crate::Result<PathInfo>;
}

impl<F> ReadDir for F
where
    F: FileRepository,
{
    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        self.read_dir(relative_name)
    }
}
