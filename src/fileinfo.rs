#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileChange {
    New(FileInfo),
    Modified(FileInfo),
    Deleted {
        relative_name: String
    },
}

impl FileChange {
    pub fn relative_name(&self) -> &String {
        match self {
            FileChange::New(info) | FileChange::Modified(info) => &info.relative_name,
            FileChange::Deleted { relative_name } => relative_name,
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, FileChange::New(_))
    }

    pub fn is_modified(&self) -> bool {
        matches!(self, FileChange::Modified(_))
    }

    pub fn is_deleted(&self) -> bool {
        matches!(self, FileChange::Deleted { relative_name: _})
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
    /// TODO: Document that we explicitly do not specify at this point if this is md5 or something
    /// else. How (and if at all) this value is interpreted is up to the backend.
    pub hash: Option<HashValue>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirectoryInfo {
    pub relative_name: String,
}

impl FileInfo {
    pub fn is_newer_than(&self, other: &Self) -> bool {
        // TODO: What if hash is lower cased hex?
        // FIXME TODO: The hash == down there is wrong???
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
