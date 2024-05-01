pub mod config;
pub mod file_hasher;
use anyhow::anyhow;
pub use config::*;
use walkdir::WalkDir;

use self::file_hasher::{FileHasher, Sha1Hasher};
use crate::file_repository::Error as FileRepositoryError;
use crate::file_repository::Result as FileRepositoryResult;
use crate::{
    file_repository::FileRepository, paths, util::SemaphoreGuarded, DirectoryInfo, FileInfo,
    PathInfo,
};
use futures::{stream::FuturesUnordered, AsyncRead, StreamExt};
use std::{
    fs::File,
    io::{ErrorKind, Seek},
    path::{Path, PathBuf},
    pin::{pin, Pin},
    sync::Arc,
    thread::available_parallelism,
};
use time::OffsetDateTime;
use tokio::{fs::File as TokioFile, sync::SemaphorePermit};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::spawn_blocking,
};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

const CONCURRENCY_LIMIT: usize = 16;

#[derive(Debug, Clone)]
pub struct LocalBackend<H = Sha1Hasher> {
    config: Arc<Config>,
    permits: Arc<Semaphore>,
    hasher: H,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// A path on the local file system. Note that although from this type definition
    /// it may look like we support non-valid-unicode paths, this is not the case
    /// and will panic at runtime.
    pub root_path: PathBuf,

    /// A list of relative_name prefixes. Any file or folder with a relative name that matches
    /// these prefixes will not be emitted by read_dir. If asked explicitly about these files,
    /// this implementation will return an error `Error::NotAllowed(_)`
    pub ignore_prefixes: Vec<String>,

    /// Whether we should calculate hashes of files. Calculating hashes is not particularly cheap
    /// as we can't really cache the result in any meaniningful way.
    pub calculate_hashes: bool,
}

impl<H: FileHasher + Default> LocalBackend<H> {
    pub fn from_config(config: Config) -> Self {
        Self::new(config, Default::default())
    }
}

impl<H: FileHasher> LocalBackend<H> {
    pub fn new(config: Config, hasher: H) -> Self {
        Self {
            config: Arc::new(config),
            permits: Arc::new(Semaphore::new(CONCURRENCY_LIMIT)),
            hasher,
        }
    }

    async fn aquire_owned_permit(&self) -> OwnedSemaphorePermit {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .expect("Semaphore should not be closed")
    }

    async fn aquire_permit(&self) -> SemaphorePermit {
        self.permits
            .acquire()
            .await
            .expect("Semaphore should not be closed")
    }

    /// cpath is still needed because it is returned as part of the metadata. Otherwise, it is ignored.
    fn read_file_impl_for_open_file(
        &self,
        cpath: String,
        mut file: File,
    ) -> FileRepositoryResult<(PathInfo, File)> {
        let metadata = file.metadata().map_err(|err| map_io_error(err, &cpath))?;
        let res = if metadata.is_file() {
            let hash = if self.config.calculate_hashes {
                // FIXME: This is (kind of?) CPU bound work that we will execute on a spawn_blocking
                //      tokio thread. That might honestly not be great. On the other hand, we limit concurrency
                //      anyways so we definitely should not end up with 500 threads doing nothing but running
                //      SHA1 so this is probably fine.
                file.rewind().map_err(as_other_error)?;
                self.hasher
                    .hash(&mut file)
                    .map_err(|err| map_io_error(err, &cpath))?
            } else {
                Vec::new()
            };

            let info = FileInfo {
                cpath,
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .expect("system should have modification times")
                    .into(),
                hash,
            };

            file.rewind().map_err(as_other_error)?;
            (PathInfo::File(info), file)
        } else if metadata.is_dir() {
            (
                PathInfo::Dir(DirectoryInfo {
                    cpath: cpath.to_owned(),
                }),
                file,
            )
        } else {
            // For now, we ignore symlinks by returning NotAllowed...
            return Err(FileRepositoryError::NotAllowed {
                cpath: cpath.to_owned(),
            });
        };

        Ok(res)
    }

    fn read_file_impl(&self, cpath: String) -> FileRepositoryResult<(PathInfo, File)> {
        if self.config.is_ignored(&cpath) {
            return Err(FileRepositoryError::NotAllowed {
                cpath: cpath.to_owned(),
            });
        }
        let local_path = self.config.get_local_path_from_relative_name(&cpath);

        // Open it, to avoid the time-of-check-time-of-use race condition. Note that
        // one can actually open directories.
        let file = File::open(local_path).map_err(|err| map_io_error(err, &cpath))?;
        self.read_file_impl_for_open_file(cpath, file)
    }

    /// Return a list of all paths contained in the given path (except for path itself)
    fn child_cpaths(
        config: &Arc<Config>,
        path: impl AsRef<Path>,
        max_depth: usize,
    ) -> FileRepositoryResult<Vec<String>> {
        let mut ret = Vec::new();

        let walker = WalkDir::new(path).max_depth(max_depth);
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    let error_depth = err.depth();
                    let error_cpath = if let Some(err_path) = err.path() {
                        paths::relative_name(&config.root_path, err_path).map_err(|err| {
                            let err = anyhow!(err).context(format!(
                                "Error occurred at depth {} for error path {err_path:?}",
                                error_depth
                            ));
                            FileRepositoryError::Other(err)
                        })?
                    } else {
                        "".to_owned()
                    };

                    let err = map_io_error(
                        err.into_io_error().expect("should be io error"),
                        &error_cpath,
                    );
                    return Err(err);
                }
            };

            if entry.depth() == 0 {
                continue;
            }

            let child_cpath =
                paths::relative_name(&config.root_path, entry.path()).map_err(as_other_error)?;

            ret.push(child_cpath);
        }

        Ok(ret)
    }
}

fn map_io_error(err: std::io::Error, cpath: &str) -> FileRepositoryError {
    if cpath.is_empty() {
        return as_other_error(err);
    }

    match err.kind() {
        ErrorKind::NotFound => FileRepositoryError::NotFound {
            cpath: cpath.to_owned(),
        },
        ErrorKind::PermissionDenied => FileRepositoryError::NotAllowed {
            cpath: cpath.to_owned(),
        },
        _ => {
            // It is actually extremely unfortunate that more error kinds are still not
            // stabilized. In the meantime, we really want the IsADirectory and IsNotADirectory
            // so we do them ourselves here...
            match err.raw_os_error() {
                Some(libc::ENOTDIR) => FileRepositoryError::NotADirectory {
                    cpath: cpath.to_owned(),
                },
                Some(libc::EISDIR) => FileRepositoryError::IsADirectory {
                    cpath: cpath.to_owned(),
                },
                _ => {
                    let e = anyhow!(err).context(cpath.to_owned());
                    FileRepositoryError::Other(e)
                }
            }
        }
    }
}

fn as_other_error(err: impl Into<anyhow::Error>) -> FileRepositoryError {
    FileRepositoryError::Other(err.into())
}

impl Config {
    fn get_local_path_from_relative_name(&self, relative_name: &str) -> PathBuf {
        self.root_path.join(relative_name)
    }

    fn is_ignored(&self, relative_name: &str) -> bool {
        for x in self.ignore_prefixes.iter() {
            if relative_name.starts_with(x) {
                return true;
            }
        }

        false
    }
}

impl<H: FileHasher> FileRepository for LocalBackend<H> {
    type Reader = Pin<SemaphoreGuarded<Compat<TokioFile>>>;

    async fn read_file(&self, cpath: &str) -> FileRepositoryResult<(FileInfo, Self::Reader)> {
        let permit = self.aquire_owned_permit().await;
        let me = self.clone();
        let cp = cpath.to_owned();
        let (PathInfo::File(info), file) = spawn_blocking(move || me.read_file_impl(cp))
            .await
            .unwrap()?
        else {
            return Err(FileRepositoryError::IsADirectory {
                cpath: cpath.to_owned(),
            });
        };

        let reader = SemaphoreGuarded {
            permit,
            inner: TokioFile::from_std(file).compat(),
        };

        Ok((info, Pin::new(reader)))
    }

    async fn write_file<R: AsyncRead + Send>(
        &self,
        cpath: &str,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        _len: u64,
    ) -> FileRepositoryResult<FileInfo> {
        let _permit = self.aquire_permit().await;
        let mut src = pin!(src.compat());
        if self.config.is_ignored(cpath) {
            return Err(FileRepositoryError::NotAllowed {
                cpath: cpath.to_owned(),
            });
        }

        // We are optimistic: Start writing the file to a temporary location
        // and maybe fail later when we cannot move it for whatever reason...
        let config = self.config.clone();
        let (mut tmp_file, tmp_file_path) = spawn_blocking(move || {
            // Use the temp file crate to at least more or less make sure that we do not leave the file behind.
            let (f, guard) = tempfile::NamedTempFile::new_in(&config.root_path)
                .expect("failed to create temp file")
                .into_parts();
            (tokio::fs::File::from_std(f), guard)
        })
        .await
        .unwrap();

        log::info!("Writing to temp path {:?}", tmp_file_path);

        tokio::io::copy(&mut src, &mut tmp_file)
            .await
            .map_err(as_other_error)?;
        drop(src);
        tmp_file.sync_all().await.unwrap();

        // We need to set the modified time, and return the files metadata. To do so, go back to std-land.
        let file = tmp_file.into_std().await;
        let me = self.clone();
        let cp = cpath.to_owned();
        let (PathInfo::File(info), _) = spawn_blocking(move || {
            if let Some(modified_time) = modified_time {
                file.set_modified(modified_time.into())
                    .map_err(as_other_error)?;
            }
            me.read_file_impl_for_open_file(cp, file)
        })
        .await
        .unwrap()?
        else {
            unreachable!()
        };

        // Make sure the directories exist..
        let local_path = self.config.get_local_path_from_relative_name(cpath);
        tokio::fs::create_dir_all(local_path.parent().unwrap())
            .await
            .map_err(as_other_error)?;

        // Make sure the file exists..
        let _ = tokio::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(false)
            .open(&local_path)
            .await
            .map_err(|err| map_io_error(err, cpath))?;

        // Actually rename. This should succeed now that we have created the file.
        spawn_blocking(|| tmp_file_path.persist(local_path))
            .await
            .unwrap()
            .map_err(as_other_error)?;

        Ok(info)
    }

    async fn delete_file(&self, cpath: &str) -> FileRepositoryResult<()> {
        let _permit = self.aquire_permit().await;

        let local_path = self.config.get_local_path_from_relative_name(cpath);

        // Pretend it is a file first. If that fails, try to delete a directory there...
        let res = tokio::fs::remove_file(&local_path)
            .await
            .map_err(move |err| map_io_error(err, cpath));

        match res {
            Ok(()) => {}
            Err(FileRepositoryError::IsADirectory { cpath: cp }) if cp == cpath => {
                // Maybe it is a directory?
                tokio::fs::remove_dir(&local_path)
                    .await
                    .map_err(move |err| map_io_error(err, cpath))?;
            }
            Err(err) => return Err(err),
        };

        Ok(())
    }

    async fn set_modified_time(
        &self,
        cpath: &str,
        modtime: OffsetDateTime,
    ) -> FileRepositoryResult<()> {
        let _permit = self.aquire_permit().await;

        let cpath = cpath.to_owned();
        let local_path = self.config.get_local_path_from_relative_name(&cpath);
        spawn_blocking(move || {
            let f = File::open(local_path).map_err(|err| map_io_error(err, &cpath))?;
            f.set_modified(modtime.into()).map_err(as_other_error)?;

            Ok::<_, FileRepositoryError>(())
        })
        .await
        .unwrap()?;

        Ok(())
    }

    async fn read_dir(&self, cpath: &str, up_to_depth: u32) -> FileRepositoryResult<Vec<PathInfo>> {
        // This whole function took less than 1 second to hash over 4000 files (around 6 Gigabytes) on my system.
        // That is fast enough for our purposes. With one core it takes around 6 seconds, so the whole
        // effort to multi-thread the hashing is almost worth it...
        let this = Arc::new(self.clone());
        let cpath = cpath.to_owned();
        let cores = available_parallelism().unwrap().get();

        let mut ret = Vec::new();

        // This listing takes around 8 ms for 4000 files on my system (much faster than I would have expected)
        let local_path = self.config.get_local_path_from_relative_name(&cpath);
        let child_cpaths = {
            let this = this.clone();
            spawn_blocking(move || {
                Self::child_cpaths(&this.config.clone(), local_path, up_to_depth as usize)
            })
            .await
            .unwrap()
        }?;

        let cpu_bound_work = Arc::new(Semaphore::new(cores));
        let mut files_to_be_processed = FuturesUnordered::new();
        for cpath in child_cpaths {
            // Hashing the files is actually quite expensive, so we do that on a thread pool.
            // But we definitely should not run too many of them in parallel.
            // The semaphore prevents that we suddenly hash 4000 files at the same time.
            let this = this.clone();
            let cpu_sem = cpu_bound_work.clone();
            let work = async move {
                let _permit = cpu_sem.acquire().await;
                spawn_blocking(move || this.read_file_impl(cpath))
                    .await
                    .expect("spawn blocking should not fail")
            };
            files_to_be_processed.push(work);
        }

        while let Some(file) = files_to_be_processed.next().await {
            match file {
                Ok((info, _)) => ret.push(info),
                Err(FileRepositoryError::NotAllowed { .. }) => continue,
                Err(err) => return Err(err),
            };
        }

        Ok(ret)
    }

    async fn get_metadata(&self, cpath: &str) -> FileRepositoryResult<PathInfo> {
        let me = self.clone();
        let cpath = cpath.to_owned();
        let (info, _) = spawn_blocking(move || me.read_file_impl(cpath))
            .await
            .unwrap()?;

        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use futures::io::Cursor;

    use super::*;
    use std::{any::Any, fs::OpenOptions, io::Write, path::Path};

    fn default_config(root_path: PathBuf) -> Config {
        Config {
            root_path,
            ignore_prefixes: vec![],
            calculate_hashes: true,
        }
    }

    fn setup_read_dir_test() -> (LocalBackend, Vec<&'static str>, Vec<&'static str>, impl Any) {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            ignore_prefixes: vec!["ignored".into()],
            ..default_config(tmp.path().to_owned())
        };

        let b = LocalBackend::<Sha1Hasher>::from_config(config);

        let mut files = vec![
            "file-1.txt",
            "file-2.txt",
            "file-3.txt",
            "subdir/this-is-a-filename-that-may-or-may-not-be-a-little-longer-than-the-the-other-filenames-because-that-is-really-something-we-should-try-at-some-point.txt",
            "subdir/another-file-but-shorter.bin",
            "anothersubdir/somefile.exe",
            "anothersubdir/d1/d2/d3/ignored"
        ];
        let mut directories = vec![
            "subdir",
            "anothersubdir",
            "anothersubdir/d1",
            "anothersubdir/d1/d2",
            "anothersubdir/d1/d2/d3",
        ];
        directories.sort();
        files.sort();

        let files_should_be = files.clone();

        files.push("ignored/subdir/f1");
        files.push("ignored/f2");

        // Create this directory structure
        for path in files {
            let (dir, filename) = paths::pop_cpath(path);
            let dir_path = Path::new(tmp.path()).join(dir);

            std::fs::create_dir_all(&dir_path).unwrap();

            let path = dir_path.join(filename);

            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .open(path)
                .unwrap();

            write!(f, "Just some data").unwrap();
        }

        (b, files_should_be, directories, tmp)
    }

    #[tokio::test]
    async fn read_dir_depth_1() {
        let (b, files_should_be, dirs_should_be, _drop_later) = setup_read_dir_test();

        // Now read it with read_dir (this should really be done recursively,
        // but the recursive function definition is kind of verbose)
        let mut retrieved_files = vec![];
        let mut retrieved_directories = vec![];

        let mut next_directories = vec!["".to_owned()];
        while let Some(relative_name) = next_directories.pop() {
            for info in b.read_dir(&relative_name, 1).await.unwrap() {
                match info {
                    PathInfo::Dir(d) => {
                        next_directories.push(d.cpath.to_owned());
                        retrieved_directories.push(d.cpath);
                    }
                    PathInfo::File(info) => retrieved_files.push(info.cpath.to_owned()),
                }
            }
        }

        retrieved_files.sort();
        retrieved_directories.sort();

        assert_eq!(retrieved_files, files_should_be);
        assert_eq!(retrieved_directories, dirs_should_be);
    }

    #[tokio::test]
    async fn read_dir_max_depth() {
        let (b, files_should_be, dirs_should_be, _drop_later) = setup_read_dir_test();

        let retrieved = b.read_dir("", u32::MAX).await.unwrap();

        let mut retrieved_files = vec![];
        let mut retrieved_dirs = vec![];
        for info in retrieved {
            match info {
                PathInfo::Dir(DirectoryInfo { cpath }) => retrieved_dirs.push(cpath),
                PathInfo::File(FileInfo { cpath, .. }) => retrieved_files.push(cpath),
            }
        }
        retrieved_dirs.sort();
        retrieved_files.sort();

        assert_eq!(retrieved_files, files_should_be);
        assert_eq!(retrieved_dirs, dirs_should_be);
    }

    #[tokio::test]
    async fn read_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            ..default_config(tmp.path().to_owned())
        };
        let b = LocalBackend::<Sha1Hasher>::from_config(config);

        let test_bytes = (0..1_000_000u64).map(|x| x as u8).collect::<Vec<_>>();

        let relative_name = "dir/subdir/a/b/somefile";
        let fpath = tmp.path().join("dir/subdir/a/b/somefile");
        let fdir = tmp.path().join("dir/subdir/a/b");

        std::fs::create_dir_all(fdir).unwrap();

        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(fpath)
            .unwrap();

        f.write_all(&test_bytes).unwrap();
        drop(f);

        let (info, mut reader) = b.read_file(relative_name).await.unwrap();
        assert_eq!(test_bytes.len() as u64, info.size);

        let mut file_bytes = Vec::new();
        futures::io::copy(&mut reader, &mut file_bytes)
            .await
            .unwrap();
        assert_eq!(file_bytes, test_bytes);
    }

    #[tokio::test]
    async fn file_ignores_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            ignore_prefixes: vec!["dir/subdir".into()],
            ..default_config(tmp.path().to_owned())
        });
        let res = b.read_file("dir/subdir/a/b/somefile").await;

        let error = res.unwrap_err();
        assert!(matches!(error, FileRepositoryError::NotAllowed { .. }));
    }

    #[tokio::test]
    async fn write_file() {
        let tmp = tempfile::tempdir().unwrap();
        let b: LocalBackend<Sha1Hasher> = LocalBackend::from_config(Config {
            ignore_prefixes: vec!["dir/we/ignore".into()],
            ..default_config(tmp.path().to_owned())
        });
        let test_bytes = (0..1_000_000u64).map(|x| x as u8).collect::<Vec<_>>();

        // Test if we can write to ignored locations
        let x = b
            .write_file("dir/we/ignore/subdir/test", None, futures::io::empty(), 0)
            .await;
        let error = x.unwrap_err();
        assert!(matches!(error, FileRepositoryError::NotAllowed { .. }));

        // Now write an actual file
        let relative_name = "dir/that-does-not-exist/somefile";
        let fpath = tmp.path().join(relative_name);

        let modtime_should_be = OffsetDateTime::now_utc();
        b.write_file(
            "dir/that-does-not-exist/somefile",
            None,
            Cursor::new(&test_bytes),
            test_bytes.len() as u64,
        )
        .await
        .unwrap();

        b.set_modified_time("dir/that-does-not-exist/somefile", modtime_should_be)
            .await
            .unwrap();

        let mut f = File::open(fpath).unwrap();

        let mut file_bytes: Vec<u8> = Vec::new();
        std::io::copy(&mut f, &mut file_bytes).unwrap();

        assert_eq!(file_bytes.len(), test_bytes.len());
        assert_eq!(file_bytes, test_bytes);

        let modtime = OffsetDateTime::from(f.metadata().unwrap().modified().unwrap());
        assert_eq!(modtime, modtime_should_be);
    }

    #[tokio::test]
    async fn write_file_ignores_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let b: LocalBackend<Sha1Hasher> = LocalBackend::from_config(Config {
            root_path: tmp.path().to_owned(),
            ignore_prefixes: vec!["dir/subdir".into()],
            ..default_config(tmp.path().to_owned())
        });

        let res = b
            .write_file("dir/subdir/test", None, futures::io::empty(), 0)
            .await;
        let error = res.unwrap_err();
        assert!(matches!(error, FileRepositoryError::NotAllowed { .. }));
    }

    #[tokio::test]
    async fn info_ignores_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            root_path: tmp.path().to_owned(),
            ignore_prefixes: vec!["dir/subdir".into()],
            ..default_config(tmp.path().to_owned())
        });

        // Create the file manually to really test it..
        let ignored_dir = tmp.path().join("dir/subdir");
        let ignored_file_path = ignored_dir.join("test");

        std::fs::create_dir_all(&ignored_dir).unwrap();
        let mut f = File::options()
            .write(true)
            .create(true)
            .open(ignored_file_path)
            .unwrap();
        write!(f, "test").unwrap();
        drop(f);

        let res = b.get_metadata("dir/subdir/test").await;
        let err = res.unwrap_err();
        assert!(matches!(err, FileRepositoryError::NotAllowed { .. }));
    }

    #[tokio::test]
    async fn hashes_can_be_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            root_path: tmp.path().to_owned(),
            ignore_prefixes: vec!["dir/subdir".into()],
            calculate_hashes: false,
        });

        let mut cursor = Cursor::new(vec![0xde, 0xad]);
        b.write_file("test", None, &mut cursor, 2).await.unwrap();

        let info = b.get_metadata("test").await.unwrap();
        match info {
            PathInfo::File(file_info) => assert!(file_info.hash.is_empty()),
            _ => panic!("Expected a file"),
        };
    }

    #[tokio::test]
    async fn read_dir_errors_on_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(default_config(
            tmp.path().join("should-not-exist"),
        ));
        let res = b.read_dir("should-not-exist", 0).await;

        let Err(FileRepositoryError::NotFound { cpath }) = res else {
            panic!("Got {res:#?}");
        };
        assert_eq!(cpath, "should-not-exist");
    }

    #[tokio::test]
    async fn write_file_on_directory() {
        // Check that if write_file is called on a path that exists but is a directory
        // returns Error::IsADirectory.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("testdir").join("nested")).unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            ..default_config(tmp.path().to_owned())
        });

        let result = b
            .write_file("testdir/nested", None, futures::io::empty(), 0)
            .await
            .unwrap_err();
        let FileRepositoryError::IsADirectory { cpath } = result else {
            panic!("Unexpectedly got {:?}", result);
        };
        assert_eq!(cpath, "testdir/nested");
    }

    #[tokio::test]
    async fn read_file_on_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("testdir").join("nested")).unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            ..default_config(tmp.path().to_owned())
        });

        let result = b.read_file("testdir/nested").await.unwrap_err();
        let FileRepositoryError::IsADirectory { cpath } = result else {
            panic!("Unexpectedly got {:?}", result);
        };
        assert_eq!(cpath, "testdir/nested");
    }

    #[tokio::test]
    async fn deleting_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let test_path = tmp.path().join("testdir");
        std::fs::create_dir(&test_path).unwrap();
        assert!(test_path.exists());

        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            ..default_config(tmp.path().to_owned())
        });

        b.delete_file("testdir").await.unwrap();

        assert!(!test_path.exists());
    }
}
