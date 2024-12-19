use crate::{
    config::Config, context::{self, ExecutionEnvironment}, gdrive::{self, config::GoogleDriveInitArgs}, local::{self, LocalInitArgs}, paths::RPath, util::Shell
};
use std::{io::ErrorKind, path::Path, sync::Arc};
use tokio::runtime::Handle as RuntimeHandle;

#[derive(clap::Subcommand, Clone)]
pub enum InitBackendCmd {
    Local(LocalInitArgs),
    GoogleDrive(GoogleDriveInitArgs),
}

pub fn init(
    b: InitBackendCmd,
    root_dir: &Path,
    shell: Arc<Shell>,
    handle: RuntimeHandle,
) -> anyhow::Result<()> {
    let config_dir = root_dir.join(context::CONFIG_DIR);

    // Make sure the config directory exists
    match std::fs::create_dir(&config_dir) {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.into()),
    };

    // Open or create the dabase
    let config = Config::new(&config_dir);

    let ctx: ExecutionEnvironment = ExecutionEnvironment {
        shell,
        config,
        wd: RPath::empty().to_owned(),
        root_dir: root_dir.to_owned(),
        handle,
    };

    match b {
        InitBackendCmd::Local(args) => local::init(&ctx, args),
        InitBackendCmd::GoogleDrive(args) => gdrive::init(&ctx, args),
    }
}
