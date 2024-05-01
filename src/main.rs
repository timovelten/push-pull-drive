pub mod commands;
use commands::{
    init::{self, InitBackendCmd},
    ls::{self, LsArgs},
    push_pull::{self, PushPullArgs},
    status::{self, StatusArgs},
};

pub mod context;
use context::{find_context, AppContext, Ctx, FullContext};

mod fileinfo;
pub use fileinfo::*;

pub mod local;
use crate::local::LocalBackend;

pub mod paths;
pub use paths::{append_to_cpath_in_place, append_to_relative_name, pop_cpath};

mod util;
pub use util::*;

pub mod db;
pub mod file_repository;
pub mod gdrive;

use anyhow::anyhow;
use clap::{Parser, Subcommand};
use env_logger::Target;
use std::{fs::File, path::Path, sync::Arc};

// Note that the level argument is implemented through the LevelFilter::from_str implementation.
// Therefore, clap cannot possibly know all values (and therefore also cannot list them in the help).
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// A path to a logfile. If not set, log to stderr.
    #[arg(long)]
    log: Option<String>,

    /// Possible values include off, error, warn, info, debug and trace. Default is warn.
    #[arg(long, default_value_t = log::LevelFilter::Warn)]
    level: log::LevelFilter,
    /// Pretend as if the current working directory was the given directory. Useful for operating
    /// on folders that are elsewhere.
    #[arg(long)]
    dir: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[command(subcommand)]
        backend: InitBackendCmd,
    },
    /// Show status of local files
    Status(StatusArgs),
    /// Show status of remote files
    RemoteStatus(StatusArgs),
    /// Pull modified / new remote files and deletions into the local state
    Pull(PushPullArgs),
    /// Push local modified / new files and deletions to the remote
    Push(PushPullArgs),
    RemoteLs(LsArgs),
}

trait CommandHandler {
    fn handle(&mut self, cmd: Commands) -> anyhow::Result<()>;
}

// TODO: When deleting a file, do not leave behind orphaned (empty) directories
// TODO: Handle long windows filenames. Probably like this https://gal.hagever.com/posts/windows-long-paths-in-rust
// TODO: Have a command like maintenance fix-duplicates that fixes "two files/folders having the same name" by
//  - if two files have the same name and sha1 hash, just delete one of them
//  - if one of two folders is empty, delete it.
//  - otherwise print a link to the web interface of google drive so the user may fix it themselves.
pub fn main() -> anyhow::Result<()> {
    // console_subscriber::init();

    let cli = Cli::parse();
    let wd = match cli.dir {
        Some(wd) => wd.into(),
        _ => std::env::current_dir()?,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let shell = Arc::new(Shell::new());
    let target = if let Some(path) = cli.log {
        let f: Box<File> = File::create(path).expect("could not open log file").into();
        Target::Pipe(f)
    } else {
        Target::Pipe(Box::new(shell.clone().owned_err()))
    };
    env_logger::Builder::new()
        .target(target)
        .filter_level(cli.level)
        .init();

    if let Commands::Init { backend } = &cli.command {
        // Handle init separately
        init::init(backend.clone(), &wd, shell, rt.handle().clone())
    } else {
        // Handle every other command
        let ctx = find_context(&wd, shell, rt.handle().clone()).unwrap();
        let mut full_context = get_full_context(ctx).unwrap();
        full_context.handle(cli.command)
    }
}

fn get_local_backend_config(root_dir: impl AsRef<Path>) -> local::Config {
    return local::Config {
        root_path: root_dir.as_ref().to_owned(),
        ignore_globs: vec![
            crate::context::CONFIG_DIR.to_owned(),
            format!("{}/**", crate::context::CONFIG_DIR),
        ],
        calculate_hashes: true,
    };
}

// This awkward construction enables us to have type parameters inside of the context, whose
// value is only determined at runtime.
fn get_full_context(ctx: AppContext) -> anyhow::Result<Box<dyn CommandHandler>> {
    let local = LocalBackend::from_config(get_local_backend_config(&ctx.root_dir));

    // Try to load an appropriate functions. It is the responsibility of the 'load' function
    // to return None if it is not applicable to the current config. In that case we
    // continue to the next candidate.
    let ret: Box<dyn CommandHandler> = if let Some(remote) = local::load(&ctx)? {
        Box::new(FullContext::new(ctx, local, remote))
    } else if let Some(remote) = gdrive::load(&ctx)? {
        Box::new(FullContext::new(ctx, local, remote))
    } else {
        return Err(anyhow!("Config invalid. All backends failed to load."));
    };

    Ok(ret)
}

impl<C: Ctx> CommandHandler for C {
    fn handle(&mut self, cmd: Commands) -> anyhow::Result<()> {
        match cmd {
            Commands::Init { .. } => unimplemented!("Init command is handled separately elsewhere"),
            Commands::Pull(args) => push_pull::pull(self, args),
            Commands::Push(args) => push_pull::push(self, args),
            Commands::RemoteLs(args) => ls::remote_ls(self, args),
            Commands::Status(args) => status::local_status(self, args),
            Commands::RemoteStatus(args) => status::remote_status(self, args),
        }
    }
}
