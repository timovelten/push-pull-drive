pub mod config;
pub mod file_hasher;
use anyhow::anyhow;
pub use config::*;
use walkdir::WalkDir;

use self::file_hasher::{FileHasher, Sha1Hasher};
use crate::file_repository::Error as FileRepositoryError;
use crate::file_repository::Result as FileRepositoryResult;
use crate::paths::RPath;
use crate::paths::RPathBuf;
use crate::{file_repository::FileRepository, DirectoryInfo, FileInfo, PathInfo};
use futures::{stream::FuturesUnordered, AsyncRead, StreamExt};
use std::{
    fs::File,
    io::{ErrorKind, Seek},
    path::{Path, PathBuf},
    pin::pin,
    sync::Arc,
    thread::available_parallelism,
};
use time::OffsetDateTime;
use tokio::fs::File as TokioFile;
use tokio::{sync::Semaphore, task::spawn_blocking};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

#[derive(Debug, Clone)]
pub struct LocalBackend<H = Sha1Hasher> {
    config: Arc<Config>,
    hasher: H,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// A path on the local file system. Note that although from this type definition
    /// it may look like we support non-valid-unicode paths, this is not the case
    /// and will panic at runtime.
    pub root_path: PathBuf,

    /// If a RPath starts with a prefix listed here, it is ignored
    pub ignore_prefixes: Vec<RPathBuf>,

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
            hasher,
        }
    }

    /// path is still needed because it is returned as part of the metadata. Otherwise, it is ignored.
    ///
    /// Note that this function might calculate the hash of the file and is therefore somewhat CPU bound. You should
    /// take care to not overwhelm the spawn_blocking executor with too many concurrent calls to this function.
    fn read_file_impl_for_open_file(
        &self,
        path: RPathBuf,
        mut file: File,
    ) -> FileRepositoryResult<(PathInfo, File)> {
        let metadata = file.metadata().map_err(|err| map_io_error(err, &path))?;
        let res = if metadata.is_file() {
            let hash = if self.config.calculate_hashes {
                file.rewind().map_err(as_other_error)?;
                self.hasher
                    .hash(&mut file)
                    .map_err(|err| map_io_error(err, &path))?
            } else {
                Vec::new()
            };

            let info = FileInfo {
                rpath: path,
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
                    rpath: path.to_owned(),
                }),
                file,
            )
        } else {
            // For now, we ignore symlinks by returning NotAllowed...
            return Err(FileRepositoryError::NotAllowed {
                rpath: path.to_owned(),
            });
        };

        Ok(res)
    }

    fn read_file_impl(&self, path: RPathBuf) -> FileRepositoryResult<(PathInfo, File)> {
        if self.config.is_ignored(&path) {
            return Err(FileRepositoryError::NotAllowed {
                rpath: path.to_owned(),
            });
        }
        let local_path = self.config.get_local_path_from_rpath(&path);

        // Open it, to avoid the time-of-check-time-of-use race condition. Note that
        // one can actually open directories.
        let file = File::open(local_path).map_err(|err| map_io_error(err, &path))?;
        self.read_file_impl_for_open_file(path, file)
    }

    fn read_dir_impl(
        config: &Arc<Config>,
        path: impl AsRef<Path>,
        max_depth: usize,
    ) -> FileRepositoryResult<Vec<RPathBuf>> {
        let mut ret = Vec::new();

        let walker = WalkDir::new(path).max_depth(max_depth);
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    let error_depth = err.depth();
                    let error_rpath = if let Some(err_path) = err.path() {
                        RPathBuf::try_from_path(&config.root_path, err_path).map_err(|err| {
                            let err = anyhow!(err).context(format!(
                                "Error occurred at depth {} for error path {err_path:?}",
                                error_depth
                            ));
                            FileRepositoryError::Other(err)
                        })?
                    } else {
                        RPathBuf::empty()
                    };

                    let err = map_io_error(
                        err.into_io_error().expect("should be io error"),
                        &error_rpath,
                    );
                    return Err(err);
                }
            };

            if entry.depth() == 0 {
                continue;
            }

            let child_rpath =
                RPathBuf::try_from_path(&config.root_path, entry.path()).map_err(as_other_error)?;

            ret.push(child_rpath);
        }

        Ok(ret)
    }
}

fn map_io_error(err: std::io::Error, path: &RPath) -> FileRepositoryError {
    if path.is_empty() {
        return as_other_error(err);
    }

    match err.kind() {
        ErrorKind::NotFound => FileRepositoryError::NotFound {
            rpath: path.to_owned(),
        },
        ErrorKind::PermissionDenied => FileRepositoryError::NotAllowed {
            rpath: path.to_owned(),
        },
        _ => {
            // It is actually extremely unfortunate that more error kinds are still not
            // stabilized. In the meantime, we really want the IsADirectory and IsNotADirectory
            // so we do them ourselves here...
            match err.raw_os_error() {
                Some(libc::ENOTDIR) => FileRepositoryError::NotADirectory {
                    rpath: path.to_owned(),
                },
                Some(libc::EISDIR) => FileRepositoryError::IsADirectory {
                    rpath: path.to_owned(),
                },
                _ => {
                    let e = anyhow!(err).context(path.to_owned());
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
    fn get_local_path_from_rpath(&self, path: &RPath) -> PathBuf {
        let slash_separated_path = path
            .with_separator('/')
            .expect("directory and filenames should not contain slashes");
        self.root_path.join(slash_separated_path)
    }

    fn is_ignored(&self, path: &RPath) -> bool {
        for prefix in self.ignore_prefixes.iter() {
            if path.starts_with(prefix) {
                return true;
            }
        }

        false
    }
}

impl<H: FileHasher> FileRepository for LocalBackend<H> {
    type Reader = Compat<TokioFile>;
    const CONCURRENCY_LIMIT: usize = 16;

    async fn read_file(&self, path: &RPath) -> FileRepositoryResult<(FileInfo, Self::Reader)> {
        let me = self.clone();
        let cp = path.to_owned();
        let (PathInfo::File(info), file) = spawn_blocking(move || me.read_file_impl(cp))
            .await
            .unwrap()?
        else {
            return Err(FileRepositoryError::IsADirectory {
                rpath: path.to_owned(),
            });
        };

        Ok((info, TokioFile::from_std(file).compat()))
    }

    async fn write_file<R: AsyncRead + Send>(
        &self,
        path: &RPath,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        _len: u64,
    ) -> FileRepositoryResult<FileInfo> {
        let mut src = pin!(src.compat());
        if self.config.is_ignored(path) {
            return Err(FileRepositoryError::NotAllowed {
                rpath: path.to_owned(),
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

        // Drop the source as early as possible. It is conceivable that our call tracks how long
        // the source reader is around (via the drop trait) and maybe terminate a connection or something.
        // (Though Clippy complains because, currently, all types we use this with have trivial drop implementation)
        #[allow(clippy::drop_non_drop)]
        drop(src);

        tmp_file.sync_all().await.unwrap();

        // We need to set the modified time, and return the files metadata. To do so, go back to std-land.
        let file = tmp_file.into_std().await;
        let me = self.clone();
        let cp = path.to_owned();
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
        let local_path = self.config.get_local_path_from_rpath(path);
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
            .map_err(|err| map_io_error(err, path))?;

        // Actually rename. This should succeed now that we have created the file.
        spawn_blocking(|| tmp_file_path.persist(local_path))
            .await
            .unwrap()
            .map_err(as_other_error)?;

        Ok(info)
    }

    async fn delete_file(&self, path: &RPath) -> FileRepositoryResult<()> {
        let local_path = self.config.get_local_path_from_rpath(path);

        // Pretend it is a file first. If that fails, try to delete a directory there...
        let res = tokio::fs::remove_file(&local_path)
            .await
            .map_err(move |err| map_io_error(err, path));

        match res {
            Ok(()) => {}
            Err(FileRepositoryError::IsADirectory { rpath: cp }) if &*cp == path => {
                // Maybe it is a directory?
                tokio::fs::remove_dir(&local_path)
                    .await
                    .map_err(move |err| map_io_error(err, path))?;
            }
            Err(err) => return Err(err),
        };

        Ok(())
    }

    async fn set_modified_time(
        &self,
        cpath: &RPath,
        modtime: OffsetDateTime,
    ) -> FileRepositoryResult<()> {
        let cpath = cpath.to_owned();
        let local_path = self.config.get_local_path_from_rpath(&cpath);
        spawn_blocking(move || {
            let f = File::open(local_path).map_err(|err| map_io_error(err, &cpath))?;
            f.set_modified(modtime.into()).map_err(as_other_error)?;

            Ok::<_, FileRepositoryError>(())
        })
        .await
        .unwrap()?;

        Ok(())
    }

    async fn read_dir(
        &self,
        cpath: &RPath,
        up_to_depth: u32,
    ) -> FileRepositoryResult<Vec<PathInfo>> {
        // This whole function took less than 1 second to hash over 4000 files (around 6 Gigabytes) on my system.
        // That is fast enough for our purposes. With one core it takes around 6 seconds, so the whole
        // effort to multi-thread the hashing is almost worth it...
        let this = Arc::new(self.clone());
        let cpath = cpath.to_owned();
        let cores = available_parallelism().unwrap().get();

        let mut ret = Vec::new();

        // This listing takes around 8 ms for 4000 files on my system (much faster than I would have expected)
        let local_path = self.config.get_local_path_from_rpath(&cpath);
        let child_cpaths = {
            let this = this.clone();
            spawn_blocking(move || {
                Self::read_dir_impl(&this.config.clone(), local_path, up_to_depth as usize)
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
                    .unwrap()
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

    async fn get_metadata(&self, cpath: &RPath) -> FileRepositoryResult<PathInfo> {
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
    use super::*;
    use futures::io::Cursor;
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
            ignore_prefixes: vec![["ignored"].into()],
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
            let (dir, filename) = match path.rsplit_once('/') {
                Some((a, b)) => (a, b),
                None => ("", path),
            };

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

        let mut next_directories = vec![RPath::empty().to_owned()];
        while let Some(relative_name) = next_directories.pop() {
            let path: RPathBuf = relative_name.try_into().unwrap();

            for info in b.read_dir(&path, 1).await.unwrap() {
                match info {
                    PathInfo::Dir(d) => {
                        retrieved_directories.push(d.rpath.with_separator('/').unwrap());
                        next_directories.push(d.rpath);
                    }
                    PathInfo::File(info) => {
                        retrieved_files.push(info.rpath.with_separator('/').unwrap())
                    }
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

        let retrieved = b.read_dir(RPath::empty(), u32::MAX).await.unwrap();

        let mut retrieved_files = vec![];
        let mut retrieved_dirs = vec![];
        for info in retrieved {
            match info {
                PathInfo::Dir(DirectoryInfo { rpath }) => {
                    retrieved_dirs.push(rpath.with_separator('/').unwrap())
                }
                PathInfo::File(FileInfo { rpath, .. }) => {
                    retrieved_files.push(rpath.with_separator('/').unwrap())
                }
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

        let the_path: RPathBuf = RPathBuf::from_components(["dir", "subdir", "a", "b", "somefile"]);
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

        let (info, mut reader) = b.read_file(&the_path).await.unwrap();
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
            ignore_prefixes: vec![RPathBuf::from_components(["dir", "subdir"])],
            ..default_config(tmp.path().to_owned())
        });
        let path: RPathBuf = RPathBuf::from_sep("dir/subdir/a/b/somefile", '/');
        let res = b.read_file(&path).await;

        let error = res.unwrap_err();
        assert!(matches!(error, FileRepositoryError::NotAllowed { .. }));
    }

    #[tokio::test]
    async fn write_file() {
        let tmp = tempfile::tempdir().unwrap();
        let b: LocalBackend<Sha1Hasher> = LocalBackend::from_config(Config {
            ignore_prefixes: vec![RPathBuf::from_components(["dir", "we", "ignore"])],
            ..default_config(tmp.path().to_owned())
        });
        let test_bytes = (0..1_000_000u64).map(|x| x as u8).collect::<Vec<_>>();

        // Test if we can write to ignored locations
        let x = b
            .write_file(
                &RPathBuf::from(["dir", "we", "ignore", "subdir", "test"]),
                None,
                futures::io::empty(),
                0,
            )
            .await;
        let error = x.unwrap_err();
        assert!(matches!(error, FileRepositoryError::NotAllowed { .. }));

        // Now write an actual file
        let fpath = tmp.path().join("dir/that-does-not-exist/somefile");

        let modtime_should_be = OffsetDateTime::now_utc();
        b.write_file(
            &RPathBuf::from(["dir", "that-does-not-exist", "somefile"]),
            None,
            Cursor::new(&test_bytes),
            test_bytes.len() as u64,
        )
        .await
        .unwrap();

        b.set_modified_time(
            &RPathBuf::from(["dir", "that-does-not-exist", "somefile"]),
            modtime_should_be,
        )
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
            ignore_prefixes: vec![RPathBuf::from_components(["dir", "subdir"])],
            ..default_config(tmp.path().to_owned())
        });

        let res = b
            .write_file(
                &RPathBuf::from(["dir", "subdir", "test"]),
                None,
                futures::io::empty(),
                0,
            )
            .await;
        let error = res.unwrap_err();
        assert!(matches!(error, FileRepositoryError::NotAllowed { .. }));
    }

    #[tokio::test]
    async fn info_ignores_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            root_path: tmp.path().to_owned(),
            ignore_prefixes: vec![RPathBuf::from_components(["dir", "subdir"])],
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

        let res = b
            .get_metadata(&RPathBuf::from(["dir", "subdir", "test"]))
            .await;

        assert!(matches!(res, Err(FileRepositoryError::NotAllowed { .. })));
    }

    #[tokio::test]
    async fn info_ignores_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            root_path: tmp.path().to_owned(),
            ignore_prefixes: vec![RPathBuf::from_components(["dir", "subdir"])],
            ..default_config(tmp.path().to_owned())
        });
        let ignored_dir = tmp.path().join("dir/subdir");
        std::fs::create_dir_all(&ignored_dir).unwrap();

        let res = b
            .get_metadata(&RPathBuf::from(["dir", "subdir"]))
            .await;
        assert!(matches!(res, Err(FileRepositoryError::NotAllowed { .. })));
    }

    #[tokio::test]
    async fn hashes_can_be_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            root_path: tmp.path().to_owned(),
            ignore_prefixes: vec![],
            calculate_hashes: false,
        });

        let mut cursor = Cursor::new(vec![0xde, 0xad]);
        let path: RPathBuf = ["test"].into();
        b.write_file(&path, None, &mut cursor, 2).await.unwrap();

        let info = b.get_metadata(&path).await.unwrap();
        match info {
            PathInfo::File(file_info) => assert!(file_info.hash.is_empty()),
            _ => panic!("Expected a file"),
        };
    }

    #[tokio::test]
    async fn read_dir_errors_on_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = RPathBuf::from_sep("should-not-exist", '/');
        let b = LocalBackend::<Sha1Hasher>::from_config(default_config(tmp.path().to_owned()));
        let res = b.read_dir(&path, 0).await;

        let Err(FileRepositoryError::NotFound { rpath }) = res else {
            panic!("Got {res:#?}");
        };
        assert_eq!(rpath, path);
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
            .write_file(
                &RPathBuf::from(["testdir", "nested"]),
                None,
                futures::io::empty(),
                0,
            )
            .await
            .unwrap_err();
        let FileRepositoryError::IsADirectory { rpath } = result else {
            panic!("Unexpectedly got {:?}", result);
        };
        assert_eq!(rpath, ["testdir", "nested"].into());
    }

    #[tokio::test]
    async fn read_file_on_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("testdir").join("nested")).unwrap();
        let b = LocalBackend::<Sha1Hasher>::from_config(Config {
            ..default_config(tmp.path().to_owned())
        });

        let result = b
            .read_file(&RPathBuf::from(["testdir", "nested"]))
            .await
            .unwrap_err();
        let FileRepositoryError::IsADirectory { rpath } = result else {
            panic!("Unexpectedly got {:?}", result);
        };
        assert_eq!(rpath, ["testdir", "nested"].into());
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

        b.delete_file(&RPathBuf::from_components(["testdir"]))
            .await
            .unwrap();

        assert!(!test_path.exists());
    }
}
