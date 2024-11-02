use crate::{
    context::{self, ExecutionEnvironment}, db::SqliteDatabase, gdrive::{self, config::GoogleDriveInitArgs}, local::{self, LocalInitArgs}, paths::RPath, util::Shell
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

    let db_path = {
        let mut tmp = config_dir;
        tmp.push(context::DATABASE_NAME);
        tmp
    };

    // Open or create the dabase
    let db: Arc<SqliteDatabase> = Arc::new(SqliteDatabase::new_from_path(db_path, true)?);

    let ctx: ExecutionEnvironment = ExecutionEnvironment {
        shell,
        wd: RPath::empty().to_owned(),
        key_value_store: db.clone(),
        tracked_files: db,
        root_dir: root_dir.to_owned(),
        handle,
    };

    match b {
        InitBackendCmd::Local(args) => local::init(&ctx, args),
        InitBackendCmd::GoogleDrive(args) => gdrive::init(&ctx, args),
    }
}
