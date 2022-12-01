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

impl<F> ReadDir for F
where
    F: FileRepository,
{
    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        self.read_dir(relative_name)
    }
}

pub trait ReadDir {
    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>>;
}

enum DirWalkerState {
    RetrieveIndex {
        // relative name of the directory, of which we need to retrieve the index.
        relative_name: String,
    },
    YieldFiles {
        index: Vec<PathInfo>,
    },
    ContinueWithNextDirectory,
    Done,
    // This state is really just a debugging aid.
    Invalid,
}