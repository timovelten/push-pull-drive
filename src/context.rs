use crate::{
    db::SqliteDatabase, file_repository::FileRepository, local::LocalBackend, paths::RPathBuf, util::Shell
};
use anyhow::bail;
use delegate::delegate;
use std::{
    io::{self, Write as _},
    path::{Path as StdPath, PathBuf as StdPathBuf},
    sync::Arc,
};
use tokio::runtime::Handle as TokioHandle;

pub const CONFIG_DIR: &str = ".push-pull-drive";
pub const DATABASE_NAME: &str = "database.sqlite3";

/// This contains the whole application context except for the two backends we use
/// as source and destination. In other words, this struct can be intialized whenever the
/// database is created (but might itself not be initialized).
///
/// The "full" context is defined below.
#[derive(Clone)]
pub struct ExecutionEnvironment {
    pub shell: Arc<Shell>,
    /// Working directory as RPath
    pub wd: RPathBuf,
    /// Root dir. That is, the directory that contains the config directory
    pub root_dir: StdPathBuf,
    pub db: Arc<SqliteDatabase>,
    pub handle: tokio::runtime::Handle,
}

impl ExecutionEnvironment {
    delegate! {
        to self.shell {
            pub fn err(&self) -> impl io::Write + '_ ;

            pub fn out(&self) -> impl io::Write + '_ ;

            pub fn print_transient(&self, s: impl std::fmt::Display) -> io::Result<()>;
        }
    }

    pub fn prompt_continue(&self, msg: &str) -> bool {
        write!(self.err(), "{msg} [Y(es) / N(o)] ").unwrap();

        let mut buffer = String::new();
        io::stdin().read_line(&mut buffer).unwrap();

        if buffer.starts_with(['Y', 'y']) {
            true
        } else {
            false
        }
    } 
}

/// Full context needed for most operations
#[derive(Clone)]
pub struct Ctx<R: FileRepository> {
    pub env: ExecutionEnvironment,
    pub local: LocalBackend,
    pub remote: R,
}

impl<R: FileRepository> Ctx<R> {
    pub fn new(env: ExecutionEnvironment, local: LocalBackend, remote: R) -> Self {
        Self { env, local, remote }
    }
}

/// This trait makes it so that we do not have to carry around the generic parameters
/// of Ctx everywhere. We can just take an `impl Context` arg and be done.
pub trait Context {
    type Remote: FileRepository;

    fn get(&self) -> &Ctx<Self::Remote>;
}

impl<R: FileRepository> Context for Ctx<R> {
    type Remote = R;

    fn get(&self) -> &Ctx<Self::Remote> {
        self
    }
}

pub fn find_context<P: AsRef<StdPath>>(
    wd: P,
    shell: Arc<Shell>,
    handle: TokioHandle,
) -> anyhow::Result<ExecutionEnvironment> {
    // Traverse ancestors of the current workign directory to try and find the next valid root...
    for root_dir in wd.as_ref().ancestors() {
        let config_dir = root_dir.join(CONFIG_DIR);
        if !config_dir.exists() {
            continue;
        }

        // This may fail. We then do not check folder further up,
        // but instead notify the user that the config folder that exists is somehow invalid.
        let db = SqliteDatabase::new_in_dir(&config_dir, false)?;

        log::debug!("Using config dir {:?}", config_dir);

        let wd = RPathBuf::try_from_std_path(root_dir, &wd)?;
        log::info!("Root directory is {root_dir:?}, relative working directory is {wd:?}");

        return Ok(ExecutionEnvironment {
            db: Arc::new(db),
            shell,
            wd,
            root_dir: root_dir.to_owned(),
            handle,
        });
    }

    bail!("Could not determine root (config) folder.")
}
