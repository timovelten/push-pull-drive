use crate::paths::{RPath, RPathBuf};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileChange {
    New(FileInfo),
    Modified(FileInfo),
    Deleted { rpath: RPathBuf },
}

impl FileChange {
    pub fn rpath(&self) -> &RPath {
        match self {
            FileChange::New(info) | FileChange::Modified(info) => &info.rpath,
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

impl PathInfo {
    pub fn is_file(&self) -> bool {
        match self {
            PathInfo::Dir(_) => false,
            PathInfo::File(_) => true,
        }
    }

    pub fn rpath(&self) -> &RPath {
        match self {
            PathInfo::Dir(DirectoryInfo { rpath }) => rpath.as_rpath(),
            PathInfo::File(FileInfo { rpath, .. }) => rpath.as_rpath(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileInfo {
    pub rpath: RPathBuf,
    pub size: u64,
    pub modified: time::OffsetDateTime,
    /// This value should typically only be compared to other FileInfo's created by the same
    /// repository. We do not specify what hash algorithm is used (or how much of the file is hashed,
    /// or how long this value is, or if it exists at all etc).
    /// For instance, Google Drive supplies us with a SHA1 hash, where as Dropbox has a very custom
    /// checksum implementation
    pub hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirectoryInfo {
    pub rpath: RPathBuf,
}
