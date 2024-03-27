use super::status::status_of_dir;
use crate::db::TrackedFilesRepository;
use crate::file_repository::Error as FileRepositoryError;
use crate::{
    context::Context,
    db::Side,
    file_repository::FileRepository,
    paths,
    progress::{AsyncReadProgress, AsyncReadWithProgress},
    util::find_common_prefix,
    FileChange, FileInfo,
};
use anyhow::{bail, Context as _};
use either::Either;
use futures::{stream::FuturesUnordered, StreamExt};
use prodash::{tree::Root, Count, NestedProgress, Progress};
use std::{
    any::Any,
    sync::{Arc, Mutex},
};

#[derive(clap::Args)]
pub struct PushPullArgs {
    #[arg(long)]
    pub ignore_index: bool,
    #[arg(long)]
    pub yes: bool,
}

fn setup_progress(name: &str) -> (prodash::tree::Item, impl Any) {
    let root = Root::new();
    let options = prodash::render::line::Options {
        ..Default::default()
    };
    let renderer = prodash::render::line(std::io::stderr(), Arc::<Root>::downgrade(&root), options);
    let item = root.add_child(name);

    // The caller must not drop this if he wants there to be progress...
    let keep_around = (root, renderer);

    (item, keep_around)
}

pub async fn push(ctx: &impl Context, args: PushPullArgs) -> anyhow::Result<()> {
    let ctx = ctx.get();
    let (progress, _drop_later) = setup_progress("push");
    let mut flow = PushPullFlow {
        tracked_files: ctx.tracked_files_or_ignore(args.ignore_index),
        from: ctx.local(),
        to: ctx.remote(),
        wd: ctx.wd(),
        from_side: Side::Local,
        progress: Arc::new(Mutex::new(progress)),
        state: PushPullState::None,
    };
    flow.execute().await?;

    Ok(())
}

pub async fn pull(ctx: &impl Context, args: PushPullArgs) -> anyhow::Result<()> {
    let ctx = ctx.get();
    let (progress, _drop_later) = setup_progress("pull");
    let mut flow = PushPullFlow {
        tracked_files: ctx.tracked_files_or_ignore(args.ignore_index),
        from: ctx.remote(),
        to: ctx.local(),
        wd: ctx.wd(),
        from_side: Side::Remote,
        progress: Arc::new(Mutex::new(progress)),
        state: PushPullState::None,
    };
    flow.execute().await?;

    Ok(())
}

#[allow(dead_code)]
enum Conflict {
    /// The file at src.cpath() == dst.cpath() has been modified in both the target and the source since
    /// the last push / pull.
    BothModified { src: FileChange, dst: FileChange },
    /// The src file a/b/c was changed, but the dst contains changes [a/b/c/d1, a/b/c/d2, ...], i.e. a/b/c is a non-empty directory
    /// in the dst, but it needs to be a file. Note that if a/b/c is an empty directory, we will just delete it in this case so
    /// dst is always non-empty.
    IsADirectory {
        src: FileChange,
        contents: Vec<FileChange>,
    },
    /// The src file a/b/c was changed, but the dest contains a file a/b (i.e. we need a/b to be a directory, but it is a file).
    SubpathIsFile { src: FileChange, file: FileChange },
}

enum PushPullState {
    None,
    NothingToDo,
    Prepared {
        changes: Vec<FileChange>,
        conflicts: Vec<Conflict>,
    },
    Commit {
        changes: Vec<FileChange>,
    },
    Done,
    Transient,
}

struct PushPullFlow<'a, T: ?Sized, A, B, P> {
    tracked_files: &'a T,
    from: &'a A,
    to: &'a B,
    wd: &'a str,
    from_side: Side,
    progress: Arc<Mutex<P>>,
    state: PushPullState,
}

/// Find a common directory that is a prefix of all cpaths in the slice. Note that this panics
/// if the slice is empty.
fn common_cpath_directory_prefix(changes: &[FileChange]) -> &str {
    let mut common_prefix = changes[0].cpath().as_str();
    for change in &changes[1..] {
        let prefix_len = find_common_prefix(common_prefix, change.cpath());
        if prefix_len < common_prefix.len() {
            common_prefix = &change.cpath()[..prefix_len];
        }
    }

    // Make sure that what we return is actually a directory
    match common_prefix.rfind('/') {
        Some(k) => &common_prefix[..k],
        None => "",
    }
}

fn sort_filechanges(changes: &mut [FileChange]) {
    changes.sort_by(|a, b| a.cpath().cmp(b.cpath()))
}

/// Return `Either::Left(from_change)` if there are no conflicts, or `Either::Right(...)` and the conflict that was found.
/// This function assumes that to_changes is sorted by cpath (as it uses binary search internally).
/// Use [`sort_filechanges`].
fn either_okay_or_conflict(
    from_change: FileChange,
    to_changes: &[FileChange],
) -> Either<FileChange, Conflict> {
    match to_changes.binary_search_by(|probe| probe.cpath().cmp(from_change.cpath())) {
        Ok(i) => {
            let their_change = &to_changes[i];
            if !their_change.is_deleted() {
                return Either::Right(Conflict::BothModified {
                    src: from_change,
                    dst: their_change.clone(),
                });
            }
        }
        Err(i) => {
            // Due to the way binary search works, to_changes[i].cpath() must be strictly larger than from_change.cpath().

            // Check if from_change is the prefix of any change in the destination (i.e. if from_change.cpath() is a directory in the target
            // that has changes).
            let mut prefixes = Vec::new();

            #[allow(clippy::needless_range_loop)]
            for j in i..to_changes.len() {
                let their_change = &to_changes[j];
                if their_change.cpath().starts_with(from_change.cpath()) {
                    if !their_change.is_deleted() {
                        prefixes.push(their_change.clone())
                    }
                } else {
                    // The list is sorted...
                    break;
                }
            }

            if !prefixes.is_empty() {
                return Either::Right(Conflict::IsADirectory {
                    src: from_change,
                    contents: prefixes,
                });
            }

            // Check if from_change is the suffix of any change in the destination (i.e. a subpath of from_change.cpath()
            // is contained in to_changes)
            if i > 0 && from_change.cpath().starts_with(to_changes[i - 1].cpath()) {
                return Either::Right(Conflict::SubpathIsFile {
                    src: from_change,
                    file: to_changes[i - 1].clone(),
                });
            }
        }
    };

    Either::Left(from_change)
}

impl<'a, T, A, B, P> PushPullFlow<'a, T, A, B, P>
where
    A: FileRepository,
    B: FileRepository,
    T: TrackedFilesRepository + ?Sized,
    P: NestedProgress,
    P::SubProgress: 'static,
{
    async fn execute(&mut self) -> anyhow::Result<()> {
        loop {
            match &self.state {
                PushPullState::None => self.prepare().await?,
                PushPullState::Prepared { .. } => self.do_conflict_resolution().await?,
                PushPullState::Commit { .. } => self.commit().await?,
                PushPullState::Transient => panic!("Transient state should never be observed."),
                PushPullState::Done => {
                    break;
                }
                PushPullState::NothingToDo => {
                    println!("There is nothing to do.");
                    self.state = PushPullState::Done;
                }
            }
        }

        Ok(())
    }
    async fn prepare(&mut self) -> anyhow::Result<()> {
        // check what has changed on the from side. These are the only files we (may) touch on the destination side.i
        // (Except possibly for directories that contain no files, we may silently delete those).
        let (_, from_changes) =
            status_of_dir(self.tracked_files, self.from, self.wd, self.from_side).await?;

        if from_changes.is_empty() {
            self.state = PushPullState::NothingToDo;
            return Ok(());
        }

        // Optimization for a somewhat common case: If only files from a certain subdirectory were changed,
        // find that subdirectory. This potentially allows us to not list less remote files, which is good.
        let better_wd = common_cpath_directory_prefix(&from_changes);
        log::debug!("Using optimized working directory {better_wd} for querying remote");

        // Determine all files in the "better working directory" and check if they have changed
        let (_, mut to_changes) =
            status_of_dir(self.tracked_files, self.to, better_wd, !self.from_side).await?;

        // find_conflicts requires to_changes to be sorted.
        sort_filechanges(&mut to_changes);

        let mut changes = Vec::new();
        let mut conflicts = Vec::new();
        for our_change in from_changes {
            match either_okay_or_conflict(our_change, &to_changes) {
                Either::Left(change) => changes.push(change),
                Either::Right(conflict) => conflicts.push(conflict),
            }
        }

        self.state = PushPullState::Prepared { changes, conflicts };

        Ok(())
    }

    async fn do_conflict_resolution(&mut self) -> anyhow::Result<()> {
        let state = std::mem::replace(&mut self.state, PushPullState::Transient);
        let PushPullState::Prepared {
            changes,
            conflicts: _conflicts,
        } = state
        else {
            panic!();
        };

        // For now, our conflict resolution is to not do it...
        self.state = PushPullState::Commit { changes };

        Ok(())
    }

    async fn commit(&mut self) -> anyhow::Result<()> {
        let PushPullState::Commit { changes } = &self.state else {
            bail!("Wrong state");
        };

        self.progress
            .lock()
            .unwrap()
            .init(Some(changes.len()), None);

        let mut deletion_futures = FuturesUnordered::new();
        let mut modification_futures = FuturesUnordered::new();

        for change in changes.iter().cloned() {
            match &change {
                FileChange::Deleted { .. } => {
                    deletion_futures.push(async { self.handle_change(change).await });
                }
                _ => {
                    modification_futures.push(async { self.handle_change(change).await });
                }
            }
        }

        // Start handling all the futures, deletions first (that might be necessary so that the change futures can suceed!). Collect the errors to display them.
        let mut errors = Vec::new();

        while let Some(res) = deletion_futures.next().await {
            if let Err(err) = res {
                errors.push(err);
            }

            self.progress.lock().unwrap().inc();
        }
        drop(deletion_futures);

        while let Some(res) = modification_futures.next().await {
            if let Err(err) = res {
                errors.push(err);
            }

            self.progress.lock().unwrap().inc();
        }
        drop(modification_futures);

        // Display the collected errors (this really could be a lot better...)
        if !errors.is_empty() {
            println!("The following errors ocurred:");
            for err in errors {
                eprintln!("{:?}", err);
            }
        }

        self.state = PushPullState::Done;
        Ok(())
    }

    /// Given a single FileChange, handle it however we need to. This is the heart of push_pull_commit.
    async fn handle_change(&self, change: FileChange) -> anyhow::Result<()> {
        loop {
            match &change {
                FileChange::New(FileInfo { cpath, .. })
                | FileChange::Modified(FileInfo { cpath, .. }) => {
                    let (from_info, reader) = self.from.read_file(cpath).await?;
                    let progress_name = get_progress_name(&from_info);
                    let len = from_info.size;

                    let mut child: Option<Box<dyn prodash::Progress>> = None;
                    let reporter = |status: AsyncReadProgress| match status {
                        AsyncReadProgress::Starting => {
                            // Once the download has actually started, we start a child for the progress entry.
                            // Note that we rely on the backend rate limiting us here: If all the read_file's run concurrently, we might add
                            // far more than a thousand progress indicators here, which is bad. In any normal situation, the file system backend should
                            // also limit this...
                            let mut c =
                                Box::new(self.progress.lock().unwrap().add_child(progress_name));
                            c.init(Some(len as usize), None);
                            child = Some(c);
                        }
                        AsyncReadProgress::InProgress { bytes }
                        | AsyncReadProgress::Finished { bytes } => {
                            let Some(child) = &child else { return };
                            child.set(bytes);
                        }
                    };

                    let reader_with_progress = AsyncReadWithProgress::new(reader, reporter);
                    let write_result = self
                        .to
                        .write_file(cpath, Some(from_info.modified), reader_with_progress, len)
                        .await;

                    let to_info = match write_result {
                        Ok(info) => info,
                        Err(err @ FileRepositoryError::IsADirectory { .. }) => {
                            let FileRepositoryError::IsADirectory { cpath: err_cpath } = &err
                            else {
                                unreachable!()
                            };
                            if cpath != err_cpath {
                                return Err(err.into());
                            }

                            self.to
                                .delete_orphaned_directory(cpath)
                                .await
                                .with_context(|| "Failed to delete orphaned directory")
                                .context(err)?;

                            continue;
                        }
                        Err(err) => return Err(err.into()),
                    };

                    // We have just pulled a file from `from` and pushed it to `to`. We therefore need to update both sides.
                    self.tracked_files.update(from_info, self.from_side)?;
                    self.tracked_files.update(to_info, !self.from_side)?;
                }
                FileChange::Deleted { cpath } => {
                    self.to.delete_file(cpath).await?;
                    self.tracked_files.delete(cpath)?;
                }
            }

            break;
        }

        Ok(())
    }
}

/// This function ensures that filenames print "nice enough". In particular,
/// it guards against filenames that contain newlines (I actually had some of those)
fn get_progress_name(info: &FileInfo) -> &str {
    paths::filename(&info.cpath).lines().next().unwrap()
}

#[cfg(test)]
mod test {
    use time::OffsetDateTime;

    use super::*;

    fn f(s: &str) -> FileInfo {
        FileInfo {
            cpath: s.to_owned(),
            size: 42,
            modified: OffsetDateTime::now_utc(),
            hash: Default::default(),
        }
    }

    #[test]
    fn test_conflict_detection_ident() {
        let mut to_changes = vec![
            FileChange::New(f("a/b/c1")),
            FileChange::New(f("a/b/c2")),
            FileChange::Modified(f("a/b/c3")),
            FileChange::Deleted {
                cpath: "a/f".to_owned(),
            },
        ];
        sort_filechanges(&mut to_changes);

        let Either::Left(_) = either_okay_or_conflict(FileChange::Modified(f("a/f")), &to_changes)
        else {
            panic!("Deletions should not be a conflict");
        };

        let Either::Right(Conflict::BothModified { dst, .. }) =
            either_okay_or_conflict(FileChange::Modified(f("a/b/c2")), &to_changes)
        else {
            panic!();
        };
        assert_eq!(dst.cpath(), "a/b/c2");
    }

    #[test]
    fn test_conflict_target_is_dir() {
        let mut changes = vec![
            FileChange::Modified(f("a/b/c/f1")),
            FileChange::Modified(f("a/b/c/f2")),
            FileChange::Modified(f("a/b/c/f3")),
            FileChange::Deleted {
                cpath: "b/c1".to_owned(),
            },
            FileChange::Deleted {
                cpath: "b/c2".to_owned(),
            },
        ];
        sort_filechanges(&mut changes);

        let Either::Right(Conflict::IsADirectory { contents, .. }) =
            either_okay_or_conflict(FileChange::Modified(f("a/b")), &changes)
        else {
            panic!("unexpected variant");
        };
        assert_eq!(contents.len(), 3);

        let Either::Left(_) = either_okay_or_conflict(FileChange::Modified(f("b")), &changes)
        else {
            panic!(
                "For our purposes b should be considered empty, and is therefore not a conflict"
            );
        };
    }

    #[test]
    fn test_conflict_target_is_subpathed() {
        let mut changes = vec![
            FileChange::Modified(f("a/b")),
            FileChange::Modified(f("a/c")),
            FileChange::New(f("dummy")),
            FileChange::Deleted {
                cpath: "something/file1".to_owned(),
            },
            FileChange::Deleted {
                cpath: "something/file2".to_owned(),
            },
        ];
        sort_filechanges(&mut changes);

        let Either::Right(Conflict::SubpathIsFile { file, .. }) =
            either_okay_or_conflict(FileChange::Modified(f("a/b/c/d")), &changes)
        else {
            panic!("unexpected variant");
        };
        assert_eq!(file.cpath(), "a/b");

        let Either::Left(_) =
            either_okay_or_conflict(FileChange::Modified(f("something")), &changes)
        else {
            panic!("unexpected variant");
        };
    }
}
