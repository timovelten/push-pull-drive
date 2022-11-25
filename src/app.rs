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

use crate::backend::FileRepository;
use crate::walker::DirWalker;
use crate::{db, App, Error, PathInfo};
use crate::{db::TrackedFile, FileChange};
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;

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
        let status = self.status(&self.local)?;
        self.print_status(&status);

        Ok(())
    }

    fn remote_status_cmd(&self) -> crate::Result<()> {
        let status = self.status(&self.remote)?;
        self.print_status(&status);

        Ok(())
    }

    fn pull_cmd(&self, yes: bool) -> crate::Result<()> {
        self.push_pull_cmd_helper(&self.remote, &self.local, yes, |rel, theirs, ours| {
            // If our file was deleted, we put there whatever remote has done ever since
            if ours == FileChange::Deleted {
                return true;
            }
            
            let (mut all_yes_change, mut all_no_change) = (false, false);
            let (mut all_yes_del, mut all_no_del) = (false, false);
            match theirs {
                FileChange::New | FileChange::Modified => {
                    Self::prompter_yes_no_all(
                        &format!("Remote file {} was changed, but the corresponding local file has un-pushed changes. Overwrite local file?", rel), 
                        &mut all_yes_change, &mut all_no_change
                    )
                },
                FileChange::Deleted => {
                    Self::prompter_yes_no_all(
                        &format!("Remote file {} was deleted, but the corresponding local file has un-pushed changes. Delete local file?", rel), 
                        &mut all_yes_del, &mut all_no_del
                    )
                }
            }
        })
    }

    fn push_cmd(&self, yes: bool) -> crate::Result<()> {
        self.push_pull_cmd_helper(&self.local, &self.remote, yes, |rel, ours, theirs| {
            // If the file was deleted remotely, there isn't really a conflict; we can just push our file and
            // be done.
            if theirs == FileChange::Deleted {
                return true;
            }

            // The annoying part: we need to cusotmize the messages depending on whether we push or pull
            let (mut all_yes_change, mut all_no_change) = (false, false);
            let (mut all_yes_del, mut all_no_del) = (false, false);
            match ours {
                FileChange::New | FileChange::Modified => {
                    Self::prompter_yes_no_all(
                        &format!("Local file {} was changed, but the corresponding remote file was also changed since the last pull. Overwrite remote file?", rel), 
                        &mut all_yes_change, &mut all_no_change
                    )
                },
                FileChange::Deleted => {
                    Self::prompter_yes_no_all(
                        &format!("Local file {} was deleted, but the corresponding remote file was changed since the last pull. Delte remote file?", rel), 
                        &mut all_yes_del, &mut all_no_del
                    )
                }
            }
        })
    }
}

impl<L: FileRepository, R: FileRepository> DefaultApp<L, R> {
    fn prompter_yes_no_all(q: &str, all_yes: &mut bool, all_no: &mut bool) -> bool {
        if *all_yes {
            return true;
        }

        if *all_no {
            return false;
        }

        loop {
            println!("{} [y - Yes, y! - Yes all, n - No, n! - No all]", q);
            print!("> ");
            std::io::stdout().flush().unwrap();

            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            let line = line.trim();
    
            match line {
                "y" => return true,
                "n" => return false,
                "y!" => {
                    *all_yes = true;
                    return true;
                }
                "n!" => {
                    *all_no = true;
                    return false;
                }
                _ => continue
            };
        }
    }

    fn prompter(q: &str) -> bool {
        loop {
            println!("{} [y - Yes, n - No]", q);
            print!("> ");
            std::io::stdout().flush().unwrap();

            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            let line = line.trim();
    
            match line {
                "y" => return true,
                "n" => return false,
                _ => continue
            };
        }
    }

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
    fn status<RD: crate::walker::ReadDir>(
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

    fn push_pull_cmd_helper<
        A: FileRepository,
        B: FileRepository,
        F: FnMut(&str, FileChange, FileChange) -> bool,
    >(
        &self,
        from: &A,
        to: &B,
        yes: bool,
        mut resolver: F,
    ) -> crate::Result<()> {
        // Query the changes in from. We
        let mut from_status = self.status(from)?.into_iter().collect::<Vec<_>>();
        from_status.sort();

        // If the user wants to not check for conflicts, we do not even need the remote status
        let to_status = if yes {
            HashMap::new()
        } else {
            self.status(to)?
        };

        // We could probably save this allocation, but realistically, what would that accomplish.
        let mut to_be_commited = vec![];
        for (relative_name, our_change) in from_status {
            if let Some(&their_change) = to_status.get(&relative_name) {
                if resolver(&relative_name, our_change, their_change) {
                    to_be_commited.push((relative_name, our_change));
                }
            } else {
                to_be_commited.push((relative_name, our_change));
            }
        }

        if to_be_commited.len() == 0 {
            println!("Already up to date. Nothing was changed.");
            return Ok(());
        }

        // Commit these changes?
        println!("{:?}", to_be_commited);
        if Self::prompter("Commit these changes?") {
            self.push(from, to, to_be_commited, true)?
        } else {
            println!("Aborting");
        }
        

        Ok(())
    }

    /// Given a list of fileChanges, pushes these changes from A to B.
    /// If you think about it, both push and pull can be implemented from this.
    fn push<
        A: FileRepository,
        B: FileRepository,
        I: IntoIterator<Item = (String, FileChange)>,
    >(
        &self,
        from: &A,
        to: &B,
        changes: I,
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
                    let info = from.read_file(&relative_name, &mut buffer)?;
                    to.write_file(&mut buffer.as_slice(), &relative_name, Some(info.modified))?;

                    if track {
                        self.track_current_version(&relative_name)?;
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
