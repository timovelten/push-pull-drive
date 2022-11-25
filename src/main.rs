mod app;
mod backend;
mod db;
mod fileinfo;
mod paths;
mod util;
mod backend_local;
mod error;
mod walker;

use backend::KEY_BACKEND;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use crate::{app::DefaultApp, backend_local::LocalBackend};

pub use error::{Error, Result};
pub use fileinfo::*;
pub use paths::*;

const CONFIG_DIR: &str = ".ppdrive";
const DATABASE_NAME: &str = "database.sqlite3";

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Adds files to myapp
    Init {
        #[command(subcommand)]
        backend: Backends,
    },
    /// Show status of local files
    Status,
    /// Show status of remote files
    RemoteStatus,
    /// Pull modified / new remote files and deletions into the local state
    Pull,
    /// Push local modified / new files and deletions to the remote
    Push,
    DeleteIndex,
}

#[derive(Subcommand)]
enum Backends {
    Local { path: String },
}

trait App {
    fn init_cmd(&self, b: Backends) -> crate::Result<()>;
    fn status_cmd(&self) -> crate::Result<()>;
    fn remote_status_cmd(&self) -> crate::Result<()>;
    fn pull_cmd(&self) -> crate::Result<()>;
    fn push_cmd(&self) -> crate::Result<()>;
}

// We need App to be dyn. Assert it here and let the compiler make sure.
impl dyn App {}

// TODO: Abstract away the ugliness of paths, by creating a custom walkdir function
// that provides us with our FileInfo enum. It's always possible to turn a relative name into a
// a path on disk. It's harder to turn an acutal path into a relative name.
// In particular, this allows us to drop the arbitrary restrictions on the format in
// which root_path is stroed
// TODO: Make a debug assertion, that checks if relative names are valid. That is, we check if
// they do not contain any dots and are slash separated.
// TODO: Handle long windows filenames. Probably like this
// https://gal.hagever.com/posts/windows-long-paths-in-rust
fn main() -> crate::Result<()> {
    let cli = Cli::parse();
    handle_command(cli.command)
}

fn handle_command(cmd: Commands) -> crate::Result<()> {
    let cwd = std::env::current_dir()?;
    let app = find_context(cwd)?;

    match cmd {
        Commands::Init { backend } => app.init_cmd(backend)?,
        Commands::Pull => app.pull_cmd()?,
        Commands::Push => app.push_cmd()?,
        Commands::Status => app.status_cmd()?,
        Commands::RemoteStatus => app.remote_status_cmd()?,
        Commands::DeleteIndex => todo!(),
    };

    Ok(())
}

fn find_context<P: AsRef<Path>>(cwd: P) -> crate::Result<Box<dyn App>> {
    let cwd = cwd.as_ref();

    let uninit_app = UninitializedApp {
        cwd: cwd.to_owned(),
    };

    for path in cwd.ancestors() {
        match uninit_app.load_actual_app(path) {
            Ok(app) => return Ok(app),
            Err(Error::NoConfigFound) => continue,
            Err(err) => {
                // There was an error, and it wasn't NoConfigFound.
                // For example, a config was found but it was invalid.
                // We stop here..
                return Err(err);
            }
        }
    }

    // Could not find a config, so return the uninitialized app
    Ok(Box::new(uninit_app))
}

struct UninitializedApp {
    cwd: PathBuf,
}

impl UninitializedApp {
    fn load_actual_app<P: AsRef<Path>>(&self, root_path: P) -> crate::Result<Box<dyn App>> {
        let config_dir = root_path.as_ref().join(crate::CONFIG_DIR);

        if !config_dir.exists() {
            return Err(Error::NoConfigFound);
        }

        let db = db::DbRepository::new_from_path(config_dir, crate::DATABASE_NAME)?;

        // This will be needed anyways, so we create it here
        let local = {
            let mut x = LocalBackend::new(root_path.as_ref().to_owned());
            x.ignore(crate::CONFIG_DIR);
            x
        };

        let cwd = paths::relative_name(root_path, &self.cwd)?;

        // Note: Changes made here will almost certainly need corresponding changes to be made
        // below in the `init_cmd` function.
        let ret: Box<dyn App> = match db.get_value(KEY_BACKEND)? {
            Some(s) if s == LocalBackend::THIS_BACKEND => {
                let local_path = db
                    .get_value(LocalBackend::KEY_LOCAL_PATH)?
                    .ok_or(Error::ConfigInvalid)?;
                let remote = LocalBackend::new(local_path.into());

                Box::new(DefaultApp::new(cwd, db, local, remote))
            }
            _ => return Err(Error::ConfigInvalid),
        };

        Ok(ret)
    }
}

impl App for UninitializedApp {
    fn init_cmd(&self, b: Backends) -> crate::Result<()> {
        let config_dir = self.cwd.join(CONFIG_DIR);
        // Make sure the config directory exists
        std::fs::create_dir(&config_dir)?;
        // create the database here
        let db = db::initialize_database(config_dir, DATABASE_NAME)?;

        // Note: Changes made here will almost certainly need corresponding changes to be
        // made above in the `load_actual_app` function.
        match b {
            Backends::Local { path } => {
                db.put_value(KEY_BACKEND, LocalBackend::THIS_BACKEND)?;
                db.put_value(LocalBackend::KEY_LOCAL_PATH, &path)?;
            }
        }

        Ok(())
    }

    fn status_cmd(&self) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }

    fn remote_status_cmd(&self) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }

    fn pull_cmd(&self) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }

    fn push_cmd(&self) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }
}
