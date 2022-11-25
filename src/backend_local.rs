use crate::{
    backend::FileRepository,
    paths,
    util::{datetime_to_filetime, u8_slice_to_hex_string},
    DirectoryInfo, Error, FileInfo, PathInfo,
};
use anyhow::Context;
use sha1::{Digest, Sha1};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
};
use time::OffsetDateTime;

#[derive(Debug)]
pub struct LocalBackend {
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
    pub const THIS_BACKEND: &str = "local";
    pub const KEY_LOCAL_PATH: &str = "local_path";

    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path: root_path,
            ignore_prefixes: Vec::new(),
        }
    }

    pub fn ignore(&mut self, prefix: &str) {
        self.ignore_prefixes.push(prefix.into());
    }

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
impl FileRepository for LocalBackend {
    fn info(&self, relative_name: &str) -> crate::Result<PathInfo> {
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
                    .expect("system does not have modification times")
                    .into(),
                hash,
            };

            Ok(PathInfo::File(info))
        } else {
            Ok(PathInfo::Dir(DirectoryInfo { relative_name }))
        }
    }

    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<PathInfo>> {
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

            let info = self
                .info(&f_relative_name)
                .with_context(|| "failed to get info")?;

            ret.push(info)
        }

        Ok(ret)
    }

    fn read_file(
        &self,
        relative_name: &str,
        dst: &mut Vec<u8>,
    ) -> crate::Result<FileInfo> {
        if self.is_ignored(relative_name) {
            return Err(Error::NotFound(relative_name.to_owned()));
        }

        let local_path = self.get_local_path_from_relative_name(relative_name);
        let mut file = File::open(local_path)?;

        std::io::copy(&mut file, dst)?;

        let metadata = file.metadata()?;

        Ok(FileInfo {
            relative_name: relative_name.to_owned(),
            size: metadata.len(),
            modified: metadata.modified()?.into(),
            hash: None,
        })
    }

    fn write_file(&self, relative_name: &str, mut src: &[u8]) -> crate::Result<()> {
        if self.is_ignored(relative_name) {
            return Err(Error::NotAllowed(relative_name.to_owned()));
        }

        let local_path = self.get_local_path_from_relative_name(relative_name);

        let mut dir = local_path.clone();
        dir.pop();

        // Make sure the directories exist..
        std::fs::create_dir_all(dir)?;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&local_path)
            .with_context(|| format!("could not open file {:?}", local_path))?;

        std::io::copy(&mut src, &mut file)?;
        drop(file);

        Ok(())
    }

    fn delete_file(&self, relative_name: &str) -> crate::Result<()> {
        let local_path = self.get_local_path_from_relative_name(relative_name);
        std::fs::remove_file(local_path)?;

        Ok(())
    }

    fn set_modified_time(&self, relative_name: &str, modtime: OffsetDateTime) -> crate::Result<()> {
        let local_path = self.get_local_path_from_relative_name(relative_name);
        filetime::set_file_mtime(local_path, datetime_to_filetime(modtime))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, path::Path};

    /// Create a LocalBackend in some temporary location. Note that the temporary directory will
    /// be deleted once the TempDir object is dropped.
    fn create_local_backend_temp_location() -> (LocalBackend, tempfile::TempDir) {
        let tempdir = tempfile::tempdir().expect("failed to create temp dir");
        (LocalBackend::new(tempdir.path().to_owned()), tempdir)
    }

    #[test]
    fn test_read_dir() {
        let (mut b, tmp) = create_local_backend_temp_location();
        b.ignore("ignored");

        assert!(b.read_dir("").unwrap().is_empty());
        assert!(matches!(
            b.read_dir("does-not-exist"),
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
            println!("{:?}", path);

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
            for info in b.read_dir(&relative_name).unwrap() {
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

    #[test]
    fn test_read_file() {
        let (mut b, tmp) = create_local_backend_temp_location();

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

        let mut file_bytes: Vec<u8> = Vec::new();
        b.read_file(relative_name, &mut file_bytes).unwrap();

        assert_eq!(file_bytes, test_bytes);

        b.ignore("dir/subdir");
        let res = b.read_file(relative_name, &mut file_bytes);
        assert!(
            matches!(res, Err(Error::NotFound(_))),
            "should be not found but got {:?}",
            res
        );
    }

    #[test]
    fn test_write_file() {
        let (mut b, tmp) = create_local_backend_temp_location();
        let test_bytes = (0..1_000_000u64).map(|x| x as u8).collect::<Vec<_>>();

        // Test if we can write to ignored locations
        b.ignore("dir/we/ignore");
        let x = b.write_file( "dir/we/ignore/subdir/test", &vec![]);
        assert!(matches!(x, Err(Error::NotAllowed(_))));

        // Now write an actual file
        let relative_name = "dir/that-does-not-exist/somefile";
        let fpath = tmp.path().join(relative_name);

        let modtime_should_be = OffsetDateTime::now_utc();
        b.write_file(
            "dir/that-does-not-exist/somefile",
            &mut (test_bytes.as_slice()),
        )
        .unwrap();

        b.set_modified_time("dir/that-does-not-exist/somefile", modtime_should_be)
            .unwrap();

        let mut f = File::open(fpath).unwrap();

        let mut file_bytes = Vec::new();
        std::io::copy(&mut f, &mut file_bytes).unwrap();

        assert_eq!(file_bytes, test_bytes);

        let modtime = OffsetDateTime::from(f.metadata().unwrap().modified().unwrap());
        assert_eq!(modtime, modtime_should_be);
    }
}
