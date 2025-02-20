use anyhow::anyhow;
use anyhow::bail;
use futures::TryFutureExt;
use serde::Deserialize;
use serde::Serialize;
use walkdir::WalkDir;

use crate::file_repository::Error;
use crate::file_repository::Result;
use crate::paths::RPath;
use crate::paths::RPathBuf;
use crate::{file_repository::FileRepository, FileInfo, PathInfo};
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
pub use config::*;

#[derive(Debug, Clone)]
pub struct LocalBackend {
    config: Arc<Config>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// A path on the local file system. Note that although from this type definition
    /// it may look like we support non-valid-unicode paths, this is not the case
    /// and will panic at runtime.
    pub root_path: PathBuf,

    /// If a RPath starts with a prefix listed here, it is ignored
    pub ignore_prefixes: Vec<RPathBuf>,
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
impl LocalBackend {
    pub fn from_config(config: Config) -> Self {
        Self {
            config: Arc::new(config),
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
            (PathInfo::Dir(path), file)
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

    async fn write_file_impl<R: AsyncRead + Send>(
        &self,
        path: &RPath,
        src: R,
        _len: u64,
        create: bool,
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
        let (PathInfo::File(info), _) =
            spawn_blocking(move || me.read_file_impl_for_open_file(cp, file))
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
            .create_new(create)
            .open(&local_path)
            .map_err(map_io_error)
            .await?;

        // Actually rename. This should succeed now that we have created the file.
        spawn_blocking(|| tmp_file_path.persist(local_path))
            .await
            .unwrap()?;

        Ok(info)
    }
}

fn map_io_error(err: std::io::Error) -> anyhow::Error {
    match err.kind() {
        ErrorKind::NotFound => Error::NotFound.into(),
        ErrorKind::PermissionDenied => Error::NotAllowed.into(),
        ErrorKind::NotADirectory => Error::NotADirectory.into(),
        ErrorKind::IsADirectory => Error::IsADirectory.into(),
        ErrorKind::AlreadyExists => Error::Exists.into(),
        _ => anyhow!(err),
    }
}

impl FileRepository for LocalBackend {
    type Reader = Compat<TokioFile>;
    const CONCURRENCY_LIMIT: usize = 16;

    async fn read_file(&self, path: &RPath) -> Result<(FileInfo, Self::Reader)> {
        let me = self.clone();
        let rp = path.to_owned();
        let (PathInfo::File(info), file) = spawn_blocking(move || me.read_file_impl(rp))
            .await
            .unwrap()?
        else {
            bail!(Error::IsADirectory)
        };

        Ok((info, TokioFile::from_std(file).compat()))
    }

    async fn metadata(&self, path: &RPath) -> Result<PathInfo> {
        let this = self.clone();
        let rp = path.to_owned();
        let (info, _file) = spawn_blocking(move || this.read_file_impl(rp))
            .await
            .unwrap()?;

        Ok(info)
    }

    async fn mkdir(&self, path: &RPath) -> Result<()> {
        let local_path = self.config.get_local_path(path);
        tokio::fs::create_dir(local_path)
            .await
            .map_err(map_io_error)?;

        Ok(())
    }

    async fn create_file<R: crate::file_repository::AsyncReader>(
        &self,
        path: &RPath,
        src: R,
        len: u64,
    ) -> Result<FileInfo> {
        self.write_file_impl(path, src, len, true).await
    }

    async fn update_file<R: crate::file_repository::AsyncReader>(
        &self,
        path: &RPath,
        src: R,
        len: u64,
    ) -> Result<FileInfo> {
        self.write_file_impl(path, src, len, false).await
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

    async fn list_files(&self, path: &RPath, up_to_depth: usize) -> Result<Vec<PathInfo>> {
        let mut ret = Vec::new();
        let local_path = &self.config.get_local_path(path);

        let walker = WalkDir::new(local_path).max_depth(up_to_depth).into_iter();

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

            if self.config.is_ignored(&entry_path) {
                continue;
            }

            if entry.file_type().is_dir() {
                ret.push(PathInfo::Dir(entry_path))
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

        Ok(ret)
    }

    async fn set_modified_time(&self, path: &RPath, time: time::OffsetDateTime) -> Result<()> {
        let local_path = self.config.get_local_path(path);

        spawn_blocking(move || {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .read(true)
                .open(&local_path)
                .map_err(map_io_error)?;

            f.set_modified(time.into())?;

            Ok::<_, anyhow::Error>(())
        })
        .await
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use futures::io::Cursor;
    use time::OffsetDateTime;
    use std::{any::Any, fs::OpenOptions, io::Write, path::Path, time::Duration, usize};
    use tempfile::TempDir;

    fn setup(ignore_prefixes: Vec<RPathBuf>) -> (LocalBackend, TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::from_config(Config {
            root_path: tmp.path().to_owned(),
            ignore_prefixes,
        });

        (b, tmp)
    }

    fn setup_read_dir_test() -> (LocalBackend, Vec<&'static str>, Vec<&'static str>, impl Any) {
        let (b, tmp) = setup(vec![["ignored"].into()]);

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
                    PathInfo::Dir(path) => {
                        retrieved_directories.push(path.with_separator('/').unwrap());
                        next_directories.push(path);
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
                PathInfo::Dir(path) => retrieved_dirs.push(path.with_separator('/').unwrap()),
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
    async fn file_ignores_ignored() {
        let (b, _tmp) = setup(vec!["dir/subdir".into()]);

        let path: RPathBuf = RPathBuf::from_sep("dir/subdir/a/b/somefile", '/');
        let res = b.read_file(&path).await;

        let error = res.unwrap_err();
        assert_matches!(error.downcast().unwrap(), Error::NotAllowed);
    }

    #[tokio::test]
    async fn create_file_ignores_ignored_locations() {
        let (b, _tmp) = setup(vec!["dir/we/ignore".into()]);
        let res = b
            .create_file("dir/we/ignore/subdir/test".into(), futures::io::empty(), 0)
            .await
            .unwrap_err();

        assert_matches!(res.downcast().unwrap(), Error::NotAllowed);
    }

    #[tokio::test]
    async fn create_file_fails_when_file_exists() {
        let (b, _tmp) = setup(vec![]);
        b.create_file("file".into(), futures::io::empty(), 0)
            .await
            .unwrap();

        let res = b
            .create_file("file".into(), futures::io::empty(), 0)
            .await
            .unwrap_err();

        assert_matches!(res.downcast().unwrap(), Error::Exists);
    }

    #[tokio::test]
    async fn create_and_read_file() {
        let (b, _tmp) = setup(vec![]);
        let test_bytes = (0..1_000_000u64).map(|x| x as u8).collect::<Vec<_>>();
        let path = "somefile".into();

        b.create_file(path, Cursor::new(&test_bytes), test_bytes.len() as u64)
            .await
            .unwrap();

        let (info, mut body) = b.read_file(path).await.unwrap();

        let mut file_bytes: Vec<u8> = Vec::new();
        futures::io::copy(&mut body, &mut file_bytes).await.unwrap();

        assert_eq!(info.size, test_bytes.len() as u64);
        assert_eq!(file_bytes.len(), test_bytes.len());
        assert_eq!(file_bytes, test_bytes);
    }

    #[tokio::test]
    async fn update_file_ignores_ignored_locations() {
        let (b, _tmp) = setup(vec!["dir/subdir".into()]);

        let res = b
            .update_file(
                &RPathBuf::from(["dir", "subdir", "test"]),
                futures::io::empty(),
                0,
            )
            .await
            .unwrap_err();

        assert_matches!(res.downcast().unwrap(), Error::NotAllowed);
    }

    #[tokio::test]
    async fn metadata_ignores_ignored() {
        let (b, tmp) = setup(vec!["dir/subdir".into()]);

        tokio::fs::create_dir_all(tmp.path().join("dir/subdir"))
            .await
            .unwrap();

        let res = b.metadata("dir/subdir".into()).await.unwrap_err();
        assert!(matches!(res.downcast().unwrap(), Error::NotAllowed));
    }

    #[tokio::test]
    async fn list_files_errors_on_not_exist() {
        let (b, _tmp) = setup(vec![]);
        let res = b
            .list_files("should-not-exist".into(), 0)
            .await
            .unwrap_err();

        assert_matches!(res.downcast().unwrap(), Error::NotFound);
    }

    #[tokio::test]
    async fn write_file_on_directory() {
        let (b, _tmp) = setup(vec![]);
        b.mkdir("testdir".into()).await.unwrap();

        let res = b
            .update_file("testdir".into(), futures::io::empty(), 0)
            .await
            .unwrap_err();

        assert_matches!(res.downcast().unwrap(), Error::IsADirectory);
    }

    #[tokio::test]
    async fn read_file_on_directory() {
        let (b, _tmp) = setup(vec![]);
        b.mkdir("testdir".into()).await.unwrap();

        let res = b.read_file("testdir".into()).await.unwrap_err();

        assert_matches!(res.downcast().unwrap(), Error::IsADirectory);
    }

    #[tokio::test]
    async fn deleting_dir() {
        // Make sure we can delete directories
        let (b, _tmp) = setup(vec![]);

        let path = "testdir".into();
        b.mkdir(path).await.unwrap();
        b.delete(path).await.unwrap();

        assert_matches!(b.maybe_metadata(path).await.unwrap(), None);
    }

    #[tokio::test]
    async fn write_and_set_modified_time() {
        let (b, _tmp) = setup(vec![]);

        let now = OffsetDateTime::now_utc();
        
        b.create_file("somefile".into(), Cursor::new([1, 2, 3]), 3)
            .await
            .unwrap();

        b.set_modified_time("somefile".into(), now - Duration::from_secs(300)).await.unwrap();
        
        let PathInfo::File(info) = b.metadata("somefile".into()).await.unwrap() else {
            panic!()
        };
        
        assert!(info.modified + Duration::from_secs(295) < now);
    }
}
