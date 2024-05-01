use super::progress::{make_progress, ProgressSender};
use super::{ConflictResolution, PushPullArgs};
use crate::util::{InspectAsyncReader, WithoutNewlines};
use crate::FileInfo;
use crate::{
    commands::status::status_of_dir,
    context::AppContext,
    db::{Side, TrackedFilesRepository},
    file_repository::FileRepository,
    util::find_common_prefix,
    FileChange,
};
use either::Either;
use futures::stream::FuturesUnordered;
use futures::StreamExt as _;
use std::io::Write;
use std::sync::Arc;

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

// One could imagine that we might add a variant here to also make some changes on the src side (?)
// For now, we do not have that though and all these represent changes we intend to make on the dst side.
#[derive(PartialEq, Clone, Debug)]
pub(super) enum ChangeRequest {
    DeleteFile { cpath: String },
    DeleteDir { cpath: String },
    CreateFile { info: FileInfo },
    UpdateFile { info: FileInfo },
}

impl From<FileChange> for ChangeRequest {
    fn from(value: FileChange) -> Self {
        match value {
            FileChange::New(info) => ChangeRequest::CreateFile { info },
            FileChange::Modified(info) => ChangeRequest::UpdateFile { info },
            FileChange::Deleted { cpath } => ChangeRequest::DeleteFile { cpath },
        }
    }
}

impl std::fmt::Display for ChangeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Note: It is somewhat important that whatever we return contains no line-breaks (so as to not break the asthetic...)
        match self {
            ChangeRequest::DeleteFile { cpath } => {
                write!(f, "Delete {}", WithoutNewlines(cpath))?;
            }
            ChangeRequest::DeleteDir { cpath } => {
                write!(f, "Delete {}", WithoutNewlines(cpath))?;
            }
            ChangeRequest::CreateFile { info } => {
                write!(f, "Create {}", WithoutNewlines(&info.cpath))?;
            }
            ChangeRequest::UpdateFile { info } => {
                write!(f, "Update {}", WithoutNewlines(&info.cpath))?;
            }
        };

        Ok(())
    }
}

enum PushPullState {
    None,
    NothingToDo,
    Prepared {
        changes: Vec<FileChange>,
        conflicts: Vec<Conflict>,
    },
    Commit {
        changes: Vec<ChangeRequest>,
    },
    Done,
}

impl PushPullState {
    fn is_done(&self) -> bool {
        matches!(self, PushPullState::Done)
    }
}

#[derive(Clone)]
pub(super) struct PushPullFlow<A, B> {
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
    pub(super) fn new_pull(local: B, remote: A, args: PushPullArgs, ctx: AppContext) -> Self {
        PushPullFlow {
            args,
            ctx,
            from: remote,
            to: local,
            from_side: Side::Remote,
        }
    }

    pub(super) fn new_push(local: A, remote: B, args: PushPullArgs, ctx: AppContext) -> Self {
        PushPullFlow {
            args,
            ctx,
            from: local,
            to: remote,
            from_side: Side::Local,
        }
    }

    fn stderr(&self) -> impl std::io::Write + '_ {
        self.ctx.shell.err()
    }

    pub(super) fn execute(self) -> anyhow::Result<()> {
        let handle = self.ctx.handle.clone();
        handle.block_on(self.execute_async())
    }

    fn tracked_files(&self) -> &dyn TrackedFilesRepository {
        self.ctx.tracked_files_or_ignore(self.args.ignore_index)
    }

    async fn execute_async(mut self) -> anyhow::Result<()> {
        let mut state = PushPullState::None;

        while !state.is_done() {
            state = self.advance(state).await?;
        }

        Ok(())
    }

    pub(self) async fn advance(&mut self, state: PushPullState) -> anyhow::Result<PushPullState> {
        let next_state = match state {
            PushPullState::None => self.prepare().await?,
            PushPullState::Prepared { changes, conflicts } => {
                self.do_conflict_resolution(changes, conflicts)?
            }
            PushPullState::Commit { changes } => self.commit(changes).await?,
            PushPullState::Done => PushPullState::Done,
            PushPullState::NothingToDo => {
                writeln!(self.ctx.shell.err(), "There is nothing to do.")?;
                PushPullState::Done
            }
        };

        Ok(next_state)
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

    fn do_conflict_resolution(
        &mut self,
        changes: Vec<FileChange>,
        conflicts: Vec<Conflict>,
    ) -> anyhow::Result<PushPullState> {
        let mut changes: Vec<ChangeRequest> = changes.into_iter().map(Into::into).collect();
        let has_conflicts = conflicts.len() > 0;

        for conflict in conflicts {
            // "conservative" for us means to make less changes. So (for now anyway) conservative
            // means to just skip this change.
            match conflict {
                Conflict::BothModified { src, dst: _ } => match self.args.conflict_resolution {
                    ConflictResolution::Interactive => {
                        writeln!(
                            self.stderr(),
                            "Conflict: File {} is modified in both the source and the destination.",
                            src.cpath()
                        )?;
                    }
                    ConflictResolution::Normal | ConflictResolution::Aggressive => {
                        changes.push(src.into());
                    }
                    ConflictResolution::Conservative => continue,
                },
                Conflict::IsADirectory { src, contents: _ } => {
                    match self.args.conflict_resolution {
                        ConflictResolution::Interactive => {
                            writeln!(self.stderr(), "Conflict: {} is a file in the source, and a directory in the destination.", src.cpath())?;
                        }
                        ConflictResolution::Normal => {
                            writeln!(
                                self.stderr(),
                                "Conflict: Consider deleting the directory {} in the destination. For now, this change will be ignored.",
                                src.cpath()
                            )?;
                        }
                        ConflictResolution::Aggressive => {
                            changes.push(ChangeRequest::DeleteDir {
                                cpath: src.cpath().to_owned(),
                            });
                        }
                        ConflictResolution::Conservative => continue,
                    }
                }
                Conflict::SubpathIsFile { src, file } => match self.args.conflict_resolution {
                    ConflictResolution::Interactive => {
                        writeln!(
                            self.stderr(),
                            "Conflict: {} was changed in the source, but {} is a file in the destination",
                            src.cpath(), file.cpath()
                        )?;
                    }
                    ConflictResolution::Normal => {
                        writeln!(
                            self.stderr(),
                            "Conflict: Consider deleting the file {} in the destination. For now, this change will be ignored.",
                            src.cpath()
                        )?;
                    }
                    ConflictResolution::Aggressive => {
                        changes.push(ChangeRequest::DeleteFile {
                            cpath: src.cpath().to_owned(),
                        });
                    }
                    ConflictResolution::Conservative => continue,
                },
            }
        }

        if has_conflicts && self.args.conflict_resolution == ConflictResolution::Interactive {
            todo!("Interactive conflict resolution is not yet implemented. Run again with --conflict-resolution argument");
        }

        Ok(PushPullState::Commit { changes })
    }

    async fn commit_deletions(
        self: Arc<Self>,
        changes: &[ChangeRequest],
        progress: &ProgressSender,
    ) -> anyhow::Result<()> {
        log::info!("Committing deletions");

        // Deletions should be quick, just do them in sequence.
        for (id, change) in changes.iter().enumerate() {
            let ChangeRequest::DeleteFile { cpath } = change else {
                continue;
            };

            self.to.delete_file(cpath).await?;
            self.tracked_files().delete(cpath)?;

            progress.notify_done(id).await;
        }

        Ok(())
    }

    async fn commit_other(
        self: Arc<Self>,
        changes: &[ChangeRequest],
        progress: &ProgressSender,
    ) -> Result<(), Vec<anyhow::Error>> {
        log::info!("Committing modifications and file creations");

        let mut change_futures = FuturesUnordered::new();

        for (id, change) in changes.iter().enumerate() {
            let (ChangeRequest::CreateFile { info } | ChangeRequest::UpdateFile { info }) =
                change.clone()
            else {
                continue;
            };

            let this = self.clone();
            let progress = progress.clone();
            change_futures.push(async move {
                tokio::spawn(async move { this.handle_update(info, id, progress).await })
                    .await
                    .unwrap()
            });
        }

        let mut errors = Vec::new();
        while let Some(res) = change_futures.next().await {
            if let Err(err) = res {
                errors.push(err);
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// The Error returns all errors that have ocurred, and the boolean indicates if the operation was "aborted"
    /// (i.e. not "really" run to completion).
    async fn commit_impl(
        self: Arc<Self>,
        changes: &[ChangeRequest],
        sender: ProgressSender,
    ) -> Result<(), (bool, Vec<anyhow::Error>)> {
        // If the deletions fail, we just bail and do not even try the modifications
        self.clone()
            .commit_deletions(changes, &sender)
            .await
            .map_err(|err| (true, vec![err]))?;

        self.commit_other(changes, &sender)
            .await
            .map_err(|err| (false, err))
    }

    async fn commit(&self, changes: Vec<ChangeRequest>) -> anyhow::Result<PushPullState> {
        let (sender, mut receiver) = make_progress(&changes);
        let this = Arc::new(self.clone());

        // We move the sender here. That is important so that the progress can finish (it only finishes when all senders are dropped)
        let work = this.commit_impl(&changes, sender);

        // Wait for progress and display it in the background, so that we are free to do whatever here.
        // In particular, we want to return early
        let progress = {
            let shell = self.ctx.shell.clone();
            async move {
                // TODO: Is this the right course of action?
                let _ = receiver.run(&shell).await;
                Ok(())
            }
        };

        // Actually do the work!
        let res = futures::try_join!(work, progress);
        if let Err((failed_early, errors)) = res {
            if failed_early {
                writeln!(
                    self.ctx.shell.err(),
                    "Aborted operation because the following errors ocurred:"
                )?;
            } else {
                writeln!(
                    self.ctx.shell.err(),
                    "During the operation, the following errors ocurred:"
                )?;
            }
            for err in errors {
                writeln!(self.ctx.shell.err(), "{:?}", err)?;
            }
        }
        Ok(PushPullState::Done)
    }

    /// Given a single FileChange, handle it however we need to. This is the heart of push_pull_commit.
    async fn handle_update(
        &self,
        FileInfo { cpath, .. }: FileInfo,
        id: usize,
        progress: ProgressSender,
    ) -> anyhow::Result<()> {
        // We might retry this once, if we think that we can fix the error by deleting some empty directories.
        for is_retry in [false, true] {
            if is_retry {
                // If this is our second try, we do any sane thing we can think of to make it suceed after all
                // (which might include "expensive" API calls we do not want to do for every file always)
                log::warn!(
                    "Trying to delete orphaned directory at {} to make change suceed.",
                    &cpath
                );
                self.to.delete_orphaned_directory(&cpath).await?;
            }

            let (from_info, reader) = self.from.read_file(&cpath).await?;
            let len = from_info.size;

            progress.notify_start(id).await;

            let reader = InspectAsyncReader::new(reader, |delta| {
                progress.notify_bytes_read(id, delta as u64);
            });
            let write_result = self
                .to
                .write_file(&cpath, Some(from_info.modified), reader, len)
                .await;

            let to_info = match write_result {
                Ok(info) => info,
                Err(err) if !is_retry => {
                    println!("Got error: {}. Retrying.", err);
                    continue;
                }
                Err(err) => return Err(err.into()),
            };

            // We have just pulled a file from `from` and pushed it to `to`. We therefore need to update both sides.
            self.tracked_files().update(from_info, self.from_side)?;
            self.tracked_files().update(to_info, !self.from_side)?;

            progress.notify_done(id).await;

            break;
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::FileInfo;
    use time::OffsetDateTime;

    fn f(s: &str) -> FileInfo {
        FileInfo {
            cpath: s.to_owned(),
            size: 42,
            modified: OffsetDateTime::now_utc(),
            hash: Default::default(),
        }
    }

    fn test_changes_1() -> Vec<FileChange> {
        // When changing this, it is important to take care not to invalidate all the test that use this.
        let mut ret = vec![
            FileChange::New(f("documents/2024/invoice")),
            FileChange::New(f("documents/2024/letter")),
            FileChange::Modified(f("documents/2024/cv")),
            FileChange::Deleted {
                cpath: "documents/template".to_owned(),
            },
            FileChange::New(f("videos/test/vid.mp4")),
            FileChange::New(f("videos/test/vid2.mkv")),
            FileChange::Modified(f("videos/test/another.mkv")),
            FileChange::Deleted {
                cpath: "tmp/file1".to_owned(),
            },
            FileChange::Deleted {
                cpath: "tmp/file2".to_owned(),
            },
        ];
        sort_filechanges(&mut ret);

        ret
    }

    #[test]
    fn deletion_should_not_conflict_with_modification() {
        let changes = test_changes_1();

        let res = either_okay_or_conflict(FileChange::Modified(f("documents/template")), &changes);
        assert!(matches!(res, Either::Left(_)));
    }

    #[test]
    fn conflict_detection_both_modified() {
        let to_changes = test_changes_1();

        let Either::Right(Conflict::BothModified { dst, .. }) = either_okay_or_conflict(
            FileChange::Modified(f("documents/2024/letter")),
            &to_changes,
        ) else {
            panic!();
        };
        assert_eq!(dst.cpath(), "documents/2024/letter");
    }

    #[test]
    fn conflict_target_is_dir() {
        let changes = test_changes_1();

        let info = f("videos/test");
        for change in [FileChange::Modified(info.clone()), FileChange::New(info)] {
            let Either::Right(Conflict::IsADirectory { contents, .. }) =
                either_okay_or_conflict(change, &changes)
            else {
                panic!();
            };
            assert_eq!(contents.len(), 3);
        }
    }

    #[test]
    fn conflict_target_is_empty_dir() {
        let changes = test_changes_1();

        // This one a is a bit tricky. Clearly, the user was able to create the file "tmp" (even though that
        // was a folder at the point in time we last synced). Unless there were remote changes in that folder
        // the user knowingly deleted the folder, and replaced it by a file. There is no conflict here!
        let Either::Left(_) = either_okay_or_conflict(FileChange::Modified(f("tmp")), &changes)
        else {
            panic!();
        };
    }

    #[test]
    fn conflict_subpath_is_file() {
        let changes = test_changes_1();

        let Either::Right(Conflict::SubpathIsFile { file, .. }) = either_okay_or_conflict(
            FileChange::Modified(f("documents/2024/cv/pictures/coverpic.jpg")),
            &changes,
        ) else {
            panic!()
        };
        assert_eq!(file.cpath(), "documents/2024/cv");
    }
}
