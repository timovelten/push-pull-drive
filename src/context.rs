use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::bail;

use crate::{
    db::{KeyValueStore, Side, SqliteDatabase, TrackedFilesRepository},
    file_repository::FileRepository,
    local::LocalBackend,
    paths, FileInfo,
};

pub struct CoreContext {
    /// Working directory. This is a "context path" of the current working directory
    /// (NOT the OS provided working directory.)
    pub wd: String,
    /// Root dir. That is, the directory that contains the config directory
    pub root_dir: PathBuf,
    pub key_value_store: Arc<dyn KeyValueStore>,
    pub tracked_files: Arc<dyn TrackedFilesRepository>,
}

pub struct GlobalContext<R: FileRepository> {
    pub core: CoreContext,
    pub local: LocalBackend,
    pub remote: R,
}

#[allow(dead_code)]
impl<R: FileRepository> GlobalContext<R> {
    pub fn new(core: CoreContext, local: LocalBackend, remote: R) -> Self {
        Self {
            core,
            local,
            remote,
        }
    }

    pub fn key_value_store(&self) -> &dyn KeyValueStore {
        &*self.get().core.key_value_store
    }

    pub fn tracked_files(&self) -> &dyn TrackedFilesRepository {
        &*self.get().core.tracked_files
    }

    pub fn tracked_files_or_ignore(&self, ignore_index: bool) -> &dyn TrackedFilesRepository {
        if ignore_index {
            &IgnoreTrackedFilesRepo
        } else {
            self.tracked_files()
        }
    }

    pub fn local(&self) -> &LocalBackend {
        &self.get().local
    }

    pub fn remote(&self) -> &R {
        &self.get().remote
    }

    pub fn wd(&self) -> &str {
        &self.get().core.wd
    }
}

/// This Trait exists purely out of convenience. We want to be able to simpl write
/// function arguments as 
/// `ctx: &impl Ctx` instead of `<R: FileRepository>(ctx: &FullContext<R>)`.
pub trait Ctx {
    type Remote: FileRepository;

    fn get(&self) -> &GlobalContext<Self::Remote>;

    fn get_mut(&mut self) -> &mut GlobalContext<Self::Remote>;

    fn into(self) -> GlobalContext<Self::Remote>;
}

impl<R: FileRepository> Ctx for GlobalContext<R> {
    type Remote = R;

    fn get(&self) -> &GlobalContext<Self::Remote> {
        self
    }

    fn get_mut(&mut self) -> &mut GlobalContext<Self::Remote> {
        self
    }
    
    fn into(self) -> GlobalContext<Self::Remote> {
        self
    }
}

pub fn find_core_context<P: AsRef<Path>>(wd: P) -> anyhow::Result<CoreContext> {
    // Traverse ancestors of the current workign directory to try and find the next valid root...
    for root_dir in wd.as_ref().ancestors() {
        let config_dir = root_dir.join(crate::CONFIG_DIR);
        if !config_dir.exists() {
            continue;
        }

        log::debug!("Using config dir {:?}", config_dir);

        // This may fail. We then do not check folder further up,
        // but instead notify the user that the config folder that exists is somehow invalid.
        let db_path = config_dir.join(crate::DATABASE_NAME);
        let db = Arc::new(SqliteDatabase::new_from_path(db_path, false)?);
        let key_value_store: Arc<dyn KeyValueStore> = db.clone();
        let tracked_files: Arc<dyn TrackedFilesRepository> = db;

        let wd = paths::relative_name(root_dir, &wd)?;
        log::info!("Root directory is {root_dir:?}, relative working directory is {wd}");
        return Ok(CoreContext {
            wd,
            key_value_store,
            tracked_files,
            root_dir: root_dir.to_owned(),
        });
    }

    bail!("Could not determine root (config) folder.")
}

pub struct IgnoreTrackedFilesRepo;

impl TrackedFilesRepository for IgnoreTrackedFilesRepo {
    fn update(&self, _info: FileInfo, _side: Side) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_all(&self, _cpath_dir: &str, _side: Side) -> anyhow::Result<Vec<FileInfo>> {
        Ok(Vec::new())
    }

    fn delete(&self, _cpath: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
