use crate::{filetree::{self}, paths::{RPath, RPathBuf}};


#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PathInfo {
    Dir(DirectoryInfo),
    File(FileInfo),
}

impl PathInfo {
    pub fn path(&self) -> &RPath {
        match self {
            Self::Dir(DirectoryInfo { path }) => path,
            Self::File(FileInfo { path, .. }) => path
        }
    }

    pub fn is_file(&self) -> bool {
        match self {
            PathInfo::Dir(_) => false,
            PathInfo::File(_) => true,
        }
    }

    pub fn unwrap_fileinfo(&self) -> &FileInfo {
        match self {
            Self::File(info) => info,
            Self::Dir(_) => panic!("Called unwrap_fileinfo on PathInfo::Dir")
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileInfo {
    pub path: RPathBuf,
    pub size: u64,
    pub modified: time::OffsetDateTime
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirectoryInfo {
    pub path: RPathBuf,
}

impl filetree::Node for PathInfo {
    fn path(&self) -> &RPath {
        Self::path(self)
    }

    fn is_terminal(&self) -> bool {
        Self::is_file(&self)
    }
}
