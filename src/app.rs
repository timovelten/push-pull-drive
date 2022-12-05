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
use futures::StreamExt;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::runtime::{Runtime, self};
use tokio::task::spawn_blocking;

use crate::backend::FileRepository;
use crate::progress::{ProgressReporter, AsyncReadWithProgress};
use crate::{db, App, Error, PathInfo, append_to_relative_name};
use crate::{db::TrackedFile, FileChange};
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;

pub struct DefaultApp<L, R> {
    rt: Runtime,
    
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
            rt: runtime::Builder::new_multi_thread()
                .enable_all().build().unwrap(),
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
        let status = self.rt.block_on(async {
            self.status(&self.local).await
        })?;
        self.print_status(&status);

        Ok(())
    }

    fn remote_status_cmd(&self) -> crate::Result<()> {
        let status = self.rt.block_on(async {
            self.status(&self.remote).await
        })?;
        self.print_status(&status);

        Ok(())
    }

    fn pull_cmd(&self, yes: bool) -> crate::Result<()> {
        self.push_pull_cmd_helper(&self.remote, &self.local, yes, |theirs, ours| {
            // If our file was deleted, we put there whatever remote has done ever since
            if ours.is_deleted() {
                return true;
            }
            
            let (mut all_yes_change, mut all_no_change) = (false, false);
            let (mut all_yes_del, mut all_no_del) = (false, false);
            match theirs {
                FileChange::New(_) | FileChange::Modified(_) => {
                    Self::prompter_yes_no_all(
                        &format!("Remote file {} was changed, but the corresponding local file has un-pushed changes. Overwrite local file?", ours.relative_name()), 
                        &mut all_yes_change, &mut all_no_change
                    )
                },
                FileChange::Deleted {
                    relative_name: _
                } => {
                    Self::prompter_yes_no_all(
                        &format!("Remote file {} was deleted, but the corresponding local file has un-pushed changes. Delete local file?", ours.relative_name()), 
                        &mut all_yes_del, &mut all_no_del
                    )
                }
            }
        })
    }

    fn push_cmd(&self, yes: bool) -> crate::Result<()> {
        self.push_pull_cmd_helper(&self.local, &self.remote, yes, |ours, theirs| {
            // If the file was deleted remotely, there isn't really a conflict; we can just push our file and
            // be done.
            if theirs.is_deleted() {
                return true;
            }

            // The annoying part: we need to cusotmize the messages depending on whether we push or pull
            let (mut all_yes_change, mut all_no_change) = (false, false);
            let (mut all_yes_del, mut all_no_del) = (false, false);
            match ours {
                FileChange::New(info) | FileChange::Modified(info) => {
                    Self::prompter_yes_no_all(
                        &format!("Local file {} was changed, but the corresponding remote file was also changed since the last pull. Overwrite remote file?", info.relative_name), 
                        &mut all_yes_change, &mut all_no_change
                    )
                },
                FileChange::Deleted {
                    relative_name
                } => {
                    Self::prompter_yes_no_all(
                        &format!("Local file {} was deleted, but the corresponding remote file was changed since the last pull. Delete remote file?", relative_name), 
                        &mut all_yes_del, &mut all_no_del
                    )
                }
            }
        })
    }

    fn remote_ls(&self, path: Option<String>) -> crate::Result<()> {
        let path = append_to_relative_name(&self.cwd, path.as_ref().map(|x| x.as_str()).unwrap_or(""));

        let dirs = self.rt.block_on(async {
            self.remote.read_dir(&path).await
        })?;
        for file in dirs {
            println!("{} [{:?}]", file.relative_name(), file);
        }

        Ok(())
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

        println!("{}", q);
        loop {
            print!("[y - Yes, y! - Yes all, n - No, n! - No all] > ");
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
        println!("{}", q);
        loop {
            print!("[y - Yes, n - No] > ");
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

    async fn track_current_version(&self, relative_name: &str) -> crate::Result<()> {
        let path_info = self.local.info(relative_name).await?;

        if let PathInfo::File(info) = path_info {
            let tf = TrackedFile {
                relative_name: info.relative_name,
                modified: info.modified,
                size: info.size,
                hash: None,
            };

            // TODO: Calculate hashes
            let db = self.db.clone();
            spawn_blocking(move || db.update_tracked_file(tf)).await.unwrap()?;

            Ok(())
        } else {
            panic!("tried to track the current version of a directory");
        }
    }

    // Determines how files have changed when comapared to another directory structure.
    async fn status<T: FileRepository>(
        &self,
        repo: &T,
    ) -> crate::Result<StatusReport> {
        let mut ret: Vec<FileChange> = Vec::new();

        let tfs = self
            .db
            .get_all_tracked_files(&self.cwd)?
            .into_iter()
            .map(|tf| (tf.relative_name.to_owned(), tf))
            .collect::<HashMap<_, _>>();

        let mut all_files_in_repo = HashSet::new();

        // Walking the file structure is really not that complicated, since we
        // do not really care about order
        let mut next_directories = vec![self.cwd.clone()];

        while next_directories.len() > 0 {
            let files = repo.read_dirs(next_directories).await?;
            next_directories = Vec::new();

            for path_info in files {
                match path_info {
                    PathInfo::Dir(dir) => next_directories.push(dir.relative_name),
                    PathInfo::File(info) => {
                        all_files_in_repo.insert(info.relative_name.clone());

                        if let Some(tf) = tfs.get(&info.relative_name) {
                            debug_assert_eq!(tf.relative_name, info.relative_name);
        
                            // This file exists in the remote, and we track it. So check if it was modified.
                            if info.is_newer_than(&tf.clone().into()) {
                                ret.push(FileChange::Modified(info));
                            }
                        } else {
                            // Remote file is new
                            ret.push(FileChange::New(info));
                        }
                    }
                }
            }
        }

        // Finally, use the list of files we built to determine which of tracked files were deleted
        for (_, tf) in tfs {
            if !all_files_in_repo.contains(&tf.relative_name) {
                ret.push(FileChange::Deleted { relative_name: tf.relative_name.clone() });
            }
        }

        Ok(StatusReport::new(ret))
    }

    fn push_pull_cmd_helper<
        A: FileRepository,
        B: FileRepository,
        F: FnMut(FileChange, FileChange) -> bool,
    >(
        &self,
        from: &A,
        to: &B,
        yes: bool,
        mut resolver: F,
    ) -> crate::Result<()> {
        // Query the changes in from. We
        let from_status = self.rt.block_on(async {
            self.status(from).await
        })?;

        // If the user wants to not check for conflicts, we do not even need the remote status
        let to_status = if yes {
            StatusReport::new(Vec::new())
        } else {
            self.rt.block_on(async {
                self.status(to).await
            })?
        };

        // We could probably save this allocation, but realistically, what would that accomplish.
        let mut to_be_commited = Vec::new();
        for our_change in from_status.into_inner() {
            if let Some(their_change) = to_status.get(our_change.relative_name().as_str()) {
                if resolver(our_change.clone(), their_change.clone()) {
                    to_be_commited.push(our_change);
                }
            } else {
                to_be_commited.push(our_change);
            }
        }

        if to_be_commited.len() == 0 {
            println!("Already up to date. Nothing was changed.");
            return Ok(());
        }

        // Commit these changes?
        for ch in &to_be_commited {
            let s = match ch {
                FileChange::New(_) => "New",
                FileChange::Modified(_) => "Modified",
                FileChange::Deleted { relative_name:_ } => "Deleted",
            };
            println!("{}: {}", s, ch.relative_name());
        }

        if Self::prompter("Commit these changes?") {
            self.rt.block_on(async {
                self.push(from, to, to_be_commited, true).await
            })?
        } else {
            println!("Aborting");
        }
        

        Ok(())
    }

    /// Given a list of fileChanges, pushes these changes from A to B.
    /// If you think about it, both push and pull can be implemented from this.
    async fn push<
        A: FileRepository,
        B: FileRepository,
    >(
        &self,
        from: &A,
        to: &B,
        changes: Vec<FileChange>,
        track: bool,
    ) -> crate::Result<()> {
        let mut fts = Vec::new();

        let multi = MultiProgress::new();
        let style = ProgressStyle::with_template(
                "{bar:20.cyan/blue} {bytes}/{total_bytes} : {msg}",
            ).unwrap()
            .progress_chars("##-");

        struct ReportProgressToProgressBar {
            multi: MultiProgress,
            pb: ProgressBar
        }

        impl ProgressReporter for ReportProgressToProgressBar {
            fn report(&self, x: usize) {
                self.pb.set_position(x as u64);
            }

            fn finish(&self) {
                self.multi.remove(&self.pb);
            }
        }

        for change in changes {

            let multi = multi.clone();
            let style = style.clone();
            let ft = async move {
                    match change {
                    FileChange::New(info) | FileChange::Modified(info) => {
                        // For now, we just download the entire file into memory.
                        // There are worse things...
                        let fileinfo = if let PathInfo::File(info) = from.info(&info.relative_name).await? {
                            info
                        } else {
                            return Err(Error::NotAFile(info.relative_name));
                        };

                        let (reader, len) = from.read_file(&info.relative_name).await?;

                        // Yay, progress reporting
                        let pb = ProgressBar::new(len)
                            .with_style(style)
                            .with_message(info.relative_name.clone());

                        let pb = multi.add(pb);

                        let reporter = ReportProgressToProgressBar {
                            multi, pb
                        };

                        let reader_with_progress = AsyncReadWithProgress::new(reader, reporter);

                        to.write_file(&info.relative_name, reader_with_progress, len).await?;
                        to.set_modified_time(&info.relative_name, fileinfo.modified).await?;

                        if track {
                            self.track_current_version(&info.relative_name).await?;
                        }
                    }
                    FileChange::Deleted {
                        relative_name
                    } => {
                        to.delete_file(&relative_name).await?;

                        if track {
                            self.db.delete_tracked_file(&relative_name)?;
                        }
                    }
                }

                Ok(())
            };

            fts.push(ft);
        }

        // Wait for all of the futures to finish
        let mut fts_stream = futures::stream::iter(fts).buffer_unordered(10);
        while let Some(_) = fts_stream.next().await {}

        Ok(())
    }

    fn print_status(&self, changes: &StatusReport) {
        for change in changes.inner().iter() {
            match change {
                FileChange::New(info) => println!("New: {}", info.relative_name),
                FileChange::Modified(info) => println!("Modified: {}", info.relative_name),
                FileChange::Deleted { relative_name } => println!("Deleted: {}", relative_name), 
            }
        }
    }
}

/// A datastructure holding a list of `FileChanges` in a way that makes it at least somehwat
/// efficient to check if a certain relative name is contained inside.
/// In other words, this is a thin wrapper around Vec<FileChange> which is enforced to always
/// be sorted by the relative name and `get` uses binary search.
struct StatusReport {
    inner: Vec<FileChange>
}

impl StatusReport {
    pub fn new(mut v: Vec<FileChange>) -> Self {
        v.sort_by(|x, y| x.relative_name().cmp(&y.relative_name()));

        Self {
            inner: v
        }
    }

    fn inner(&self) -> &Vec<FileChange> {
        &self.inner
    }

    pub fn into_inner(self) -> Vec<FileChange> {
        self.inner
    } 

    pub fn get<S: AsRef<str>>(&self, relative_name: S) -> Option<&FileChange> {
        let rel_name = relative_name.as_ref();
        let idx = self.inner.binary_search_by(|x| x.relative_name().as_str().cmp(rel_name))
            .ok()?;
        Some(&self.inner[idx])
    }
}

