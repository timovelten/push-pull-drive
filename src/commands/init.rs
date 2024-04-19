use crate::{
    context::{self, AppContext},
    db::SqliteDatabase,
    gdrive::{self, config::GoogleDriveInitArgs},
    local::{self, LocalInitArgs},
    util::Shell,
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

    // In an earlier version, we failed here if the database already existed. We
    // are now a bit more lenient. If the database already exists, we just run with it.
    // This is so that we can prompt users to to run init again in case the refresh token
    // ever expires or something similar happens.
    let db_path = {
        let mut tmp = config_dir;
        tmp.push(context::DATABASE_NAME);
        tmp
    };

    let db = match SqliteDatabase::new_from_path(db_path, true) {
        Ok(db) => Arc::new(db),
        Err(_) => {
            todo!("Intialize the database")
        }
    };

    let ctx: AppContext = AppContext {
        shell,
        wd: "".to_owned(),
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
