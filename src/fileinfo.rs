use crate::{filetree::{self, Node}, paths::{RPath, RPathBuf}};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileChange {
    New(FileInfo),
    Modified(FileInfo),
    Deleted { rpath: RPathBuf },
}

impl FileChange {
    pub fn rpath(&self) -> &RPath {
        match self {
            FileChange::New(info) | FileChange::Modified(info) => &info.path,
            FileChange::Deleted { rpath } => rpath,
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, FileChange::New(_))
    }

    pub fn is_modified(&self) -> bool {
        matches!(self, FileChange::Modified(_))
    }

    pub fn is_deleted(&self) -> bool {
        matches!(self, FileChange::Deleted { rpath: _ })
    }
}

pub type HashValue = String;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PathInfo {
    Dir(DirectoryInfo),
    File(FileInfo),
}

impl filetree::Node for PathInfo {
    fn path(&self) -> &RPath {
        match self {
            PathInfo::Dir(DirectoryInfo { path: rpath }) => rpath.as_rpath(),
            PathInfo::File(FileInfo { path: rpath, .. }) => rpath.as_rpath(),
        }
    }

    fn typ(&self) -> crate::filetree::Type {
        match self {
            PathInfo::Dir(_) => filetree::Type::Dir,
            PathInfo::File(_) => filetree::Type::File,
        }
    }
}

impl PathInfo {
    pub fn is_file(&self) -> bool {
        match self {
            PathInfo::Dir(_) => false,
            PathInfo::File(_) => true,
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
