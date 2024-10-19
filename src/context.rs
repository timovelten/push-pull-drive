use crate::{
    db::{KeyValueStore, Side, SqliteDatabase, TrackedFilesRepository},
    file_repository::FileRepository,
    local::LocalBackend,
    paths::{RPath, RPathBuf},
    util::Shell,
    FileInfo,
};
use anyhow::bail;
use std::{
    path::{Path, PathBuf},
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
    pub root_dir: PathBuf,
    pub key_value_store: Arc<dyn KeyValueStore>,
    pub tracked_files: Arc<dyn TrackedFilesRepository>,
    pub handle: tokio::runtime::Handle,
}

impl ExecutionEnvironment {
    #[allow(dead_code)]
    pub fn key_value_store(&self) -> &dyn KeyValueStore {
        &*self.key_value_store
    }

    pub fn tracked_files(&self) -> &dyn TrackedFilesRepository {
        &*self.tracked_files
    }

    pub fn tracked_files_or_ignore(&self, ignore_index: bool) -> &dyn TrackedFilesRepository {
        if ignore_index {
            &IgnoreTrackedFilesRepo
        } else {
            self.tracked_files()
        }
    }

    pub fn wd(&self) -> &RPath {
        &self.wd
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

pub fn find_context<P: AsRef<Path>>(
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

        log::debug!("Using config dir {:?}", config_dir);

        // This may fail. We then do not check folder further up,
        // but instead notify the user that the config folder that exists is somehow invalid.
        let db_path = config_dir.join(DATABASE_NAME);
        let db = Arc::new(SqliteDatabase::new_from_path(db_path, false)?);
        let key_value_store: Arc<dyn KeyValueStore> = db.clone();
        let tracked_files: Arc<dyn TrackedFilesRepository> = db;

        let wd = RPathBuf::try_from_path(root_dir, &wd)?;
        log::info!("Root directory is {root_dir:?}, relative working directory is {wd}");
        return Ok(ExecutionEnvironment {
            shell,
            wd,
            key_value_store,
            tracked_files,
            root_dir: root_dir.to_owned(),
            handle,
        });
    }

    bail!("Could not determine root (config) folder.")
}

pub struct IgnoreTrackedFilesRepo;

impl TrackedFilesRepository for IgnoreTrackedFilesRepo {
    fn update(&self, _info: FileInfo, _side: Side) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_all(&self, _dir_path: &RPath, _side: Side) -> anyhow::Result<Vec<FileInfo>> {
        Ok(Vec::new())
    }

    fn delete(&self, _path: &RPath) -> anyhow::Result<()> {
        Ok(())
    }
}
