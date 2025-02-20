use crate::{
    context::{Context, Ctx, ExecutionEnvironment},
    file_repository::FileRepository,
    filetree::{self, file_tree_of_repository, Node as _, Patch, PatchKind},
    paths::{RPath, RPathBuf},
    FileInfo, InspectAsyncReader,
};
use anyhow::Error;
use progress::{ProgressId, ProgressSender};
use std::{collections::VecDeque, io::Write as _, sync::Arc, time::Duration, usize};
use tokio::{sync::Semaphore, task::JoinSet};

mod progress;

#[derive(clap::Args, Clone)]
pub struct PushPullArgs {
    path: Option<String>,
}

fn get_path(path: Option<String>, wd: &RPath) -> RPathBuf {
    match path {
        Some(path) => {
            let path = RPathBuf::deserialize_sloppy(&path);
            wd.join(&path)
        }
        None => wd.to_owned(),
    }
}

pub fn push(ctx: &impl Context, args: PushPullArgs) -> anyhow::Result<()> {
    let Ctx { env, local, remote } = ctx.get();
    PushPull {
        env: env.clone(),
        from: local.clone(),
        to: remote.clone(),
        path: get_path(args.path, &env.wd),
    }
    .execute()?;

    Ok(())
}

pub fn pull(ctx: &impl Context, args: PushPullArgs) -> anyhow::Result<()> {
    let Ctx { env, local, remote } = ctx.get();
    PushPull {
        env: env.clone(),
        from: remote.clone(),
        to: local.clone(),
        path: get_path(args.path, &env.wd),
    }
    .execute()?;

    Ok(())
}

#[derive(Clone)]
pub(super) struct PushPull<A, B> {
    env: ExecutionEnvironment,
    from: A,
    to: B,
    path: RPathBuf,
}

impl<A, B> PushPull<A, B>
where
    A: FileRepository,
    B: FileRepository,
{
    pub(super) fn execute(self) -> anyhow::Result<()> {
        let handle = self.env.handle.clone();
        handle.block_on(self.execute_async())
    }

    fn concurrency_limit(&self) -> usize {
        std::cmp::min(A::CONCURRENCY_LIMIT, B::CONCURRENCY_LIMIT)
    }

    async fn transfer_files(
        self: Arc<Self>,
        sender: ProgressSender,
        files: Vec<(RPathBuf, ProgressId)>,
    ) -> Vec<(RPathBuf, Error)> {
        let sem = Arc::new(Semaphore::new(self.concurrency_limit()));

        let mut fts = JoinSet::new();

        for (file, id) in files {
            let sender = sender.clone();
            let this = self.clone();
            let sem = sem.clone();

            fts.spawn(async move {
                let _permit = sem.acquire().await.unwrap();

                sender.notify_start(id);

                let progress_callback = |bytes: usize| sender.notify_bytes_read(id, bytes as u64);
                if let Err(err) = this.transfer_file(&file, progress_callback).await {
                    sender.notify_failed(id);
                    Err((file, err))
                } else {
                    sender.notify_done(id);
                    Ok(())
                }
            });
        }

        let res = fts.join_all().await;
        res.into_iter().filter_map(|res| res.err()).collect()
    }

    /// Try and do everything a patch wants us to do, except actually transfering files.
    /// That is, we record which files need to be transferred, and delete files that need
    /// need to be deleted.
    ///
    /// Note that this does not check if the file is actually modified. We leave that
    /// to somewhere else
    async fn prepare_patch<'a>(
        &self,
        patch: &'a Patch<'_>,
        to_be_updated: &mut Vec<&'a RPath>,
        to_be_created: &mut Vec<&'a RPath>,
    ) -> anyhow::Result<()> {
        match patch.kind {
            PatchKind::Create { terminal: true } => {
                // Creating a file is done by someone eles
                to_be_created.push(patch.path);
            }
            PatchKind::Create { terminal: false } => {
                self.to.mkdir(patch.path).await?;
            }
            PatchKind::Delete { .. } => {
                self.to.delete(patch.path).await?;
            }
            PatchKind::Unchanged { terminal: true } => {
                // If needed, file updates are handled by someone else.
                to_be_updated.push(patch.path);
            }
            PatchKind::Unchanged { terminal: false } => {
                // Directory updates are unimportant to us
            }
            PatchKind::Replace { terminal: true } => {
                // Replace a dir by a file
                self.to.delete(patch.path).await?;
                to_be_created.push(patch.path);
            }
            PatchKind::Replace { terminal: false } => {
                // Replace a file by a directory
                self.to.delete(patch.path).await?;
                self.to.mkdir(patch.path).await?;
            }
        }

        Ok(())
    }

    async fn transfer_file<F>(&self, path: &RPath, progress: F) -> anyhow::Result<FileInfo>
    where
        F: Fn(usize) + Sync,
    {
        let (info, reader) = self.from.read_file(path).await?;
        let reader = InspectAsyncReader::new(reader, |buf| progress(buf.len()));

        let info = self
            .to
            .write_file(path, Some(info.modified), reader, info.size)
            .await?;

        Ok(info)
    }

    /// Look and from and to, and determine which files need to be transferred.
    /// This also performs deletions and creates directories that are necessary for these file transfers
    /// to succeed.
    ///
    /// If a deletion or a directory creation fails, then the files are NOT returned in the
    /// list of files that should be transferred.
    ///
    /// That is, to the best of our knowledge, the files we return can actually be transferred without
    /// any further conflicts.
    async fn prepare_transfer(&self) -> anyhow::Result<Vec<RPathBuf>> {
        writeln!(self.env.err(), "Reading 'from' files...").unwrap();
        let from_files = file_tree_of_repository(&self.from, &self.path).await?;

        writeln!(self.env.err(), "Reading 'to' files...").unwrap();
        let to_files = file_tree_of_repository(&self.to, &self.path).await?;

        let patches = filetree::diff(&to_files, &from_files);

        let mut to_be_created = Vec::new();
        let mut to_be_updated = Vec::new();
        let mut early_failures = Vec::new();

        // Go through what we have to do in BFS order, skipping over entire branches if a parent operation fails.
        let _ = self
            .env
            .print_transient("Preparing directory structures...");

        let mut queue: VecDeque<&RPath> = vec![RPath::empty()].into();
        while let Some(path) = queue.pop_front() {
            let patch = patches.get(path).expect("path is a node in the tree");
            debug_assert_eq!(patch.path, path);

            // Execute the operation associated with this patch
            match self
                .prepare_patch(patch, &mut to_be_updated, &mut to_be_created)
                .await
            {
                Ok(b) => b,
                Err(err) => {
                    early_failures.push((patch, err));
                    continue;
                }
            };

            let children = patches
                .children(path)
                .expect("path is a node in the tree")
                .map(|x| x.path())
                .collect::<Vec<_>>();

            queue.extend(children);
        }

        if !early_failures.is_empty() {
            writeln!(self.env.err(), "The following errors occurred:").unwrap();

            for (patch, err) in early_failures {
                writeln!(self.env.err(), "{:?}: {:?}", patch.path(), err).unwrap()
            }
        }

        // Prune the files that need to be updated, leaving out the ones that are up to date
        let to_be_updated = to_be_updated
            .into_iter()
            .filter(|path| {
                let from_info = from_files.get(path).unwrap().unwrap_fileinfo();
                let to_info = to_files.get(path).unwrap().unwrap_fileinfo();

                let unchanged = from_info.size == to_info.size
                    && (from_info.modified - to_info.modified) < Duration::from_secs(1);

                if unchanged {
                    log::debug!("{:?} is unchanged", path);
                }
                !unchanged
            })
            .collect::<Vec<_>>();

        let ret: Vec<_> = to_be_updated
            .into_iter()
            .chain(to_be_created.into_iter())
            .map(|f| f.to_owned())
            .collect();

        Ok(ret)
    }

    async fn execute_async(self) -> anyhow::Result<()> {
        let to_be_transferred = self.prepare_transfer().await?;

        let (sender, mut receiver) = progress::make();

        // Associate each file with an id so that it can tell us about its progress
        let to_be_transferred: Vec<_> = to_be_transferred
            .into_iter()
            .map(|f| (f.clone(), receiver.add_item(f)))
            .collect();

        // Actually transfer files. Note that the sender is moved below, so the sender will be
        // dropped when transfer_files finishes
        let this = Arc::new(self.clone());
        let transfer_task =
            tokio::spawn(async move { this.transfer_files(sender, to_be_transferred).await });

        // Progress task ends when all senders are dropped
        receiver.run(&self.env.shell).await.unwrap();

        let errors = transfer_task.await.unwrap();
        if !errors.is_empty() {
            writeln!(self.env.err(), "The following errors occurred:").unwrap();

            for (path, err) in errors {
                writeln!(self.env.err(), "{path:?}: {err:?}").unwrap()
            }
        }

        Ok(())
    }
}
