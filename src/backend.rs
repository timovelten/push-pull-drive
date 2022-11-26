use std::fmt::Debug;
use time::OffsetDateTime;
use crate::{PathInfo, DirectoryInfo};

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
    ) -> crate::Result<()>;

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

pub struct DirWalker<'a, R: ReadDir> {
    read: &'a R,

    state: DirWalkerState,
    next_directories: Vec<String>,
}

impl<'a, R: ReadDir> DirWalker<'a, R> {
    pub fn new(relative_name: String, read: &'a R) -> Self {
        return Self {
            read,
            state: DirWalkerState::RetrieveIndex { relative_name },
            next_directories: vec![],
        };
    }
}

impl<'a, R: ReadDir> Iterator for DirWalker<'a, R> {
    type Item = crate::Result<PathInfo>;

    fn next(&mut self) -> Option<Self::Item> {
        match std::mem::replace(&mut self.state, DirWalkerState::Invalid) {
            DirWalkerState::Invalid => {
                panic!("this state should never be observed.");
            }
            DirWalkerState::RetrieveIndex { relative_name } => {
                match self.read.read_dir(&relative_name) {
                    Ok(index) => {
                        self.state = DirWalkerState::YieldFiles { index };

                        Some(Ok(PathInfo::Dir(DirectoryInfo { relative_name })))
                    }
                    Err(err) => {
                        self.state = DirWalkerState::ContinueWithNextDirectory;
                        println!("Retrieve index of {:?} failed", relative_name);
                        Some(Err(err))
                    }
                }
            }
            DirWalkerState::YieldFiles { mut index } => {
                let next_file = index.pop();
                self.state = DirWalkerState::YieldFiles { index };

                match next_file {
                    // Directory
                    Some(PathInfo::Dir(DirectoryInfo { relative_name })) => {
                        self.next_directories.push(relative_name);
                        self.next()
                    }
                    // File
                    Some(file) => Some(Ok(file)),
                    // Done
                    None => {
                        self.state = DirWalkerState::ContinueWithNextDirectory;
                        self.next()
                    }
                }
            }
            DirWalkerState::ContinueWithNextDirectory => {
                if let Some(next_dir) = self.next_directories.pop() {
                    self.state = DirWalkerState::RetrieveIndex {
                        relative_name: next_dir,
                    };
                    self.next()
                } else {
                    self.state = DirWalkerState::Done;
                    self.next()
                }
            }
            DirWalkerState::Done => None,
        }
    }
}
