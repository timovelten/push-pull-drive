mod progress;

use self::progress::{make_progress, ProgressSender};

use super::status::status_of_dir;
use crate::context::{AppContext, FullContext};
use crate::db::TrackedFilesRepository;
use crate::file_repository::Error as FileRepositoryError;
use crate::util::InspectAsyncReader;
use crate::{
    context::Ctx, db::Side, file_repository::FileRepository, util::find_common_prefix, FileChange,
    FileInfo,
};
use either::Either;
use futures::{stream::FuturesUnordered, StreamExt};
use std::io::Write;
use std::sync::Arc;

#[derive(clap::Args, Clone)]
pub struct PushPullArgs {
    #[arg(long)]
    pub ignore_index: bool,

    #[arg(long)]
    pub yes: bool,
}

pub fn push(ctx: &impl Ctx, args: PushPullArgs) -> anyhow::Result<()> {
    let FullContext { ctx, local, remote } = ctx.get();
    let flow = PushPullFlow {
        args,
        ctx: ctx.clone(),
        from: local.clone(),
        to: remote.clone(),
        from_side: Side::Local,
    };
    flow.execute()?;

    Ok(())
}

pub fn pull(ctx: &impl Ctx, args: PushPullArgs) -> anyhow::Result<()> {
    let FullContext { ctx, local, remote } = ctx.get();
    let flow = PushPullFlow {
        args,
        ctx: ctx.clone(),
        from: remote.clone(),
        to: local.clone(),
        from_side: Side::Remote,
    };
    flow.execute()?;

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
}

#[derive(Clone)]
struct PushPullFlow<A, B> {
    args: PushPullArgs,
    ctx: AppContext,
    from: A,
    to: B,
    from_side: Side,
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

impl<A, B> PushPullFlow<A, B>
where
    A: FileRepository,
    B: FileRepository,
{
    fn execute(self) -> anyhow::Result<()> {
        let handle = self.ctx.handle.clone();
        handle.block_on(self.execute_async())
    }

    fn tracked_files(&self) -> &dyn TrackedFilesRepository {
        self.ctx.tracked_files_or_ignore(self.args.ignore_index)
    }

    async fn execute_async(mut self) -> anyhow::Result<()> {
        let mut state = PushPullState::None;

        loop {
            let next_state = match state {
                PushPullState::None => self.prepare().await?,
                PushPullState::Prepared { changes, conflicts } => {
                    self.do_conflict_resolution(changes, conflicts).await?
                }
                PushPullState::Commit { changes } => self.commit(changes).await?,
                PushPullState::Done => break,
                PushPullState::NothingToDo => {
                    writeln!(self.ctx.shell.err(), "There is nothing to do.")?;
                    PushPullState::Done
                }
            };

            state = next_state;
        }

        Ok(())
    }
    async fn prepare(&mut self) -> anyhow::Result<PushPullState> {
        // check what has changed on the from side. These are the only files we (may) touch on the destination side.i
        // (Except possibly for directories that contain no files, we may silently delete those).
        self.ctx
            .shell
            .print_transient("Determining status of files on 'from' side...")?;
        let (_, from_changes) = status_of_dir(
            self.tracked_files(),
            &self.from,
            &self.ctx.wd,
            self.from_side,
        )
        .await?;

        if from_changes.is_empty() {
            return Ok(PushPullState::NothingToDo);
        }

        // Optimization for a somewhat common case: If only files from a certain subdirectory were changed,
        // find that subdirectory. This potentially allows us to not list less remote files, which is good.
        let better_wd = common_cpath_directory_prefix(&from_changes);
        log::debug!("Using optimized working directory {better_wd} for querying remote..");

        // Determine all files in the "better working directory" and check if they have changed
        self.ctx
            .shell
            .print_transient("Determining status of files on 'to' side")?;
        let (_, mut to_changes) =
            status_of_dir(self.tracked_files(), &self.to, better_wd, !self.from_side).await?;

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

        Ok(PushPullState::Prepared { changes, conflicts })
    }

    async fn do_conflict_resolution(
        &mut self,
        changes: Vec<FileChange>,
        _conflicts: Vec<Conflict>,
    ) -> anyhow::Result<PushPullState> {
        // For now, our conflict resolution is to not do it...
        Ok(PushPullState::Commit { changes })
    }

    async fn commit(&self, changes: Vec<FileChange>) -> anyhow::Result<PushPullState> {
        let mut deletion_futures = FuturesUnordered::new();
        let mut modification_futures = FuturesUnordered::new();
        let (sender, mut receiver) = make_progress(&changes);

        // Lets not duplicate so much data
        let this = Arc::new(self.clone());

        for (idx, change) in changes.iter().cloned().enumerate() {
            let this = this.clone();
            let sender = sender.clone();
            let id = receiver.get_id(idx);

            match &change {
                FileChange::Deleted { .. } => {
                    deletion_futures
                        .push(async move { this.handle_change(change, id, sender).await });
                }
                FileChange::New(..) | FileChange::Modified(..) => modification_futures
                    .push(async move { this.handle_change(change, id, sender).await }),
            }
        }

        // Drop the "original" sender, so that the channel is closed once all of the cloned senders are dropped
        drop(sender);

        let work_future = tokio::spawn(async move {
            let mut errors = Vec::new();

            while let Some(res) = deletion_futures.next().await {
                if let Err(err) = res {
                    errors.push(err);
                }
            }
            while let Some(res) = modification_futures.next().await {
                if let Err(err) = res {
                    errors.push(err);
                }
            }

            drop(deletion_futures);
            drop(modification_futures);

            errors
        });

        receiver.run(&self.ctx.shell).await?;

        // Display the collected errors (this really could be a lot better...)
        let errors = work_future.await.unwrap();
        if !errors.is_empty() {
            writeln!(self.ctx.shell.err(), "The following errors ocurred:")?;
            for err in errors {
                writeln!(self.ctx.shell.err(), "{:?}", err)?;
            }
        }

        Ok(PushPullState::Done)
    }

    /// Given a single FileChange, handle it however we need to. This is the heart of push_pull_commit.
    async fn handle_change(
        &self,
        change: FileChange,
        id: usize,
        progress: ProgressSender,
    ) -> anyhow::Result<()> {
        loop {
            match &change {
                FileChange::New(FileInfo { cpath, .. })
                | FileChange::Modified(FileInfo { cpath, .. }) => {
                    let (from_info, reader) = self.from.read_file(cpath).await?;
                    let len = from_info.size;

                    progress.notify_start(id).await;

                    let reader = InspectAsyncReader::new(reader, |delta| {
                        progress.notify_bytes_read(id, delta as u64);
                    });
                    let write_result = self
                        .to
                        .write_file(cpath, Some(from_info.modified), reader, len)
                        .await;

                    let to_info = match write_result {
                        Ok(info) => info,
                        Err(err) => {
                            let should_retry = Self::should_try_to_delete_orphaned(cpath, &err);
                            if should_retry {
                                continue;
                            } else {
                                return Err(err.into());
                            }
                        }
                    };

                    // We have just pulled a file from `from` and pushed it to `to`. We therefore need to update both sides.
                    self.tracked_files().update(from_info, self.from_side)?;
                    self.tracked_files().update(to_info, !self.from_side)?;

                    progress.notify_done(id).await;
                }
                FileChange::Deleted { cpath } => {
                    self.to.delete_file(cpath).await?;
                    self.tracked_files().delete(cpath)?;

                    progress.notify_done(id).await;
                }
            }

            break;
        }

        Ok(())
    }

    /// Interpret the error returned by FileRepository::write to determine if we should try
    /// to delete orphaned directories and try again.
    fn should_try_to_delete_orphaned(cpath: &str, write_error: &FileRepositoryError) -> bool {
        matches!(write_error, FileRepositoryError::IsADirectory { cpath: err_cpath } if err_cpath == cpath)
    }
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
