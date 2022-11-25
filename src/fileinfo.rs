#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum FileChange {
    New,
    Modified,
    Deleted,
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

    pub fn relative_name(&self) -> &str {
        match self {
            PathInfo::Dir(x) => &x.relative_name,
            PathInfo::File(x) => &x.relative_name,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileInfo {
    pub relative_name: String,
    pub size: u64,
    pub modified: time::OffsetDateTime,
    pub hash: Option<HashValue>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirectoryInfo {
    pub relative_name: String,
}

impl FileInfo {
    pub fn is_newer_than(&self, other: &Self) -> bool {
        let ret = self.size != other.size
            || self.modified > other.modified
            // Note that we do not have to explicitly verify that other is_some()
            || self.hash == other.hash && self.hash.is_some();
        ret
    }

    pub fn dir_and_name(&self) -> (&str, &str) {
        if let Some((dir, name)) = self.relative_name.rsplit_once('/') {
            (dir, name)
        } else {
            ("", &self.relative_name)
        }
    }
}

impl From<crate::db::TrackedFile> for FileInfo {
    fn from(tf: crate::db::TrackedFile) -> Self {
        FileInfo {
            relative_name: tf.relative_name,
            size: tf.size,
            modified: tf.modified,
            hash: tf.hash,
        }
    }
}
