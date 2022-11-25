//! This provides the necessary context for the rest of the app. In particular, we store
//! from where we were launched, so that the user can chose to operate on only parts
//! of the directory tree, instead of everything.
//!
//! We take the time, to document some standing assumptions. Unfortunately, we have to work
//! a lot with paths.
//!
//! In the following, we often speak of "relative names". By this, we mean a string of the form
//! abc/def/ceg
//! i.e. a slash separated path, that does not contain dots. In particular, it does not start with a dot.

use std::collections::HashMap;
use std::collections::HashSet;
use crate::backend::FileRepository;
use crate::walker::DirWalker;
use crate::{db, App, Error, PathInfo};
use crate::{db::TrackedFile, FileChange};

pub struct DefaultApp<L, R> {
    // Note how this app does not even know the "root" directory, because it does not need to!
    /// This is *not* the cwd we get from the operation system. Instead, this
    /// is the cwd as a "relative name"
    cwd: String,
    db: db::DbRepository,

    local: L,
    remote: R,
}

impl<L: FileRepository, R: FileRepository> DefaultApp<L, R> {
    /// This expects to find a folder CONFIG_DIR containing the FILE DATABASE_NAME to succeed.
    pub fn new(cwd: String, db: db::DbRepository, local: L, remote: R) -> Self {
        DefaultApp {
            db,
            cwd,
            local,
            remote,
        }
    }
}

impl<L: FileRepository, R: FileRepository> App for DefaultApp<L, R> {
    fn init_cmd(&self, _: crate::Backends) -> crate::Result<()> {
        Err(Error::AlreadyInitialized)
    }

    fn status_cmd(&self) -> crate::Result<()> {
        let status = self.status_helper(&self.local)?;
        self.print_status(&status);

        Ok(())
    }

    fn remote_status_cmd(&self) -> crate::Result<()> {
        let status = self.status_helper(&self.remote)?;
        self.print_status(&status);

        Ok(())
    }

    fn pull_cmd(&self) -> crate::Result<()> {
        let remote_status = self.status_helper(&self.remote)?;
        self.push_pull_helper(&self.remote, &self.local, &remote_status, true)
    }

    fn push_cmd(&self) -> crate::Result<()> {
        let remote_status = self.status_helper(&self.local)?;
        self.push_pull_helper(&self.local, &self.remote, &remote_status, true)
    }
}

impl<L: FileRepository, R: FileRepository> DefaultApp<L, R> {
    fn track_current_version(&self, relative_name: &str) -> crate::Result<()> {
        let path_info = self.local.info(relative_name)?;

        if let PathInfo::File(info) = path_info {
            let tf = TrackedFile {
                relative_name: info.relative_name,
                modified: info.modified,
                size: info.size,
                hash: None,
            };

            // TODO: Calculate hashes
            self.db.update_tracked_file(tf)?;

            Ok(())
        } else {
            panic!("tried to track the current version of a directory");
        }
    }

    // Determines how files have changed when comapared to another directory structure.
    fn status_helper<RD: crate::walker::ReadDir>(
        &self,
        r: &RD,
    ) -> crate::Result<HashMap<String, FileChange>> {
        let mut ret = HashMap::new();

        let tfs = self
            .db
            .get_all_tracked_files(&self.cwd)?
            .into_iter()
            .map(|tf| (tf.relative_name.to_owned(), tf))
            .collect::<HashMap<_, _>>();

        let mut files = HashSet::new();

        let walker = DirWalker::new(self.cwd.to_owned(), r);

        for path_info in walker {
            let p = match path_info {
                Ok(info) => info,
                Err(err) => {
                    println!("Ignoring error: {:?}", err);
                    continue;
                }
            };

            if let PathInfo::File(info) = p {
                files.insert(info.relative_name.clone());

                if let Some(tf) = tfs.get(&info.relative_name) {
                    debug_assert_eq!(tf.relative_name, info.relative_name);

                    // This file exists in the remote, and we track it. So check if it was modified.
                    if info.is_newer_than(&tf.clone().into()) {
                        ret.insert(info.relative_name, FileChange::Modified);
                    }
                } else {
                    // Remote file is new
                    ret.insert(info.relative_name, FileChange::New);
                }
            }
        }

        // Finally, use the list of files we built to determine which of tracked files were deleted
        for (_, tf) in tfs {
            if !files.contains(&tf.relative_name) {
                ret.insert(tf.relative_name, FileChange::Deleted);
            }
        }

        Ok(ret)
    }

    /// Given a list of fileChanges, pushes these changes from A to B.
    /// If you think about it, both push and pull can be implemented from this.
    fn push_pull_helper<A: FileRepository, B: FileRepository>(
        &self,
        from: &A,
        to: &B,
        changes: &HashMap<String, FileChange>,
        track: bool,
    ) -> crate::Result<()> {
        // Allocate the temporary buffer only once, so we at least minimize the number of allocations
        let mut buffer: Vec<u8> = Vec::new();

        for (relative_name, change) in changes {
            buffer.clear();

            match change {
                FileChange::New | FileChange::Modified => {
                    // For now, we just download the entire file into memory.
                    // There are worse things...
                    let info = from.read_file(relative_name, &mut buffer)?;
                    to.write_file(&mut buffer.as_slice(), relative_name, Some(info.modified))?;

                    if track {
                        self.track_current_version(relative_name)?;
                    }
                }
                FileChange::Deleted => {
                    to.delete_file(&relative_name)?;

                    if track {
                        self.db.delete_tracked_file(&relative_name)?;
                    }
                }
            }
        }

        Ok(())
    }

    fn print_status(&self, changes: &HashMap<String, FileChange>) {
        println!("{:#?}", changes);
    }
}
