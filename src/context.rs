use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::bail;

use crate::{
    db::{KeyValueStore, Side, SqliteDatabase, TrackedFilesRepository},
    file_repository::RemoteRepository,
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

pub struct FullContext<R: RemoteRepository> {
    pub core: CoreContext,
    pub local: LocalBackend<R::Hasher>,
    pub remote: R,
}

impl<R: RemoteRepository> FullContext<R> {
    pub fn new(core: CoreContext, local: LocalBackend<R::Hasher>, remote: R) -> Self {
        Self {
            core,
            local,
            remote,
        }
    }
}

pub trait Context {
    type Remote: RemoteRepository;

    fn get(&self) -> &FullContext<Self::Remote>;

    fn get_mut(&mut self) -> &mut FullContext<Self::Remote>;

    fn key_value_store(&self) -> &dyn KeyValueStore {
        &*self.get().core.key_value_store
    }

    fn tracked_files(&self) -> &dyn TrackedFilesRepository {
        &*self.get().core.tracked_files
    }

    fn tracked_files_or_ignore(&self, ignore_index: bool) -> &dyn TrackedFilesRepository {
        if ignore_index {
            &IgnoreTrackedFilesRepo
        } else {
            self.tracked_files()
        }
    }

    fn local(&self) -> &LocalBackend<<Self::Remote as RemoteRepository>::Hasher> {
        &self.get().local
    }

    fn remote(&self) -> &Self::Remote {
        &self.get().remote
    }

    fn wd(&self) -> &str {
        &self.get().core.wd
    }
}

impl<R: RemoteRepository> Context for FullContext<R> {
    type Remote = R;

    fn get(&self) -> &FullContext<Self::Remote> {
        self
    }

    fn get_mut(&mut self) -> &mut FullContext<Self::Remote> {
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
