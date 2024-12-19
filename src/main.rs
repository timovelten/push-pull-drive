use anyhow::anyhow;
use clap::{Parser, Subcommand};
use env_logger::Target;
use log::LevelFilter;
use std::{path::Path, sync::Arc};

pub mod commands;
use commands::{
    init::{self, InitBackendCmd},
    ls::{self, LsArgs},
    push_pull::{self, PushPullArgs},
    status::{self, StatusArgs},
};

pub mod context;
use context::{find_context, Context, Ctx, ExecutionEnvironment};

mod config;
mod fileinfo;
pub use fileinfo::*;
use paths::RPathBuf;

pub mod local;
use crate::local::LocalBackend;

pub mod paths;

mod util;
pub use util::*;

pub mod db;
pub mod file_repository;
pub mod filetree;
pub mod gdrive;

// Note that the level argument is implemented through the LevelFilter::from_str implementation.
// Therefore, clap cannot possibly know all values (and therefore also cannot list them in the help).
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// Possible values include off, error, warn, info, debug and trace. Default is warn.
    #[arg(long)]
    level: Option<log::LevelFilter>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[command(subcommand)]
        backend: InitBackendCmd,
    },
    RemoteLs(LsArgs),
    Status(StatusArgs),
    RemoteStatus(StatusArgs),
    Pull(PushPullArgs),
}

trait CommandHandler {
    fn handle(&mut self, cmd: Commands) -> anyhow::Result<()>;
}

fn init_logger(err: impl std::io::Write + Send + 'static, level: Option<LevelFilter>) {
    let mut builder = env_logger::Builder::from_default_env();
    builder.target(Target::Pipe(Box::new(err)));
    if let Some(level) = level {
        builder.filter_level(level);
    }

    builder.init();
}

// TODO: When deleting a file, do not leave behind orphaned (empty) directories
// TODO: Handle long windows filenames. Probably like this https://gal.hagever.com/posts/windows-long-paths-in-rust
// TODO: Have a command like maintenance fix-duplicates that fixes "two files/folders having the same name" by
//  - if two files have the same name and sha1 hash, just delete one of them
//  - if one of two folders is empty, delete it.
//  - otherwise print a link to the web interface of google drive so the user may fix it themselves.
pub fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let shell = Arc::new(Shell::new());
    
    init_logger(shell.clone().owned_err(), cli.level);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let wd = std::env::current_dir()?;
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
    local::Config {
        root_path: root_dir.as_ref().to_owned(),
        ignore_prefixes: vec![RPathBuf::from_components([crate::context::CONFIG_DIR])],
    }
}

// This awkward construction enables us to have type parameters inside of the context, whose
// value is only determined at runtime.
fn get_full_context(ctx: ExecutionEnvironment) -> anyhow::Result<Box<dyn CommandHandler>> {
    let local = LocalBackend::from_config(get_local_backend_config(&ctx.root_dir));

    // Try to load an appropriate functions. It is the responsibility of the 'load' function
    // to return None if it is not applicable to the current config. In that case we
    // continue to the next candidate.
    let ret: Box<dyn CommandHandler> = if let Some(remote) = local::load(&ctx)? {
        Box::new(Ctx::new(ctx, local, remote))
    } else if let Some(remote) = gdrive::load(&ctx)? {
        Box::new(Ctx::new(ctx, local, remote))
    } else {
        return Err(anyhow!("Config invalid. All backends failed to load."));
    };

    Ok(ret)
}

impl<C: Context> CommandHandler for C {
    fn handle(&mut self, cmd: Commands) -> anyhow::Result<()> {
        match cmd {
            Commands::Init { .. } => unimplemented!("Init command is handled separately elsewhere"),
            Commands::RemoteLs(args) => ls::remote_ls(self, args),
            Commands::Status(args) => status::local_status(self.get(), args),
            Commands::RemoteStatus(args) => status::remote_status(self.get(), args),
            Commands::Pull(args) => push_pull::pull(self.get(), args),
        }
    }
}
