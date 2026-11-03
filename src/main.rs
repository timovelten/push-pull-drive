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
};

pub mod context;
use context::{Ctx, ExecutionEnvironment, find_environment};

mod fileinfo;
pub use fileinfo::*;
use paths::RPathBuf;

pub mod local;
use crate::{file_repository::FileRepository, local::LocalBackend};

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
    Pull(PushPullArgs),
    Push(PushPullArgs),
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
#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let shell = Arc::new(Shell::new());

    init_logger(shell.clone().owned_err(), cli.level);
    let wd = std::env::current_dir()?;

    // Handle init separately, because when it is called there is no backend to load
    if let Commands::Init { backend } = &cli.command {
        return init::init(backend.clone(), &wd, shell).await;
    }

    async fn handle<R: FileRepository>(
        env: ExecutionEnvironment,
        local: LocalBackend,
        remote: R,
        cmd: Commands,
    ) -> anyhow::Result<()> {
        let ctx = Ctx::new(env, local, remote);

        match cmd {
            Commands::Init { .. } => unimplemented!("Init command is handled separately elsewhere"),
            Commands::RemoteLs(args) => ls::remote_ls(&ctx, args).await,
            Commands::Pull(args) => push_pull::pull(&ctx, args).await,
            Commands::Push(args) => push_pull::push(&ctx, args).await,
        }
    }

    let env = find_environment(&wd, shell).unwrap();
    let local = LocalBackend::from_config(get_local_backend_config(&env.root_dir));

    // Now load a remote backend and monomorphize
    if let Some(remote) = local::load(&env)? {
        handle(env, local, remote, cli.command).await
    } else if let Some(remote) = gdrive::load(&env)? {
        handle(env, local, remote, cli.command).await
    } else {
        Err(anyhow!("Config invalid. All backends failed to load."))
    }
}

fn get_local_backend_config(root_dir: impl AsRef<Path>) -> local::Config {
    local::Config {
        root_path: root_dir.as_ref().to_owned(),
        ignore_prefixes: vec![RPathBuf::from_components([crate::context::CONFIG_DIR])],
    }
}
