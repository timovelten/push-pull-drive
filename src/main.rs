mod app;
mod backend;
mod local;
mod db;
mod error;
mod fileinfo;
mod gdrive;
mod paths;
mod util;
mod progress;

use crate::{app::DefaultApp, local::LocalBackend};
use clap::{Parser, Subcommand};
use env_logger::{Builder, Target};
use gdrive::{FatAccessToken, GDriveBackend, GoogleCredential};
use oauth2::{AccessToken, ClientId, ClientSecret, RefreshToken};
use std::{
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
};
use time::OffsetDateTime;

pub use error::{Error, Result};
pub use fileinfo::*;
pub use paths::*;

const CONFIG_DIR: &str = ".ppdrive";
const DATABASE_NAME: &str = "database.sqlite3";

// Constants, that are used in the app configuration (database)
pub const KEY_BACKEND: &str = "backend";

const GDRIVE_BACKEND: &str = "gdrive";
const KEY_GDRIVE_CLIENT_SECRET: &str = "s";
const KEY_GDRIVE_CLIENT_ID: &str = "cid";
const KEY_GDRIVE_TOKEN: &str = "t";
const KEY_GDRIVE_TOKEN_EXPIRES: &str = "exp";
const KEY_GDRIVE_REFRESH_TOKEN: &str = "rt";

const LOCAL_BACKEND: &str = "local";
const KEY_LOCAL_PATH: &str = "local_path";

const CONCURRENT_FUTURES: usize = 10;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// A path to a log file. Can be "-" to log to stdout. Logging to terminal is not recommended,
    /// because it currently (more or less) breaks the various progress bars we show.
    #[arg(long)]
    log: Option<String>,
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
    Status {
        #[arg(long)]
        ignore_index: bool
    },
    /// Show status of remote files
    RemoteStatus {
        #[arg(long)]
        ignore_index: bool,
    },
    /// Pull modified / new remote files and deletions into the local state
    Pull {
        #[arg(long)]
        ignore_index: bool,
        #[arg(long)]
        yes: bool
    },
    /// Push local modified / new files and deletions to the remote
    Push {
        #[arg(long)]
        ignore_index: bool,
        #[arg(long)]
        yes: bool
    },
    RemoteLs {
        path: Option<String>,
    },
}

#[derive(Subcommand)]
enum Backends {
    Local {
        path: String,
    },
    GoogleDrive {
        /// Path to a JSON file which contains the two keys `installed/client_id` and
        /// `installed/client_secret` with values the OAuth Client ID and Client Secret respectively.
        /// You can create this file yourself or download it from the Google Cloud Console using the
        /// "Download JSON" button after creating a new Desktop OAuth Client at
        /// https://console.cloud.google.com/apis/credentials
        #[arg(value_name = "path")]
        secrets_path: String,
    },
}

// TODO: When deleting a file, do not leave behind orphaned (empty) directories
// TODO: Handle long windows filenames. Probably like this
// TODO: Rename to basic drive
// https://gal.hagever.com/posts/windows-long-paths-in-rust
// TODO: Have a command like maintenance fix-duplicates that fixes "two files/folders having the same name" by
//  - if two files have the same name and sha1 hash, just delete one of them
//  - if one of two folders is empty, delete it.
//  - otherwise print a link to the web interface of google drive so the user may fix it himself.
fn main() {
    let cli = Cli::parse();

    if let Some(path) = cli.log {
        let target = if path == "-" {
            Target::Stdout
        } else {
            let f: Box<File> = File::create(path)
                .expect("could not open log file")
                .into();
            Target::Pipe(f)
        };

        Builder::new()
            .target(target)
            .filter_level(log::LevelFilter::Info)
            .init();
    }

    match handle_command(cli.command) {
        Ok(_) => {}
        Err(err) => println!("{} [{:?}]", err, err),
    }
}

trait App {
    fn init_cmd(&self, b: Backends) -> crate::Result<()>;
    fn status_cmd(&self, ignore_index: bool) -> crate::Result<()>;
    fn remote_status_cmd(&self, ignore_index: bool) -> crate::Result<()>;
    fn pull_cmd(&self, ignore_index: bool, yes: bool) -> crate::Result<()>;
    fn push_cmd(&self, ignore_index: bool, yes: bool) -> crate::Result<()>;
    fn remote_ls(&self, path: Option<String>) -> crate::Result<()>;
}

// We need App to be dyn. Assert it here and let the compiler make sure.
impl dyn App {}

fn handle_command(cmd: Commands) -> crate::Result<()> {
    let cwd = std::env::current_dir()?;
    let app = find_context(cwd)?;

    match cmd {
        Commands::Init { backend } => app.init_cmd(backend)?,
        Commands::Pull { ignore_index, yes } => app.pull_cmd(ignore_index, yes)?,
        Commands::Push { ignore_index, yes } => app.push_cmd(ignore_index, yes)?,
        Commands::Status { ignore_index } => app.status_cmd(ignore_index)?,
        Commands::RemoteStatus { ignore_index } => app.remote_status_cmd(ignore_index)?,
        Commands::RemoteLs { path } => app.remote_ls(path)?,
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
            Err(Error::ConfigInvalid) => {
                // Return the uninitialized app to allow the user to re-run init
                // without having to delete everything.
                break;
            }
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
        let local = LocalBackend::new(
            root_path.as_ref().to_owned(),
            vec![crate::CONFIG_DIR.to_string()],
        );

        let cwd = paths::relative_name(root_path, &self.cwd)?;

        // Note: Changes made here will almost certainly need corresponding changes to be made
        // below in the `init_cmd` function.
        let ret: Box<dyn App> = match db.get_value(KEY_BACKEND)? {
            Some(s) if s == LOCAL_BACKEND => {
                let local_path = db.get_value(KEY_LOCAL_PATH)?.ok_or(Error::ConfigInvalid)?;

                let remote = LocalBackend::new(local_path.into(), Vec::new());

                Box::new(DefaultApp::new(cwd, db, local, remote))
            }
            Some(s) if s == GDRIVE_BACKEND => {
                // Read what we previously saved from the database
                let client_id = db
                    .get_value(KEY_GDRIVE_CLIENT_ID)?
                    .ok_or(Error::ConfigInvalid)?;

                let client_secret = db
                    .get_value(KEY_GDRIVE_CLIENT_SECRET)?
                    .ok_or(Error::ConfigInvalid)?;

                let last_token = db.get_value(KEY_GDRIVE_TOKEN)?;
                let last_token_expires = db.get_value(KEY_GDRIVE_TOKEN_EXPIRES)?;
                let last_token = match (last_token, last_token_expires) {
                    (Some(tok), Some(exp)) => {
                        let exp_timestamp = exp.parse::<i64>().map_err(|_| Error::ConfigInvalid)?;
                        let expires = OffsetDateTime::from_unix_timestamp(exp_timestamp)
                            .expect("failed to convert timestamp to Datetime");

                        Some(FatAccessToken {
                            access_token: AccessToken::new(tok),
                            expires,
                        })
                    }
                    (_, _) => None,
                };

                let refresh_token = db
                    .get_value(KEY_GDRIVE_REFRESH_TOKEN)?
                    .ok_or(Error::ConfigInvalid)?;

                // Create some objects in a DI kind of way and initialize the backend

                let google_client = gdrive::oauth::new_google_oauth_client(
                    ClientId::new(client_id),
                    ClientSecret::new(client_secret),
                );

                let cloned_db = db.clone();
                let credential = GoogleCredential::new(
                    google_client,
                    RefreshToken::new(refresh_token),
                    last_token,
                    move |token| {
                        // Note how we fail silently here. Maybe we shouldn't (?), we'll see
                        let _ = cloned_db.put_value(KEY_GDRIVE_TOKEN, token.access_token.secret());
                        let _ = cloned_db.put_value(
                            KEY_GDRIVE_TOKEN_EXPIRES,
                            &token.expires.unix_timestamp().to_string(),
                        );
                    },
                );

                let remote = GDriveBackend::new(credential);

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
        match std::fs::create_dir(&config_dir) {
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        };

        // In an earlier version, we failed here if the database already existed. We
        // are now a bit more lenient. If the database already exists, we just run with it.
        // This is so that we can prompt users to to run init again in case the refresh token
        // ever expires or something similar happens.
        let db = match db::DbRepository::new_from_path(&config_dir, crate::DATABASE_NAME) {
            Ok(db) => db,
            Err(_) => {
                println!("Could not open database");
                db::initialize_database(config_dir, DATABASE_NAME)?
            }
        };

        // Note: Changes made here will almost certainly need corresponding changes to be
        // made above in the `load_actual_app` function.
        match b {
            Backends::Local { path } => {
                db.put_value(KEY_BACKEND, LOCAL_BACKEND)?;
                db.put_value(KEY_LOCAL_PATH, &path)?;
            }
            Backends::GoogleDrive { secrets_path } => {
                let f = File::open(secrets_path)?;
                let parsed: serde_json::Value = serde_json::from_reader(f)?;
                let installed = parsed.get("installed").ok_or(Error::ConfigInvalid)?;

                let client_id = ClientId::new(
                    installed
                        .get("client_id")
                        .ok_or(Error::ConfigInvalid)?
                        .as_str()
                        .ok_or(Error::ConfigInvalid)?
                        .to_string(),
                );

                let client_secret = ClientSecret::new(
                    installed
                        .get("client_secret")
                        .ok_or(Error::ConfigInvalid)?
                        .as_str()
                        .ok_or(Error::ConfigInvalid)?
                        .to_string(),
                );

                // Login the user using the OAuth device flow
                let google_client = gdrive::oauth::new_google_oauth_client(
                    client_id.clone(),
                    client_secret.clone(),
                );

                let (fat_token, refresh_token) = gdrive::oauth::sign_in(google_client)?;

                // Store them in the database and return
                db.put_value(KEY_BACKEND, GDRIVE_BACKEND)?;
                db.put_value(KEY_GDRIVE_CLIENT_ID, client_id.as_str())?;
                db.put_value(KEY_GDRIVE_CLIENT_SECRET, client_secret.secret())?;

                db.put_value(KEY_GDRIVE_REFRESH_TOKEN, refresh_token.secret())?;
                db.put_value(KEY_GDRIVE_TOKEN, fat_token.access_token.secret())?;
                db.put_value(
                    KEY_GDRIVE_TOKEN_EXPIRES,
                    &format!("{}", fat_token.expires.unix_timestamp()),
                )?;

                println!("Success");
            }
        }

        Ok(())
    }

    fn remote_ls(&self, _s: Option<String>) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }

    fn status_cmd(&self, _ignore_index: bool) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }

    fn remote_status_cmd(&self, _ignore_index: bool) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }

    fn pull_cmd(&self, _ignore_index: bool, _yes: bool) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }

    fn push_cmd(&self, _ignore_index: bool, _yes: bool) -> crate::Result<()> {
        Err(Error::NoConfigFound)
    }
}
