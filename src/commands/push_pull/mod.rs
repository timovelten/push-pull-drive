use crate::{
    FileInfo, InspectAsyncReader, PathInfo, WithoutNewlines, context::{CONFIG_DIR, Context, Ctx, ExecutionEnvironment}, file_repository::FileRepository, filetree::{self, FileTree, Node as _, Patch, PatchKind, file_tree_of_repository}, paths::{RPath, RPathBuf},
};
use anstyle::{AnsiColor, Reset, Style};
use anyhow::{Error, bail};
use progress::{ProgressId, ProgressSender};
use std::{
    cmp::Reverse, collections::VecDeque, fmt::Display, io::Write as _, sync::Arc,
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

    /// If constructive, we try to avoid deletions on the target side. Note that we still delete
    /// directories if they need to be replaced by a file.
    #[arg(long, default_value = "false")]
    constructive: bool,

    /// If reset is active, newer files on the "from" side are overwritten by the
    /// older files that are currently present in the "to" side.
    #[arg(long, default_value = "false")]
    reset: bool
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

pub async fn push(ctx: &impl Context, args: PushPullArgs) -> anyhow::Result<()> {
    let Ctx { env, local, remote } = ctx.get();
    PushPull {
        env: env.clone(),
        from: local.clone(),
        to: remote.clone(),
        path: get_path(args.path, &env.wd),
        max_concurrent_transfers: args.limit_concurrent_transfers,

        reset: args.reset,
        constructive: args.constructive,
        yes: args.yes,
        from_files: FileTree::empty(),
        to_files: FileTree::empty(),
    }
    .execute_async()
    .await?;

    Ok(())
}

pub async fn pull(ctx: &impl Context, args: PushPullArgs) -> anyhow::Result<()> {
    let Ctx { env, local, remote } = ctx.get();
    PushPull {
        env: env.clone(),
        from: remote.clone(),
        to: local.clone(),
        path: get_path(args.path, &env.wd),
        max_concurrent_transfers: args.limit_concurrent_transfers,

        reset: args.reset,
        constructive: args.constructive,
        yes: args.yes,
        from_files: FileTree::empty(),
        to_files: FileTree::empty(),
    }
    .execute_async()
    .await?;

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
    Create,
    Update,
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
        const CREATE_STYLE: Style =
            Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Green)));
        const DELETE_STYLE: Style =
            Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Red)));
        const END: Reset = Reset;

        match self {
            Self::CreateDirectory(path) => {
                let path = WithoutNewlines::new(format!("{path}"));

                write!(f, "{CREATE_STYLE}Create directory {path} {END}")
            }
            Self::Transfer(transfer) => match transfer.kind {
                TransferKind::Create => {
                    write!(f, "{CREATE_STYLE}{transfer}{END}")
                }
                TransferKind::Update => write!(f, "{transfer}"),
            },
            Self::Delete(path) => {
                let path = WithoutNewlines::new(format!("{path}"));
                write!(f, "{DELETE_STYLE}Delete: {path}{END}")
            }
        }
    }
}

impl Display for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let serialized = format!("{}", self.path);

        match self.kind {
            TransferKind::Update => write!(f, "Update: {}", WithoutNewlines::new(&serialized))?,
            TransferKind::Create => write!(f, "Create: {}", WithoutNewlines::new(&serialized))?,
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
    constructive: bool,
    reset: bool,
    from_files: FileTree<PathInfo>,
    to_files: FileTree<PathInfo>,
}

impl<A, B> PushPull<A, B>
where
    A: FileRepository,
    B: FileRepository,
{
    async fn execute_async(mut self) -> anyhow::Result<()> {
        let mut operations = self.prepare().await?;
        operations.sort();

        if operations.is_empty() {
            writeln!(self.env.err(), "Nothing to do").unwrap();
            return Ok(());
        }

        if !self.yes {
            for op in &operations {
                writeln!(self.env.err(), "{op}").unwrap();
            }

            let cont = self.env.prompt_continue(
                "These operations will be performed in the 'to' repository. Continue?",
            );
            if !cont {
                bail!("Aborted");
            }
        }

        self.execute_operations(operations).await?;

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

            if is_system_path(path) {
                // Do not let anyone write to our directory...
                continue;
            }

            // TODO: Skip over the .push_pull_drive directory here, and also introduce a
            // way for the to repository to say that it wants to ignore that path
            // so that we do not wait for the operation to fail but explicitly ignore the path
            // here
            match patch.kind {
                PatchKind::CreateFile => {
                    let from_info = self.from_files.get(path).unwrap().unwrap_fileinfo();

                    ops.push(Operation::Transfer(Transfer {
                        kind: TransferKind::Create,
                        path: path.to_owned(),
                        size: from_info.size,
                    }));
                }
                PatchKind::CreateDirectory => {
                    ops.push(Operation::CreateDirectory(path.to_owned()));
                }
                PatchKind::DeleteDirectory | PatchKind::DeleteFile => {
                    if !self.constructive {
                        ops.push(Operation::Delete(path.to_owned()));
                    }
                }
                PatchKind::PresentFile => {
                    let from_info = self.from_files.get(path).unwrap().unwrap_fileinfo();
                    let to_info = self.to_files.get(path).unwrap().unwrap_fileinfo();

                    if self.should_update_file(from_info, to_info) {
                        ops.push(Operation::Transfer(Transfer {
                            path: path.to_owned(),
                            kind: TransferKind::Update,
                            size: from_info.size,
                        }));
                    } else {
                        log::debug!("{:?} is unchanged", path);
                    }
                }
                PatchKind::PresentDirectory => {
                    // We ignore changes to only directories
                }
                PatchKind::ReplaceDirectoryByFile => {
                    // Replace a dir by a file
                    let from_info = self.from_files.get(path).unwrap().unwrap_fileinfo();

                    ops.push(Operation::Delete(path.to_owned()));
                    ops.push(Operation::Transfer(Transfer {
                        kind: TransferKind::Create,
                        path: path.to_owned(),
                        size: from_info.size,
                    }));
                }
                PatchKind::ReplaceFileByDirectory => {
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

    fn should_update_file(&self, from_info: &FileInfo, to_info: &FileInfo) -> bool {
        if from_info.size != to_info.size {
            return true;
        }

        let mod_delta = (from_info.modified - to_info.modified).as_seconds_f64();
        if mod_delta >= 1.0 {
            // The from file is more recent than the to file, so we overwrite it
            return true;
        } else if self.reset && mod_delta.abs() >= 1.0 {
            // We are in reset mode and the mod times differ. "reset" the to_file
            // to what the from repo has
            return true;
        }

        false
    }

    async fn transfer_files(
        self: Arc<Self>,
        sender: ProgressSender,
        mut files: Vec<(Transfer, ProgressId)>,
        mut poisened_paths: Vec<RPathBuf>,
    ) -> Vec<(Transfer, Error)> {
        const LARGE_FILE_SIZE: u64 = 24 * 1024 * 1024;

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
                let poisened = poisened_paths.iter().any(|f| transfer.path.starts_with(f));
                if poisened {
                    sender.notify_failed(id, "Skipped due to previous error".to_owned());

                    // Do not return an error here. That would only make the output harder to read.
                    // We already notified the user that this was skipped.
                    return Ok(());
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
                        log::error!("Preliminary operation: Deleting {path} failed: {err}");
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

    fn concurrency_limit(&self) -> usize {
        A::CONCURRENCY_LIMIT
            .min(B::CONCURRENCY_LIMIT)
            .min(self.max_concurrent_transfers)
    }
}

fn is_system_path(path: &RPath) -> bool {
    let config_dir: RPathBuf = RPathBuf::from_components([CONFIG_DIR]);
    path.starts_with(&config_dir)
}
