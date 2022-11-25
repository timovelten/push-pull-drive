use std::fmt::Debug;
use anyhow::anyhow;
use time::OffsetDateTime;
use crate::{walker::ReadDir, Error, FileInfo, PathInfo};

pub const KEY_BACKEND: &str = "backend";

pub trait FileRepository: Debug {
    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>>;

    /// Read the file contents into dst. Returns some file metdata. Note that some backends
    /// may not always return all metdata here. In particular, the LocalBackend does not compute the
    /// hash here.
    fn read_file(
        &self,
        relative_name: &str,
        dst: &mut dyn std::io::Write,
    ) -> crate::Result<FileInfo>;

    /// Write the content from src into the file with the relative name info.relative_name.
    /// If given, also changes that files modified time.
    fn write_file(
        &self,
        src: &mut dyn std::io::Read,
        relative_name: &str
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

#[derive(Eq, PartialEq, Debug)]
pub struct NoBackend;

impl NoBackend {
    fn uninitialized<T>() -> crate::Result<T> {
        Err(Error::Other(anyhow!("backend is not initialized")))
    }
}

impl FileRepository for NoBackend {
    fn read_dir(&self, _relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        Self::uninitialized()
    }

    fn read_file(
        &self,
        _relative_name: &str,
        _dstt: &mut dyn std::io::Write,
    ) -> crate::Result<FileInfo> {
        Self::uninitialized()
    }

    fn write_file(
        &self,
        _src: &mut dyn std::io::Read,
        _relative_namee: &str
    ) -> crate::Result<()> {
        Self::uninitialized()
    }

    fn delete_file(&self, _relative_name: &str) -> crate::Result<()> {
        Self::uninitialized()
    }

    fn info(&self, _relative_name: &str) -> crate::Result<PathInfo> {
        Self::uninitialized()
    }

    fn set_modified_time(&self, _relative_name: &str, _modtime: OffsetDateTime) -> crate::Result<()> {
        Self::uninitialized()
    }
}
