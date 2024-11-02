use crate::{
    paths::{RPath, RPathBuf},
    FileInfo,
};
use anyhow::{bail, Context};
use const_format::concatcp;
use rusqlite::{named_params, Connection, OpenFlags, OptionalExtension, Row};
use static_assertions::assert_obj_safe;
use std::{path::Path, sync::Mutex};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

impl std::ops::Not for Side {
    type Output = Side;

    fn not(self) -> Self::Output {
        match self {
            Side::Local => Side::Remote,
            Side::Remote => Side::Local,
        }
    }
}

pub trait KeyValueStore: Send + Sync {
    fn get(&self, key: &str) -> anyhow::Result<Option<String>>;

    fn put(&self, key: &str, value: &str) -> anyhow::Result<()>;

    fn must_get(&self, key: &str) -> anyhow::Result<String> {
        match self.get(key)? {
            Some(res) => Ok(res),
            None => Err(anyhow::anyhow!("No value for key {key}")),
        }
    }
}
assert_obj_safe!(KeyValueStore);

pub trait TrackedFilesRepository: Send + Sync {
    fn update(&self, info: FileInfo, side: Side) -> anyhow::Result<()>;

    fn get_all(&self, dir_path: &RPath, side: Side) -> anyhow::Result<Vec<FileInfo>>;

    fn delete(&self, path: &RPath) -> anyhow::Result<()>;
}
assert_obj_safe!(TrackedFilesRepository);

#[derive(Debug, Clone, PartialEq)]
pub struct TrackedFile {
    pub path: RPathBuf,
    pub size: u64,
    pub modified: time::OffsetDateTime,
    pub local_hash: Vec<u8>,
    pub remote_hash: Vec<u8>,
}

impl TrackedFile {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<TrackedFile> {
        Ok(TrackedFile {
            path: row.get("rpath")?,
            size: row.get("size")?,
            modified: row.get("modified")?,
            local_hash: row
                .get::<_, Option<Vec<u8>>>("local_hash")?
                .unwrap_or_default(),
            remote_hash: row
                .get::<_, Option<Vec<u8>>>("remote_hash")?
                .unwrap_or_default(),
        })
    }
}

#[derive(Debug)]
pub struct SqliteDatabase {
    raw: Mutex<RawConn>,
}

// I have thought quite a bit about whether there should be an async version of this API
// (where we would necessarily need to use spawn_blocking or something like that).
// In short: I think it is fine to call these from async contexts, especially in the context
// of a CLI application. According to
// https://tokio.rs/blog/2020-04-preemption#a-note-on-blocking
// tokio tasks should last "tens of milliseconds at most", and I genuinely believe that
// the calls made here should be much shorter than that in general, so we would't end
// up blocking the executor too much.
// For reference: Reading around 4000 entries (get_all_tracked_files) took around 2 ms on
// my system.
impl SqliteDatabase {
    pub fn new(c: Connection) -> Self {
        Self {
            raw: Mutex::new(RawConn::new(c)),
        }
    }

    pub fn new_from_path<P: AsRef<Path>>(
        path: P,
        create_if_not_exists: bool,
    ) -> anyhow::Result<Self> {
        let mut flags = OpenFlags::default();
        if !create_if_not_exists {
            flags.remove(OpenFlags::SQLITE_OPEN_CREATE);
        }
        let raw_conn = RawConn::new(Connection::open_with_flags(&path, flags)?);

        // Does the database there have a schema we recognize?
        let v = raw_conn.get_value("schema_version");
        let (valid_schema, could_be_empty) = match v {
            Ok(Some(x)) if x == "1" => (true, false),
            Ok(Some(_)) => (false, false),
            _ => (false, true),
        };

        if !valid_schema && !create_if_not_exists {
            bail!("Database at {:?} is invalid", path.as_ref());
        } else if !could_be_empty {
            bail!("Could not create database at {:?}. A database already exists there and is not empty.", path.as_ref())
        } else {
            // At this point, both create_if_not_exists and could_be_empty are true.
            // We just write the schema and are done
            raw_conn.write_schema()?;
        }

        Ok(Self::new(raw_conn.into_connection()))
    }

    delegate::delegate! {
        to self.raw.lock().unwrap() {
            pub fn get_value(&self, key: &str) -> rusqlite::Result<Option<String>>;

            pub fn put_value(&self, key: &str, value: &str) -> rusqlite::Result<()>;

            pub fn update_tracked_file(&self, t: TrackedFile, side: Option<Side>) -> rusqlite::Result<()>;

            pub fn delete_tracked_file(&self, path: &RPath) -> rusqlite::Result<()>;

            pub fn get_all_tracked_files<T, F: Fn(TrackedFile) -> T>(
                &self,
                prefix: &RPath,
                op: F,
            ) -> rusqlite::Result<Vec<T>>;

            #[allow(dead_code)]
            pub fn get_tracked_file(&self, path: &RPath) -> rusqlite::Result<Option<TrackedFile>>;
        }
    }
}

#[derive(Debug)]
struct RawConn {
    conn: Connection,
}

impl RawConn {
    fn new(c: Connection) -> Self {
        Self { conn: c }
    }

    fn into_connection(self) -> Connection {
        self.conn
    }

    fn write_schema(&self) -> anyhow::Result<()> {
        const SCHEMA: &str = r"
            BEGIN;
            DROP TABLE IF EXISTS tracked_files;
            CREATE TABLE tracked_files (
                rpath 	        BLOB,
                size    		INTEGER,
                modified		DATETIME,
                local_hash      BLOB,
                remote_hash     BLOB,
                PRIMARY KEY (rpath)
            );
            
            DROP TABLE IF EXISTS config;
            CREATE TABLE config (
                key 		TEXT,
                value		TEXT,
                PRIMARY KEY (key)						
            );
            
            INSERT INTO Config VALUES ('schema_version', '1');
            COMMIT;
        ";

        Ok(self.conn.execute_batch(SCHEMA)?)
    }

    #[allow(dead_code)]
    fn get_tracked_file(&self, path: &RPath) -> rusqlite::Result<Option<TrackedFile>> {
        let res = self.conn.prepare_cached("SELECT rpath, size, modified, local_hash, remote_hash FROM tracked_files WHERE rpath = :rpath")?
            .query_row(named_params! { ":rpath": path }, TrackedFile::from_row)
            .optional()?;

        Ok(res)
    }

    /// If side is none, both remote_hash and local_hash are updated. If side == Some(Side::Local),
    /// If side == Some(Side::Remote), then only the remote hash is updated.
    fn update_tracked_file(&self, t: TrackedFile, side: Option<Side>) -> rusqlite::Result<()> {
        // This is kind of horrible
        match side {
            None => {
                let mut s = self.conn.prepare_cached(
                    "INSERT OR REPLACE INTO tracked_files (rpath, size, modified, local_hash, remote_hash) VALUES (:rpath, :size, :modified, :local_hash, :remote_hash)"
                )?;
                s.execute(named_params! {
                    ":rpath": t.path,
                    ":size": t.size,
                    ":modified": t.modified,
                    ":local_hash": t.local_hash,
                    ":remote_hash": t.remote_hash
                })?;
            }
            Some(Side::Local) => {
                let mut s = self.conn.prepare_cached(
                    "INSERT OR REPLACE INTO tracked_files (rpath, size, modified, local_hash) VALUES (:rpath, :size, :modified, :local_hash)"
                )?;
                s.execute(named_params! {
                    ":rpath": t.path,
                    ":size": t.size,
                    ":modified": t.modified,
                    ":local_hash": t.local_hash
                })?;
            }
            Some(Side::Remote) => {
                let mut s = self.conn.prepare_cached(
                    "INSERT OR REPLACE INTO tracked_files (rpath, size, modified, remote_hash) VALUES (:rpath, :size, :modified, :remote_hash)
                ")?;
                s.execute(named_params! {
                    ":rpath": t.path,
                    ":size": t.size,
                    ":modified": t.modified,
                    ":remote_hash": t.remote_hash
                })?;
            }
        }

        Ok(())
    }

    fn get_value(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT value FROM config WHERE key = :key")?;

        let ret: Option<String> = stmt
            .query_row(named_params! { ":key": key }, |row| row.get("value"))
            .optional()?;

        Ok(ret)
    }

    fn put_value(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("INSERT OR REPLACE INTO config VALUES (:key, :value)")?;

        stmt.execute(named_params! { ":key": key, ":value": value})?;

        Ok(())
    }

    /// This will not return an error if the specified entry does not exist.
    fn delete_tracked_file(&self, path: &RPath) -> rusqlite::Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("DELETE FROM tracked_files WHERE rpath = :rpath")?;

        stmt.execute(named_params! { ":rpath": path })?;

        Ok(())
    }

    /// Returns all tracked files with rpaths that start with the given prefix and an additional
    /// separator (i.e. if prefix == "test" then return all files that have rpath "test/**". Of course,
    /// the real separator is a null byte currently)
    fn get_all_tracked_files<T, F: Fn(TrackedFile) -> T>(
        &self,
        prefix: &RPath,
        op: F,
    ) -> rusqlite::Result<Vec<T>> {
        const QUERY: &str =
            "SELECT rpath, size, modified, local_hash, remote_hash FROM tracked_files";
        const CONSTRAINED_QUERY: &str =
            concatcp!(QUERY, " WHERE rpath >= :lower AND rpath < :upper");
        // Some pain and suffering went into this function. We need sqlite to find all strings that
        // match a certain prefix. One might think that using GLOB is a good idea. It is not however,
        // as we cannot guarantee that the special wildcard symbol '*' does not appear in paths.
        // Then, one might think that SQLITE's LIKE would work. That, however, does case insensitive comparisons
        // by default, and PRAGMA case_sensitive_like is deprecated since 3.44.
        // So from here, that leaves us with a few options
        // 1) Return all rows and filter them in application code (or use a custom sql function to do the filtering
        // "inside" SQLITE, but either way, this results in a full table scan, and hurts my heart)
        // 2) Figure out how to compile sqlite with the flag SQLITE_CASE_SENSITIVE_LIKE. This wouldn't be a bad option.
        // 3) Go with the kind of hacky (but well-defined!) solution we have chosen here.
        if !prefix.is_empty() {
            // Thanks to this person https://stackoverflow.com/a/20423673
            // This solution relies on the fact that, by default, SQLITE just uses lexiocraphic order on the bytes
            // (by way of memcmp), and stores string as UTF-8 (a superset of ascii). This should avoid a full table scan
            // (if there is an index on the path).
            let lower = prefix.serialize();

            // Below we rely on the fact that lower is terminated by the separator char, and that the character
            // immediatly after the sep char is 0.
            let (lower_without_sep, sep) = lower.split_at(lower.len() - 1);
            debug_assert_eq!(sep, "/");

            let params = named_params! {
                ":lower": lower,
                // 0 (0x30) is the character immediatly following / (0xF) in ASCII
                ":upper": format!("{}0", lower_without_sep)
            };

            self.conn
                .prepare_cached(CONSTRAINED_QUERY)?
                .query_map(params, |row| TrackedFile::from_row(row).map(&op))?
                .collect()
        } else {
            self.conn
                .prepare_cached(QUERY)?
                .query_map((), |row| TrackedFile::from_row(row).map(&op))?
                .collect()
        }
    }
}

impl TrackedFilesRepository for SqliteDatabase {
    fn update(&self, info: FileInfo, side: Side) -> anyhow::Result<()> {
        let (local_hash, remote_hash) = match side {
            Side::Local => (info.hash, Vec::new()),
            Side::Remote => (Vec::new(), info.hash),
        };

        let updated = TrackedFile {
            path: info.rpath,
            size: info.size,
            modified: info.modified,
            local_hash,
            remote_hash,
        };

        self.update_tracked_file(updated, Some(side))?;

        Ok(())
    }

    fn get_all(&self, dir_rpath: &RPath, side: Side) -> anyhow::Result<Vec<FileInfo>> {
        let ret = self.get_all_tracked_files(dir_rpath, |tf| FileInfo {
            rpath: tf.path,
            size: tf.size,
            modified: tf.modified,
            hash: match side {
                Side::Local => tf.local_hash,
                Side::Remote => tf.remote_hash,
            },
        })?;
        Ok(ret)
    }

    fn delete(&self, path: &RPath) -> anyhow::Result<()> {
        self.delete_tracked_file(path)?;
        Ok(())
    }
}

impl KeyValueStore for SqliteDatabase {
    fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.get_value(key)
            .with_context(|| format!("Tried to retrieve value for key `{key}`)"))
    }

    fn put(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.put_value(key, value).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{macros::datetime, OffsetDateTime};

    fn empty_in_memory_repository() -> SqliteDatabase {
        let r = SqliteDatabase::new(Connection::open_in_memory().unwrap());
        r.raw.lock().unwrap().write_schema().unwrap();

        r
    }

    #[test]
    fn key_value() {
        let repo = empty_in_memory_repository();

        let v = repo.get_value("SomeKey").unwrap();
        assert!(v.is_none(), "key should not yet exist");

        let _ = repo.put_value("SomeKey", "SomeValue 123456").unwrap();

        let v = repo.get_value("SomeKey").unwrap();
        assert!(v.is_some());
        assert_eq!(v.unwrap(), "SomeValue 123456");
    }

    #[test]
    fn file_tracking() {
        let repo = empty_in_memory_repository();

        let rpath = &RPathBuf::from_components(["somedir", "that", "contains", "file"]);
        let res = repo.get_tracked_file(rpath).unwrap();
        assert!(res.is_none());

        let tf = TrackedFile {
            path: rpath.to_owned(),
            size: 42069,
            modified: datetime!(2019-12-01 0:00 UTC),
            local_hash: "a0acb1671c1f8829404636476b221cd417fe89bf"
                .to_owned()
                .into_bytes(),
            remote_hash: "asdasdkljhasdkjh".to_owned().into_bytes(),
        };

        repo.update_tracked_file(tf.clone(), None).unwrap();

        let tf_retrieved = repo.get_tracked_file(rpath).unwrap();

        assert_eq!(tf_retrieved, Some(tf));

        repo.delete_tracked_file(rpath).unwrap();

        let tf3 = repo.get_tracked_file(rpath).unwrap();
        assert!(tf3.is_none());
    }

    fn update_sided_impl(side: Side) {
        let db = empty_in_memory_repository();
        let example_info = FileInfo {
            rpath: ["somedir", "that", "contains", "file"].into(),
            size: 42,
            modified: datetime!(2019-12-01 0:00 UTC),
            hash: "sadjhasdjhg".to_owned().into_bytes(),
        };

        db.update(example_info.clone(), side).unwrap();

        let retrieved = db
            .get_all(&RPathBuf::from(["somedir", "that", "contains"]), side)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(retrieved, example_info);
    }

    #[test]
    fn update_remote() {
        update_sided_impl(Side::Remote)
    }

    #[test]
    fn update_local() {
        update_sided_impl(Side::Local)
    }

    #[test]
    fn fail_for_non_existing_database() {
        let p = "/tmp/3e3a7d09-2c8d-4382-b8aa-688fdbaffbc4eca4ad30-879c-4b73-9485-d278502538d8/no_such_file";
        let x = SqliteDatabase::new_from_path(p, false);
        assert!(
            x.is_err(),
            "new_from_path should fail early for non existing paths"
        )
    }

    #[test]
    fn get_all_tracked_files_works() {
        let db = empty_in_memory_repository();

        let tf = TrackedFile {
            path: RPathBuf::empty(),
            size: 42,
            modified: OffsetDateTime::UNIX_EPOCH,
            local_hash: vec![],
            remote_hash: vec![],
        };

        let tf1 = TrackedFile {
            path: RPathBuf::from_components(["test_ends_with_/", "file1_with_/"]),
            ..tf.clone()
        };
        let tf2 = TrackedFile {
            path: RPathBuf::from_components(["test_ends_with_/", "file2_with_\\and_/"]),
            ..tf.clone()
        };
        let tf3 = TrackedFile {
            path: RPathBuf::from_components(["test_ends_with//", "a_file"]),
            ..tf.clone()
        };

        db.update_tracked_file(tf1.clone(), None).unwrap();
        db.update_tracked_file(tf2.clone(), None).unwrap();
        db.update_tracked_file(tf3.clone(), None).unwrap();

        let prefix = RPathBuf::from_components(["test_ends_with_/"]);
        let mut tfs = db.get_all_tracked_files(&prefix, |x| x).unwrap();
        tfs.sort_by(|x, y| x.path.cmp(&y.path));

        assert_eq!(tfs, [tf1, tf2]);
    }
}
