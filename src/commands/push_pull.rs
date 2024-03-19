use std::{
    any::Any,
    sync::{Arc, Mutex},
};

use super::status::status_of_dir;
use crate::{
    context::Context,
    db::Side,
    file_repository::FileRepository,
    paths,
    progress::{AsyncReadProgress, AsyncReadWithProgress},
    FileChange, FileInfo,
};
use crate::{db::TrackedFilesRepository, file_repository::Error as FileRepositoryError};

use futures::{stream::FuturesUnordered, StreamExt};
use prodash::{tree::Root, Count, NestedProgress, Progress};

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
    let flow = PushPullFlow {
        tracked_files: ctx.tracked_files_or_ignore(args.ignore_index),
        from: ctx.local(),
        to: ctx.remote(),
        wd: ctx.wd(),
        from_side: Side::Local,
        progress: Arc::new(Mutex::new(progress)),
    };
    let (mut changes, conflicts) = flow.prepare().await?;
    // TODO: Prompt the user here if the conflicting changes are okay...
    changes.extend(conflicts.into_iter().map(|x| x.proposed));

    flow.commit(&changes).await?;

    Ok(())
}

pub async fn pull(ctx: &impl Context, args: PushPullArgs) -> anyhow::Result<()> {
    let ctx = ctx.get();
    let (progress, _drop_later) = setup_progress("pull");
    let flow = PushPullFlow {
        tracked_files: ctx.tracked_files_or_ignore(args.ignore_index),
        from: ctx.remote(),
        to: ctx.local(),
        wd: ctx.wd(),
        from_side: Side::Remote,
        progress: Arc::new(Mutex::new(progress)),
    };
    let (mut changes, conflicts) = flow.prepare().await?;
    // TODO: Prompt the user here if the conflicting changes are okay...
    changes.extend(conflicts.into_iter().map(|x| x.proposed));

    flow.commit(&changes).await?;

    Ok(())
}

#[allow(dead_code)]
struct Conflict {
    proposed: FileChange,
    conflicting: FileChange,
}

struct PushPullFlow<'a, T: ?Sized, A, B, P> {
    tracked_files: &'a T,
    from: &'a A,
    to: &'a B,
    wd: &'a str,
    from_side: Side,
    progress: Arc<Mutex<P>>,
}

impl<'a, T, A, B, P> PushPullFlow<'a, T, A, B, P>
where
    A: FileRepository,
    B: FileRepository,
    T: TrackedFilesRepository + ?Sized,
    P: NestedProgress,
    P::SubProgress: 'static,
{
    async fn prepare(&self) -> anyhow::Result<(Vec<FileChange>, Vec<Conflict>)> {
        // Query the changes in from. We
        let from_changes =
            status_of_dir(self.tracked_files, self.from, self.wd, self.from_side).await?;
        // FIXME: This could be somewhat more efficient. We could try to only determine the status
        // of those remote directories that actually have local changes
        let to_changes =
            match status_of_dir(self.tracked_files, self.to, self.wd, !self.from_side).await {
                Ok(s) => s,
                Err(err) => match err.downcast_ref::<FileRepositoryError>() {
                    // If the remote directory does not exist, that is fine as well.
                    Some(FileRepositoryError::NotFound { cpath }) if cpath == self.wd => {
                        Default::default()
                    }
                    _ => return Err(err),
                },
            };

        let mut changes = Vec::new();
        let mut conflicting = Vec::new();
        for our_change in from_changes {
            // TODO: Benchmark with a few thousand changes how long this linear search actually takes
            // and if we need to replace it with a hash set, or a sorted vector...
            let conflicting_change = to_changes
                .iter()
                .filter(|&x| x.cpath() == our_change.cpath())
                .next();

            if let Some(their_change) = conflicting_change {
                conflicting.push(Conflict {
                    proposed: our_change,
                    conflicting: their_change.clone(),
                });
            } else {
                changes.push(our_change);
            }
        }

        Ok((changes, conflicting))
    }

    async fn commit(&self, changes: &[FileChange]) -> anyhow::Result<()> {
        self.progress
            .lock()
            .unwrap()
            .init(Some(changes.len()), None);

        let mut change_futures = changes
            .iter()
            .cloned()
            .map(|change| {
                // Each file needs a reference to the progress bar...
                async { self.handle_change(change).await }
            })
            .collect::<FuturesUnordered<_>>();

        // Wait for all of the futures to finish
        let mut errors = Vec::new();
        while let Some(res) = change_futures.next().await {
            if let Err(err) = res {
                errors.push(err);
            }

            self.progress.lock().unwrap().inc();
        }

        if !errors.is_empty() {
            println!("The following errors ocurred:");
            for err in errors {
                eprintln!("{:?}", err);
            }
        }

        Ok(())
    }

    /// Given a single FileChange, handle it however we need to. This is the heart of push_pull_commit.
    async fn handle_change(&self, change: FileChange) -> anyhow::Result<()> {
        match change {
            FileChange::New(FileInfo { cpath, .. })
            | FileChange::Modified(FileInfo { cpath, .. }) => {
                let (from_info, reader) = self.from.read_file(&cpath).await?;
                let progress_name = get_progress_name(&from_info);
                let len = from_info.size;

                let mut child: Option<Box<dyn prodash::Progress>> = None;
                let reporter = |status: AsyncReadProgress| match status {
                    AsyncReadProgress::Starting => {
                        // Once the download has actually started, we start a child for the progress entry.
                        // Note that we rely on the backend rate limiting us here: If all the read_file's run concurrently, we might add
                        // far more than a thousand progress indicators here, which is bad. In any normal situation, the file system backend should
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
                let to_info = self
                    .to
                    .write_file(&cpath, Some(from_info.modified), reader_with_progress, len)
                    .await?;

                // We have just pulled a file from `from` and pushed it to `to`. We therefore need to update both sides.
                self.tracked_files.update(from_info, self.from_side)?;
                self.tracked_files.update(to_info, !self.from_side)?;
            }
            FileChange::Deleted { cpath } => {
                self.to.delete_file(&cpath).await?;
                self.tracked_files.delete(&cpath)?;
            }
        }

        Ok(())
    }
}

/// This function ensures that filenames print "nice enough". In particular,
/// it guards against filenames that contain newlines (I actually had some of those)
fn get_progress_name(info: &FileInfo) -> &str {
    paths::filename(&info.cpath).lines().next().unwrap()
}
