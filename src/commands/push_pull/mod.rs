use crate::{
    context::{Context, Ctx, ExecutionEnvironment},
    file_repository::FileRepository,
    filetree::{self, file_tree_of_repository, FileTree, Node as _, Patch, PatchKind},
    paths::{RPath, RPathBuf},
    InspectAsyncReader, PathInfo,
};
use anyhow::{bail, Error};
use progress::{ProgressId, ProgressSender};
use std::{
    cmp::Reverse, collections::VecDeque, fmt::Display, io::Write as _, sync::Arc, time::Duration,
    usize,
};
use tokio::{sync::Semaphore, task::JoinSet};

mod progress;

#[derive(clap::Args, Clone)]
pub struct PushPullArgs {
    /// Limit pull to certain (relative) path
    path: Option<String>,
    #[arg(short, long, default_value = "8")]
    limit_concurrent_transfers: usize,
    /// Answer all prompts with yes
    #[arg(long, default_value = "false")]
    yes: bool,
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
        max_concurrent_transfers: args.limit_concurrent_transfers,

        yes: args.yes,
        from_files: FileTree::empty(),
        to_files: FileTree::empty(),
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
        max_concurrent_transfers: args.limit_concurrent_transfers,

        yes: args.yes,
        from_files: FileTree::empty(),
        to_files: FileTree::empty(),
    }
    .execute()?;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Transfer {
    kind: TransferKind,
    path: RPathBuf,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum TransferKind {
    Update,
    Create,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Operation {
    CreateDirectory(RPathBuf),
    Transfer(Transfer),
    // Sort deletions to the bottom, so that they are visible in long printed lists
    Delete(RPathBuf),
}

impl Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateDirectory(path) => write!(f, "Create directory {path}"),
            Self::Delete(path) => write!(f, "Delete {path}"),
            Self::Transfer(transfer) => write!(f, "{transfer}"),
        }
    }
}

impl Display for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            TransferKind::Update => write!(f, "{}", self.path)?,
            TransferKind::Create => write!(f, "Create: {}", self.path)?,
        }

        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct PushPull<A, B> {
    env: ExecutionEnvironment,
    from: A,
    to: B,
    path: RPathBuf,
    max_concurrent_transfers: usize,

    yes: bool,
    from_files: FileTree<PathInfo>,
    to_files: FileTree<PathInfo>,
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
        A::CONCURRENCY_LIMIT
            .min(B::CONCURRENCY_LIMIT)
            .min(self.max_concurrent_transfers)
    }

    async fn transfer_files(
        self: Arc<Self>,
        sender: ProgressSender,
        mut files: Vec<(Transfer, ProgressId)>,
        mut poisened_paths: Vec<RPathBuf>,
    ) -> Vec<(Transfer, Error)> {
        const LARGE_FILE_SIZE: u64 = 128 * 1024 * 1024;

        poisened_paths.sort();
        let poisened_paths = Arc::new(poisened_paths);

        // Sort by size, larger sizes first. In principle, they should be transferred first
        // (but the order is not strictly deterministic further down, so maybe they wont be)
        files.sort_by_key(|(a, _)| Reverse(a.size));

        let num_permits = self.concurrency_limit();
        let sem = Arc::new(Semaphore::new(num_permits));

        let mut fts = JoinSet::new();

        for (transfer, id) in files {
            let sender = sender.clone();
            let this = self.clone();
            let sem = sem.clone();
            let poisened_paths = poisened_paths.clone();

            fts.spawn(async move {
                // This transfer may depend on directory creation or deletions. If these failed, we skip it
                let poisened = poisened_paths.iter().any(|f| transfer.path.starts_with(&f));
                if poisened {
                    sender.notify_failed(id, "Skipped due to previous error".to_owned());
                }

                // We only really want concurrent file transfers for "small" files that cannot make reasonable
                // use of all the bandwidth without any concurrency. If a file is at least kind of large
                // it can max out the connection by itself, and we should let it do that.
                // It is actually really annoying to wait for 8 large files (say 2 gb) to be transferred
                // concurrently without any observable progress.
                let permits_needed = if transfer.size >= LARGE_FILE_SIZE {
                    num_permits as u32
                } else {
                    1
                };

                let _permit = sem.acquire_many(permits_needed).await.unwrap();

                log::info!("Transferring file {}", transfer.path);

                sender.notify_start(id);

                let progress_callback = |bytes: usize| sender.notify_bytes_read(id, bytes as u64);
                if let Err(err) = this.transfer_file(&transfer, progress_callback).await {
                    sender.notify_failed(id, format!("{err}"));
                    log::error!("File transfer failed: {:?}", err);
                    Err((transfer, err))
                } else {
                    sender.notify_done(id);
                    Ok(())
                }
            });
        }

        let res = fts.join_all().await;
        res.into_iter().filter_map(|res| res.err()).collect()
    }

    async fn transfer_file<F>(&self, transfer: &Transfer, progress: F) -> anyhow::Result<()>
    where
        F: Fn(usize) + Sync,
    {
        let (info, reader) = self.from.read_file(&transfer.path).await?;
        let reader = InspectAsyncReader::new(reader, |buf| progress(buf.len()));

        match transfer.kind {
            TransferKind::Update => {
                self.to.update_file(&info.path, reader, info.size).await?;
                self.to.set_modified_time(&info.path, info.modified).await?;
            }
            TransferKind::Create => {
                self.to.create_file(&info.path, reader, info.size).await?;
                self.to.set_modified_time(&info.path, info.modified).await?;
            }
        };

        Ok(())
    }

    async fn prepare(&mut self) -> anyhow::Result<Vec<Operation>> {
        writeln!(self.env.err(), "Reading 'from' files...").unwrap();
        self.from_files = file_tree_of_repository(&self.from, &self.path).await?;

        writeln!(self.env.err(), "Reading 'to' files...").unwrap();
        self.to_files = file_tree_of_repository(&self.to, &self.path).await?;

        let patches = filetree::diff(&self.to_files, &self.from_files);

        // Go through what we have to do in BFS order, skipping over entire branches if a parent operation fails.
        let _ = self
            .env
            .print_transient("Preparing directory structures...");

        // Use file tress to determine what operations we need to do
        let operations = self.determine_operations(&patches);

        Ok(operations)
    }

    fn determine_operations<'a>(&'a self, diff: &FileTree<Patch<'a>>) -> Vec<Operation> {
        let mut ops = Vec::new();

        let mut queue: VecDeque<&RPath> = vec![RPath::empty()].into();
        while let Some(path) = queue.pop_front() {
            let patch = diff.get(path).expect("path is a node in the tree");
            debug_assert_eq!(patch.path, path);

            match patch.kind {
                PatchKind::Create { terminal: true } => {
                    let from_info = self.from_files.get(&path).unwrap().unwrap_fileinfo();

                    ops.push(Operation::Transfer(Transfer {
                        kind: TransferKind::Create,
                        path: path.to_owned(),
                        size: from_info.size,
                    }));
                }
                PatchKind::Create { terminal: false } => {
                    ops.push(Operation::CreateDirectory(path.to_owned()));
                }
                PatchKind::Delete => {
                    ops.push(Operation::Delete(path.to_owned()));
                }
                PatchKind::Unchanged { terminal: true } => {
                    let from_info = self.from_files.get(&path).unwrap().unwrap_fileinfo();
                    let to_info = self.to_files.get(&path).unwrap().unwrap_fileinfo();

                    // Update a file (at least if it is changed)
                    let unchanged = from_info.size == to_info.size
                        && (from_info.modified - to_info.modified) < Duration::from_secs(1);

                    if !unchanged {
                        ops.push(Operation::Transfer(Transfer {
                            path: path.to_owned(),
                            kind: TransferKind::Update,
                            size: from_info.size,
                        }));
                    } else {
                        log::debug!("{:?} is unchanged", path);
                    }
                }
                PatchKind::Unchanged { terminal: false } => {
                    // Directory updates are unimportant to us
                }
                PatchKind::Replace { terminal: true } => {
                    // Replace a dir by a file
                    let from_info = self.from_files.get(&path).unwrap().unwrap_fileinfo();

                    ops.push(Operation::Delete(path.to_owned()));
                    ops.push(Operation::Transfer(Transfer {
                        kind: TransferKind::Create,
                        path: path.to_owned(),
                        size: from_info.size,
                    }));
                }
                PatchKind::Replace { terminal: false } => {
                    // Replace a file by a directory
                    ops.push(Operation::Delete(path.to_owned()));
                    ops.push(Operation::CreateDirectory(path.to_owned()));
                }
            }

            let children = diff
                .children(path)
                .expect("path is a node in the tree")
                .map(|x| x.path())
                .collect::<Vec<_>>();
            queue.extend(children);
        }

        ops
    }

    async fn execute_operations(&mut self, operations: Vec<Operation>) -> anyhow::Result<()> {
        let (sender, mut receiver) = progress::make();

        let mut poisened_paths = Vec::new();
        let mut file_transfers = Vec::new();

        writeln!(self.env.err(), "Executing preliminary actions...").unwrap();

        // Execute preliminary operations (in sequence), and remember file transfers for later, so that they can be processed
        // in parallel
        for op in operations {
            match op {
                Operation::CreateDirectory(path) => {
                    let res = self.to.mkdir(&path).await;
                    if let Some(err) = res.err() {
                        writeln!(self.env.err(), "Failed to create directory at {path}").unwrap();
                        log::error!(
                            "Preliminary operation: Creating directory {path} failed: {err}"
                        );
                        poisened_paths.push(path);
                    }
                }
                Operation::Delete(path) => {
                    let res = self.to.delete(&path).await;
                    if let Some(err) = res.err() {
                        writeln!(self.env.err(), "Failed to delete {path}").unwrap();
                        log::error!("Preliminary operation: Deleeting {path} failed: {err}");
                        poisened_paths.push(path);
                    }
                }
                Operation::Transfer(transfer) => {
                    let progress_id = receiver.add_item(transfer.clone());
                    file_transfers.push((transfer, progress_id))
                }
            }
        }

        // Execute file transfers in parallel, skipping over those where we expect errors ("poisened paths")
        // Actually transfer files. Note that the sender is moved below, so the sender will be
        // dropped when transfer_files finishes
        let this = Arc::new(self.clone());
        let transfer_task = tokio::spawn(async move {
            this.transfer_files(sender, file_transfers, poisened_paths)
                .await
        });

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

    async fn execute_async(mut self) -> anyhow::Result<()> {
        let mut operations = self.prepare().await?;
        operations.sort();

        if !self.yes {
            for op in &operations {
                writeln!(self.env.err(), "{op}").unwrap();
            }

            let cont = self
                .env
                .prompt_continue("These operations will be performed in the 'to' repository. Continue?");
            if !cont {
                bail!("Aborted");
            }
        }

        self.execute_operations(operations).await?;

        Ok(())
    }
}
