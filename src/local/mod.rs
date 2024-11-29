use anyhow::anyhow;
use anyhow::bail;
use futures::TryFutureExt;
use walkdir::WalkDir;

use crate::file_repository::Error;
use crate::file_repository::Result;
use crate::filetree::FileTree;
use crate::paths::RPath;
use crate::paths::RPathBuf;
use crate::{file_repository::FileRepository, DirectoryInfo, FileInfo, PathInfo};
use futures::AsyncRead;
use std::path::Path as StdPath;
use std::{
    fs::File,
    io::{ErrorKind, Seek},
    path::PathBuf,
    pin::pin,
    sync::Arc,
};
use tokio::fs::File as TokioFile;
use tokio::task::spawn_blocking;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

pub mod config;
pub mod file_hasher;
pub use config::*;

#[derive(Debug, Clone)]
pub struct LocalBackend {
    config: Arc<Config>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// A path on the local file system. Note that although from this type definition
    /// it may look like we support non-valid-unicode paths, this is not the case
    /// and will panic at runtime.
    pub root_path: PathBuf,

    /// If a RPath starts with a prefix listed here, it is ignored
    pub ignore_prefixes: Vec<RPathBuf>,
}

impl LocalBackend {
    pub fn from_config(config: Config) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl LocalBackend {
    /// path is still needed because it is returned as part of the metadata. Otherwise, it is ignored.
    ///
    /// Note that this function might calculate the hash of the file and is therefore somewhat CPU bound. You should
    /// take care to not overwhelm the spawn_blocking executor with too many concurrent calls to this function.
    fn read_file_impl_for_open_file(
        &self,
        path: RPathBuf,
        mut file: File,
    ) -> Result<(PathInfo, File)> {
        let metadata = file.metadata().map_err(map_io_error)?;
        let res = if metadata.is_file() {
            let info = FileInfo {
                path,
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .expect("system should have modification times")
                    .into(),
            };

            file.rewind()?;
            (PathInfo::File(info), file)
        } else if metadata.is_dir() {
            (
                PathInfo::Dir(DirectoryInfo {
                    path: path.to_owned(),
                }),
                file,
            )
        } else {
            bail!("symlinks cannot be opened (at {path:?})")
        };

        Ok(res)
    }

    fn read_file_impl(&self, path: RPathBuf) -> Result<(PathInfo, File)> {
        if self.config.is_ignored(&path) {
            bail!(Error::NotAllowed);
        }
        let local_path = self.config.get_local_path(&path);

        // Open it, to avoid the time-of-check-time-of-use race condition. Note that
        // one can actually open directories.
        let file = File::open(local_path).map_err(map_io_error)?;
        self.read_file_impl_for_open_file(path, file)
    }
}

fn map_io_error(err: std::io::Error) -> anyhow::Error {
    match err.kind() {
        ErrorKind::NotFound => Error::NotFound.into(),
        ErrorKind::PermissionDenied => Error::NotAllowed.into(),
        _ => {
            // It is actually extremely unfortunate that more error kinds are still not
            // stabilized. In the meantime, we really want the IsADirectory and IsNotADirectory
            // so we do them ourselves here...
            // TODO: Change this when Rust 1.83 lands
            match err.raw_os_error() {
                Some(libc::ENOTDIR) => Error::NotADirectory.into(),
                Some(libc::EISDIR) => Error::IsADirectory.into(),
                _ => anyhow!(err),
            }
        }
    }
}

impl Config {
    fn get_local_path(&self, path: &RPath) -> PathBuf {
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

    fn get_path(&self, local_path: impl AsRef<StdPath>) -> anyhow::Result<RPathBuf> {
        RPathBuf::try_from_path(&self.root_path, local_path.as_ref())
    }
}

impl FileRepository for LocalBackend {
    type Reader = Compat<TokioFile>;
    const CONCURRENCY_LIMIT: usize = 16;

    async fn read_file(&self, path: &RPath) -> Result<(FileInfo, Self::Reader)> {
        let me = self.clone();
        let cp = path.to_owned();
        let (PathInfo::File(info), file) = spawn_blocking(move || me.read_file_impl(cp))
            .await
            .unwrap()?
        else {
            bail!(Error::IsADirectory)
        };

        Ok((info, TokioFile::from_std(file).compat()))
    }

    async fn mkdir(&self, path: &RPath) -> Result<DirectoryInfo> {
        let local_path = self.config.get_local_path(path);
        tokio::fs::create_dir(local_path)
            .await
            .map_err(map_io_error)?;

        Ok(DirectoryInfo {
            path: path.to_owned(),
        })
    }

    async fn write_file<R: AsyncRead + Send>(
        &self,
        path: &RPath,
        modified_time: Option<time::OffsetDateTime>,
        src: R,
        _len: u64,
    ) -> Result<FileInfo> {
        let mut src = pin!(src.compat());
        if self.config.is_ignored(path) {
            bail!(Error::NotAllowed);
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

        tokio::io::copy(&mut src, &mut tmp_file).await?;

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
                file.set_modified(modified_time.into())?;
            }
            me.read_file_impl_for_open_file(cp, file)
        })
        .await
        .unwrap()?
        else {
            unreachable!()
        };

        // Make sure the directories exist..
        let local_path = self.config.get_local_path(path);

        // Make sure the file exists..
        let _ = tokio::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(false)
            .open(&local_path)
            .map_err(map_io_error)
            .await?;

        // Actually rename. This should succeed now that we have created the file.
        spawn_blocking(|| tmp_file_path.persist(local_path))
            .await
            .unwrap()?;

        Ok(info)
    }

    async fn delete(&self, path: &RPath) -> Result<()> {
        let local_path = self.config.get_local_path(path);
        let f = tokio::fs::File::open(&local_path).await?;
        let meta = f.metadata().await?;

        if meta.is_dir() {
            drop(f);
            tokio::fs::remove_dir_all(local_path).await?;
        } else if meta.is_file() {
            tokio::fs::remove_file(local_path).await?;
        } else {
            bail!(Error::NotAllowed)
        }

        Ok(())
    }

    async fn file_tree(&self, path: &RPath, up_to_depth: usize) -> Result<FileTree<PathInfo>> {
        let mut ret = Vec::new();
        let filter_path = &self.config.get_local_path(&path);

        let walker = WalkDir::new(&self.config.root_path)
            .max_depth(up_to_depth as usize)
            .into_iter()
            .filter_entry(|x| {
                x.path().starts_with(filter_path) || filter_path.starts_with(x.path())
            });

        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    let err = if err.io_error().is_some() {
                        let mapped_io_error = map_io_error(err.into_io_error().unwrap());
                        anyhow!(mapped_io_error)
                    } else {
                        anyhow!(err)
                    };

                    return Err(err.context(path.to_owned()));
                }
            };

            let entry_path = self.config.get_path(entry.path())?;

            // Note: This means that we just return an empty list when this is called with a directory
            // that we are not allowed to access (is that okay?)
            if self.config.is_ignored(&entry_path) {
                continue;
            }

            if entry.file_type().is_dir() {
                ret.push(PathInfo::Dir(DirectoryInfo { path: entry_path }))
            } else if entry.file_type().is_file() {
                let meta = entry.metadata()?;

                let info = FileInfo {
                    path: entry_path,
                    size: meta.len(),
                    modified: meta
                        .modified()
                        .expect("system should have modification times")
                        .into(),
                };
                ret.push(PathInfo::File(info));
            } else {
                // We ignore symlinks
                continue;
            }
        }

        Ok(ret.try_into().expect("these paths form a tree"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use futures::io::Cursor;
    use std::{any::Any, fs::OpenOptions, io::Write, path::Path, usize};
    use time::OffsetDateTime;

    fn default_config(root_path: PathBuf) -> Config {
        Config {
            root_path,
            ignore_prefixes: vec![],
        }
    }

    fn setup_read_dir_test() -> (LocalBackend, Vec<&'static str>, Vec<&'static str>, impl Any) {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            ignore_prefixes: vec![["ignored"].into()],
            ..default_config(tmp.path().to_owned())
        };

        let b = LocalBackend::from_config(config);

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
            "",
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
    async fn file_tree_returns_partial_trees() {
        let (b, _, _, _drop_later) = setup_read_dir_test();
        let tree = b
            .file_tree("anothersubdir/d1/d2/does-not-exist".into(), usize::MAX)
            .await
            .unwrap();

        let nodes = tree.as_slice().iter().map(|x| x.path()).collect::<Vec<_>>();
        let expected: Vec<&'static RPath> = vec![
            "".into(),
            "anothersubdir".into(),
            "anothersubdir/d1".into(),
            "anothersubdir/d1/d2".into(),
        ];

        assert_eq!(nodes, expected);
    }

    #[tokio::test]
    async fn read_dir_depth_1() {
        let (b, files_should_be, dirs_should_be, _drop_later) = setup_read_dir_test();

        // Now read it with read_dir (this should really be done recursively,
        // but the recursive function definition is kind of verbose)
        let mut retrieved_files = vec![];
        let mut retrieved_directories = vec![String::new()];

        let mut next_directories = vec![RPath::empty().to_owned()];
        while let Some(relative_name) = next_directories.pop() {
            let path: RPathBuf = relative_name.try_into().unwrap();

            for info in b.list_files(&path, 1).await.unwrap() {
                if info.path() == path.as_rpath() {
                    continue;
                }

                match info {
                    PathInfo::Dir(d) => {
                        retrieved_directories.push(d.path.with_separator('/').unwrap());
                        next_directories.push(d.path);
                    }
                    PathInfo::File(info) => {
                        retrieved_files.push(info.path.with_separator('/').unwrap())
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

        let retrieved = b.list_files(RPath::empty(), usize::MAX).await.unwrap();

        let mut retrieved_files = vec![];
        let mut retrieved_dirs = vec![];
        for info in retrieved {
            match info {
                PathInfo::Dir(DirectoryInfo { path }) => {
                    retrieved_dirs.push(path.with_separator('/').unwrap())
                }
                PathInfo::File(FileInfo { path, .. }) => {
                    retrieved_files.push(path.with_separator('/').unwrap())
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
        let b = LocalBackend::from_config(config);

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
        let b = LocalBackend::from_config(Config {
            ignore_prefixes: vec![RPathBuf::from_components(["dir", "subdir"])],
            ..default_config(tmp.path().to_owned())
        });
        let path: RPathBuf = RPathBuf::from_sep("dir/subdir/a/b/somefile", '/');
        let res = b.read_file(&path).await;

        let error = res.unwrap_err();
        assert_matches!(error.downcast().unwrap(), Error::NotAllowed);
    }

    #[tokio::test]
    async fn write_file() {
        let tmp = tempfile::tempdir().unwrap();
        let b: LocalBackend = LocalBackend::from_config(Config {
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
        assert_matches!(error.downcast().unwrap(), Error::NotAllowed);

        // Now write an actual file
        let fpath = tmp.path().join("a-directory/somefile");

        let modtime_should_be = OffsetDateTime::now_utc();
        b.mkdir(&RPathBuf::from(["a-directory"])).await.unwrap();
        b.write_file(
            &RPathBuf::from(["a-directory", "somefile"]),
            Some(modtime_should_be),
            Cursor::new(&test_bytes),
            test_bytes.len() as u64,
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
        let b: LocalBackend = LocalBackend::from_config(Config {
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
        assert_matches!(error.downcast().unwrap(), Error::NotAllowed);
    }

    #[tokio::test]
    async fn info_ignores_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::from_config(Config {
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
            .read_file(&RPathBuf::from(["dir", "subdir", "test"]))
            .await;

        assert_matches!(
            res.unwrap_err().downcast().unwrap(),
            Error::NotAllowed { .. }
        );
    }

    #[tokio::test]
    async fn info_ignores_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::from_config(Config {
            root_path: tmp.path().to_owned(),
            ignore_prefixes: vec![RPathBuf::from_components(["dir", "subdir"])],
            ..default_config(tmp.path().to_owned())
        });
        let ignored_dir = tmp.path().join("dir/subdir");
        std::fs::create_dir_all(&ignored_dir).unwrap();

        let res = b.read_file(&RPathBuf::from(["dir", "subdir"])).await;
        assert!(matches!(
            res.unwrap_err().downcast().unwrap(),
            Error::NotAllowed
        ));
    }

    #[tokio::test]
    async fn list_files_errors_on_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::from_config(default_config(tmp.path().to_owned()));
        let res = b
            .list_files("should-not-exist".into(), 0)
            .await
            .unwrap_err();

        println!("{res:?}");
        assert_matches!(res.downcast().unwrap(), Error::NotFound);
    }

    #[tokio::test]
    async fn write_file_on_directory() {
        // Check that if write_file is called on a path that exists but is a directory
        // returns Error::IsADirectory.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("testdir").join("nested")).unwrap();
        let b = LocalBackend::from_config(Config {
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

        assert_matches!(result.downcast().unwrap(), Error::IsADirectory);
    }

    #[tokio::test]
    async fn read_file_on_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("testdir").join("nested")).unwrap();
        let b = LocalBackend::from_config(Config {
            ..default_config(tmp.path().to_owned())
        });

        let result = b
            .read_file(&RPathBuf::from(["testdir", "nested"]))
            .await
            .unwrap_err();

        assert_matches!(result.downcast().unwrap(), Error::IsADirectory);
    }

    #[tokio::test]
    async fn deleting_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let test_path = tmp.path().join("testdir");
        std::fs::create_dir(&test_path).unwrap();
        assert!(test_path.exists());

        let b = LocalBackend::from_config(Config {
            ..default_config(tmp.path().to_owned())
        });

        b.delete(&RPathBuf::from_components(["testdir"]))
            .await
            .unwrap();

        assert!(!test_path.exists());
    }
}
