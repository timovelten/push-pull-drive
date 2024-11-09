use crate::{file_repository::{Change, ChangeKind}, paths::{RPath, RPathBuf}};

#[derive(Default)]
struct Plan {
    delete_files: Vec<RPathBuf>,
    delete_dirs: Vec<RPathBuf>,
    update_files: Vec<RPathBuf>,
    create_dirs: Vec<RPathBuf>,
    create_files: Vec<RPathBuf>,
}

enum ChangeEffect {
    Unchanged,
    Modified,
    Deleted
}

struct ChangeList<'a> {
    changes: &'a [Change]
}

impl<'a> ChangeList<'a> {
    /// This will panic if changes is not sorted!
    fn new(changes: &'a [Change]) -> Self {
        todo!()
    }

    fn is_deleted(&self, path: &RPath) -> bool {
        todo!()
    }

    fn is_modified(&self, path: &RPath) -> bool {
        todo!()
    }

    fn effect_on(&self, path: &RPath) -> ChangeEffect {
        if self.is_deleted(path) {
            ChangeEffect::Deleted
        } else if self.is_modified(path) {
            ChangeEffect::Modified
        } else {
            ChangeEffect::Unchanged
        }
    }
}