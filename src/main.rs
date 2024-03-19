mod commands;
mod context;
mod db;
mod error;
mod file_repository;
mod fileinfo;
mod gdrive;
mod local;
mod paths;
mod progress;
mod util;

use anyhow::anyhow;
use clap::{Parser, Subcommand};
use commands::{
    init::{self, InitBackend},
    ls::{self, LsArgs},
    push_pull::{self, PushPullArgs},
    status::{self, StatusArgs},
};
use context::{find_core_context, Context, CoreContext, FullContext};
use env_logger::{Builder, Target};
pub use fileinfo::*;
use local::LocalBackend;
pub use paths::*;
use std::fs::File;

const CONFIG_DIR: &str = ".basicdrive";
const DATABASE_NAME: &str = "database.sqlite3";

// Constants, that are used in the app configuration (database)
pub const KEY_BACKEND: &str = "backend";

pub type Result<E> = anyhow::Result<E>;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// A path to a log file. Can be "-" to log to stdout. Logging to terminal is not recommended,
    /// because it currently (more or less) breaks the various progress bars we show.
    #[arg(long)]
    log: Option<String>,
    /// Pretend as if the current working directory was the given directory. Useful for operating
    /// on folders that are elsewhere.
    #[arg(long)]
    dir: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Adds files to myapp
    Init {
        #[command(subcommand)]
        backend: InitBackend,
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
// TODO: Handle long windows filenames. Probably like this
// TODO: Rename to basic drive
// https://gal.hagever.com/posts/windows-long-paths-in-rust
// TODO: Have a command like maintenance fix-duplicates that fixes "two files/folders having the same name" by
//  - if two files have the same name and sha1 hash, just delete one of them
//  - if one of two folders is empty, delete it.
//  - otherwise print a link to the web interface of google drive so the user may fix it himself.
pub fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let wd = match cli.dir {
        Some(wd) => wd.into(),
        _ => std::env::current_dir()?,
    };

    if let Some(path) = cli.log {
        let target = if path == "-" {
            Target::Stdout
        } else {
            let f: Box<File> = File::create(path).expect("could not open log file").into();
            Target::Pipe(f)
        };

        Builder::new()
            .target(target)
            .filter_level(log::LevelFilter::Debug)
            .init();
    }

    if let Commands::Init { backend } = &cli.command {
        // Handle init separately
        init::init(backend.clone(), &wd)
    } else {
        // Handle every other command
        let ctx = find_core_context(&wd).unwrap();
        let mut full_context = get_full_context(ctx).unwrap();
        full_context.handle(cli.command)
    }
}

// We use this awkward construction, where this returns dyn CommandHandler instead
// of FullContext<B> so that FullContext<B> does not need to be dyn.
fn get_full_context(ctx: CoreContext) -> anyhow::Result<Box<dyn CommandHandler>> {
    let config = local::Config {
        root_path: ctx.root_dir.clone(),
        ignore_prefixes: vec![crate::CONFIG_DIR.to_string()],
        ..Default::default()
    };
    let local = LocalBackend::from_config(config);

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

impl<C: Context> CommandHandler for C {
    fn handle(&mut self, cmd: Commands) -> anyhow::Result<()> {
        let handler_future = async move {
            match cmd {
                Commands::Init { .. } => {
                    unimplemented!("Init command is handled separately elsewhere")
                }
                Commands::Pull(args) => push_pull::pull(self, args).await,
                Commands::Push(args) => push_pull::push(self, args).await,
                Commands::RemoteLs(args) => ls::remote_ls(self, args).await,
                Commands::Status(args) => status::local_status(self, args).await,
                Commands::RemoteStatus(args) => status::remote_status(self, args).await,
            }
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("building runtime should not fail");

        rt.block_on(handler_future)
    }
}
