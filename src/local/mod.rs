use crate::{
    backend::FileRepository,
    paths,
    util::{datetime_to_filetime, u8_slice_to_hex_string},
    DirectoryInfo, Error, FileInfo, PathInfo,
};
use async_trait::async_trait;
use futures::AsyncRead;
use sha1::{Digest, Sha1};
use std::{fs::File, path::PathBuf, sync::Arc};
use time::OffsetDateTime;
use tokio::task::spawn_blocking;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

#[derive(Debug, Clone)]
pub struct LocalBackend {
    inner: Arc<InnerLocalBackend>,
}
#[derive(Debug, Clone)]
struct InnerLocalBackend {
    /// A path on the local file system. Note that although from this type definition
    /// it may look like we support non-valid-unicode paths, this is not the case
    /// and will panic at runtime.
    root_path: PathBuf,

    /// A list of relative_name prefixes. Any file or folder with a relative name that matches
    /// these prefixes will not be emitted by read_dir. It is also not possible to download
    /// or upload to there, which will produce an error.
    /// info() will also throw an error when asked about these files.
    ignore_prefixes: Vec<String>,
}

impl LocalBackend {
    pub fn new(root_path: PathBuf, ignore_prefixes: Vec<String>) -> Self {
        Self {
            inner: Arc::new(InnerLocalBackend {
                root_path,
                ignore_prefixes,
            }),
        }
    }
}

impl InnerLocalBackend {
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

    fn info_block(&self, relative_name: &str) -> crate::Result<PathInfo> {
        if self.is_ignored(relative_name) {
            return Err(Error::NotFound(relative_name.to_string()));
        }

        let relative_name = relative_name.to_owned();

        let local_path = self.get_local_path_from_relative_name(&relative_name);
        let metadata = local_path.metadata()?;

        if metadata.is_file() {
            // Calculate the files SHA1 Hash
            let mut file = File::open(&local_path)?;

            let mut hasher = Sha1::new();
            std::io::copy(&mut file, &mut hasher)?;
            let hash = Some(u8_slice_to_hex_string(&hasher.finalize()));

            let info = FileInfo {
                relative_name,
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .expect("system has modification times")
                    .into(),
                hash,
            };

            Ok(PathInfo::File(info))
        } else {
            Ok(PathInfo::Dir(DirectoryInfo { relative_name }))
        }
    }

    fn read_dir_block(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        let local_path = self.get_local_path_from_relative_name(relative_name);

        let mut ret = vec![];

        let iter = match local_path.read_dir() {
            Ok(x) => x,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(relative_name.into()))
            }
            Err(err) => {
                return Err(Error::IoError(err));
            }
        };

        for f in iter {
            let f = f.map_err(|e| (e, "while listing folder contents of"))?;

            let name_osstr = f.file_name();
            let name = name_osstr
                .to_str()
                .ok_or(Error::PathContainsInvalidUnicode)?;

            // Note that if relative name is a valid relative name, so will this be
            let f_relative_name = paths::append_to_relative_name(relative_name, name);

            // TODO: Possibly make it so that we can just call info on the file and still get a useful
            // answer (we then would only have to check if a file is ignored inside of info)
            if self.is_ignored(&f_relative_name) {
                continue;
            }

            let info = self.info_block(&f_relative_name)?;

            ret.push(info)
        }

        Ok(ret)
    }

    fn delete_file_block(&self, relative_name: &str) -> crate::Result<()> {
        let local_path = self.get_local_path_from_relative_name(relative_name);
        std::fs::remove_file(local_path)?;

        Ok(())
    }

    fn set_modified_time_block(
        &self,
        relative_name: &str,
        modtime: OffsetDateTime,
    ) -> crate::Result<()> {
        let local_path = self.get_local_path_from_relative_name(relative_name);
        filetime::set_file_mtime(local_path, datetime_to_filetime(modtime))?;

        Ok(())
    }

    fn read_dirs_block(&self, dirs_relative_name: Vec<String>) -> crate::Result<Vec<PathInfo>> {
        let mut ret = Vec::new();
        for relative_name in dirs_relative_name {
            ret.extend(self.read_dir_block(relative_name.as_ref())?)
        }
        Ok(ret)
    }
}

#[async_trait]
impl FileRepository for LocalBackend {
    type Reader = Compat<tokio::fs::File>;

    async fn info(&self, relative_name: &str) -> crate::Result<PathInfo> {
        let inner = self.inner.clone();
        let relative_name = relative_name.to_string();
        spawn_blocking(move || inner.info_block(&relative_name))
            .await
            .unwrap()
    }

    async fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
        let inner = self.inner.clone();
        let relative_name = relative_name.to_string();
        spawn_blocking(move || inner.read_dir_block(&relative_name))
            .await
            .unwrap()
    }

    async fn read_file(&self, relative_name: &str) -> crate::Result<(Self::Reader, u64)> {
        if self.inner.is_ignored(relative_name) {
            return Err(Error::NotFound(relative_name.to_owned()));
        }

        let local_path = self.inner.get_local_path_from_relative_name(relative_name);

        let f = tokio::fs::File::open(local_path).await?;
        let len = f.metadata().await?.len();
        Ok((f.compat(), len))
    }

    async fn write_file<R: AsyncRead + Send>(
        &self,
        relative_name: &str,
        src: R,
        _len: u64,
    ) -> crate::Result<()> {
        if self.inner.is_ignored(relative_name) {
            return Err(Error::NotAllowed(relative_name.to_owned()));
        }

        let local_path = self.inner.get_local_path_from_relative_name(relative_name);

        let mut dir = local_path.clone();
        dir.pop();

        // Make sure the directories exist..
        std::fs::create_dir_all(dir)?;

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&local_path)
            .await?;

        let mut src = Box::pin(src.compat());
        tokio::io::copy(&mut src, &mut file).await?;
        file.sync_all().await.unwrap();

        Ok(())
    }

    async fn delete_file(&self, relative_name: &str) -> crate::Result<()> {
        let inner = self.inner.clone();
        let relative_name = relative_name.to_string();
        spawn_blocking(move || inner.delete_file_block(&relative_name))
            .await
            .unwrap()
    }

    async fn set_modified_time(
        &self,
        relative_name: &str,
        modtime: OffsetDateTime,
    ) -> crate::Result<()> {
        let inner = self.inner.clone();
        let relative_name = relative_name.to_string();
        spawn_blocking(move || inner.set_modified_time_block(&relative_name, modtime))
            .await
            .unwrap()
    }

    async fn read_dirs(&self, dirs_relative_name: Vec<String>) -> crate::Result<Vec<PathInfo>> {
        let inner = self.inner.clone();
        spawn_blocking(move || inner.read_dirs_block(dirs_relative_name))
            .await
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use futures::io::Cursor;

    use super::*;
    use std::{fs::OpenOptions, io::Write, path::Path};

    #[tokio::test]
    async fn test_read_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::new(tmp.path().to_owned(), vec!["ignored".into()]);

        assert!(b.read_dir("").await.unwrap().is_empty());
        assert!(matches!(
            b.read_dir("does-not-exist").await,
            Err(Error::NotFound(_))
        ));

        let mut files = vec![
            ("", "file-1.txt"),
            ("", "file-2.txt"),
            ("", "file-3.txt"),
            ("subdir", "this-is-a-filename-that-may-or-may-not-be-a-little-longer-than-the-the-other-filenames-because-that-is-really-something-we-should-try-at-some-point.txt"),
            ("subdir", "another-file-but-shorter.bin"),
            ("anothersubdir", "somefile.exe"),
            ("anothersubdir/d1/d2/d3", "ignored")
        ];
        files.sort();

        let files_should_be = files.clone();

        files.push(("ignored/subdir", "f1"));
        files.push(("ignored", "f2"));

        // Create this directory structure
        for (dir, filename) in files {
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

        // Now read it with read_dir (this should really be done recursively,
        // but the recursive function definition is kind of verbose)
        let mut retrieved_directory_structure = vec![];

        let mut next_directories = vec!["".to_owned()];
        while let Some(relative_name) = next_directories.pop() {
            for info in b.read_dir(&relative_name).await.unwrap() {
                match info {
                    PathInfo::Dir(d) => next_directories.push(d.relative_name),
                    PathInfo::File(info) => {
                        let (dir, name) = info.dir_and_name();
                        retrieved_directory_structure.push((dir.to_owned(), name.to_owned()));
                    }
                }
            }
        }

        retrieved_directory_structure.sort();

        assert_eq!(retrieved_directory_structure.len(), files_should_be.len());

        for i in 0..files_should_be.len() {
            assert!(retrieved_directory_structure[i].0 == files_should_be[i].0);
            assert!(retrieved_directory_structure[i].1 == files_should_be[i].1);
        }
    }

    #[tokio::test]
    async fn test_read_file() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::new(tmp.path().to_owned(), Vec::new());

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

        let (mut reader, len) = b.read_file(relative_name).await.unwrap();
        assert_eq!(test_bytes.len() as u64, len);

        let mut file_bytes = Vec::new();
        futures::io::copy(&mut reader, &mut file_bytes)
            .await
            .unwrap();
        assert_eq!(file_bytes, test_bytes);

        let b = LocalBackend::new(tmp.path().to_owned(), vec!["dir/subdir".into()]);
        let res = b.read_file(relative_name).await;
        assert!(
            matches!(res, Err(Error::NotFound(_))),
            "should be not found but got {:?}",
            res
        );
    }

    #[tokio::test]
    async fn test_write_file() {
        let tmp = tempfile::tempdir().unwrap();
        let b = LocalBackend::new(tmp.path().to_owned(), vec!["dir/we/ignore".into()]);
        let test_bytes = (0..1_000_000u64).map(|x| x as u8).collect::<Vec<_>>();

        // Test if we can write to ignored locations
        let x = b
            .write_file("dir/we/ignore/subdir/test", futures::io::empty(), 0)
            .await;
        assert!(matches!(x, Err(Error::NotAllowed(_))));

        // Now write an actual file
        let relative_name = "dir/that-does-not-exist/somefile";
        let fpath = tmp.path().join(relative_name);

        let modtime_should_be = OffsetDateTime::now_utc();
        b.write_file(
            "dir/that-does-not-exist/somefile",
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
}
