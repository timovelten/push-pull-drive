use crate::FileInfo;
use anyhow::bail;
use const_format::concatcp;
use rusqlite::{named_params, Connection, OpenFlags, OptionalExtension, Row};
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
}

pub trait TrackedFilesRepository: Send + Sync {
    fn update(&self, info: FileInfo, side: Side) -> anyhow::Result<()>;

    fn get_all(&self, cpath_dir: &str, side: Side) -> anyhow::Result<Vec<FileInfo>>;

    fn delete(&self, cpath: &str) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackedFile {
    pub cpath: String,
    pub size: u64,
    pub modified: time::OffsetDateTime,
    pub local_hash: Vec<u8>,
    pub remote_hash: Vec<u8>,
}

#[derive(Debug)]
pub struct SqliteDatabase {
    db: Mutex<Connection>,
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
        Self { db: Mutex::new(c) }
    }

    pub fn new_from_path<P: AsRef<Path>>(
        path: P,
        create_if_not_exists: bool,
    ) -> anyhow::Result<Self> {
        let mut flags = OpenFlags::default();
        if !create_if_not_exists {
            flags.remove(OpenFlags::SQLITE_OPEN_CREATE);
        }
        let connection = Connection::open_with_flags(&path, flags)?;
        let ret = Self::new(connection);

        // Does the database there have a schema we recognize?
        let v = ret.get_value("schema_version");
        let (valid_schema, could_be_empty) = match v {
            Ok(Some(x)) if x == "1" => (true, false),
            Ok(Some(_)) => (false, false),
            _ => (false, true),
        };

        if valid_schema {
            Ok(ret)
        } else if !create_if_not_exists {
            bail!("Database at {:?} is invalid", path.as_ref());
        } else if !could_be_empty {
            bail!("Could not create database at {:?}. A database already exists there and is not empty.", path.as_ref())
        } else {
            // At this point, both create_if_not_exists and could_be_empty are true.
            // We just write the schema and are done
            ret.write_schema()?;
            Ok(ret)
        }
    }

    fn write_schema(&self) -> anyhow::Result<()> {
        const SCHEMA: &str = r"
            BEGIN;
            DROP TABLE IF EXISTS tracked_files;
            CREATE TABLE tracked_files (
                cpath 	        TEXT,
                size    		INTEGER,
                modified		DATETIME,
                local_hash      BLOB,
                remote_hash     BLOB,
                PRIMARY KEY (cpath)
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

        Ok(self.db.lock().unwrap().execute_batch(SCHEMA)?)
    }

    fn tracked_file_from_row(row: &Row<'_>) -> rusqlite::Result<TrackedFile> {
        Ok(TrackedFile {
            cpath: row.get("cpath")?,
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

    //
    // Here lie the functions that actually do the SQL. If you change a column name, look here!
    // There is not that many, so it shouldn't be too hard. Also the tests probably save you.
    //

    /// Returns all tracked files with cpaths of the form cpath_dir/* (so all the files in that
    /// directory essentially)
    pub fn get_all_tracked_files<F, T>(&self, cpath_dir: &str, op: F) -> rusqlite::Result<Vec<T>>
    where
        F: Fn(TrackedFile) -> T,
    {
        const QUERY: &str =
            "SELECT cpath, size, modified, local_hash, remote_hash FROM tracked_files";
        const CONSTRAINED_QUERY: &str =
            concatcp!(QUERY, " WHERE cpath >= :lower AND cpath < :upper");
        // Some pain and suffering went into this function. We need sqlite to find all strings that
        // match a certain prefix. One might think that using GLOB is a good idea. It is now however,
        // as we cannot guarantee that the special wildcard symbol '*' does not appear in paths.
        // Then, one might think that SQLITE's LIKE would work. That, however, does case insensitive comparisons
        // by default, and PRAGMA case_sensitive_like is deprecated since 3.44.
        // So from here, that leaves us with a few options
        // 1) Return all rows and filter them in application code (or use a custom sql function to do the filtering
        // "inside" SQLITE, but either way, this results in a full table scan, and hurts my heart)
        // 2) Figure out how to compile sqlite with the flag SQLITE_CASE_SENSITIVE_LIKE. This wouldn't be a bad option.
        // 3) Go with the kind of hacky (but well-defined!) solution we have chosen here.
        let db = self.db.lock().unwrap();

        if !cpath_dir.is_empty() {
            // Thanks to this person https://stackoverflow.com/a/20423673
            // This solution relies on the fact that, by default, SQLITE just uses lexiocraphic order on the bytes
            // (by way of memcmp), and stores string as UTF-8 (a superset of ascii). This should avoid a full table scan
            // (if there is an index on the path)
            assert_eq!('0', char::from_u32('/' as u32 + 1).unwrap()); // This assert is optimized out..
            let params = named_params! {
                ":lower": format!("{cpath_dir}/"),
                ":upper": format!("{cpath_dir}0")
            };

            db.prepare_cached(CONSTRAINED_QUERY)?
                .query_map(params, |row| Self::tracked_file_from_row(row).map(&op))?
                .collect()
        } else {
            db.prepare_cached(QUERY)?
                .query_map((), |row| Self::tracked_file_from_row(row).map(&op))?
                .collect()
        }
    }

    #[allow(dead_code)]
    pub fn get_tracked_file(&self, cpath: &str) -> rusqlite::Result<Option<TrackedFile>> {
        let db = self.db.lock().unwrap();
        let res = db.prepare_cached("SELECT cpath, size, modified, local_hash, remote_hash FROM tracked_files WHERE cpath = :cpath")?
            .query_row(named_params! { ":cpath": cpath }, Self::tracked_file_from_row)
            .optional()?;

        Ok(res)
    }

    /// If side is none, both remote_hash and local_hash are updated. If side == Some(Side::Local),
    /// If side == Some(Side::Remote), then only the remote hash is updated.
    pub fn update_tracked_file(&self, t: TrackedFile, side: Option<Side>) -> rusqlite::Result<()> {
        let db = self.db.lock().unwrap();

        // This is kind of horrible
        match side {
            None => {
                let mut s = db.prepare_cached(
                    "INSERT OR REPLACE INTO tracked_files (cpath, size, modified, local_hash, remote_hash) VALUES (:cpath, :size, :modified, :local_hash, :remote_hash)"
                )?;
                s.execute(named_params! {
                    ":cpath": t.cpath,
                    ":size": t.size,
                    ":modified": t.modified,
                    ":local_hash": t.local_hash,
                    ":remote_hash": t.remote_hash
                })?;
            }
            Some(Side::Local) => {
                let mut s = db.prepare_cached(
                    "INSERT OR REPLACE INTO tracked_files (cpath, size, modified, local_hash) VALUES (:cpath, :size, :modified, :local_hash)"
                )?;
                s.execute(named_params! {
                    ":cpath": t.cpath,
                    ":size": t.size,
                    ":modified": t.modified,
                    ":local_hash": t.local_hash
                })?;
            }
            Some(Side::Remote) => {
                let mut s = db.prepare_cached(
                    "INSERT OR REPLACE INTO tracked_files (cpath, size, modified, remote_hash) VALUES (:cpath, :size, :modified, :remote_hash)
                ")?;
                s.execute(named_params! {
                    ":cpath": t.cpath,
                    ":size": t.size,
                    ":modified": t.modified,
                    ":remote_hash": t.remote_hash
                })?;
            }
        }

        Ok(())
    }

    /// This will not return an error if the specified entry does not exist.
    pub fn delete_tracked_file(&self, cpath: &str) -> rusqlite::Result<()> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached("DELETE FROM tracked_files WHERE cpath = :cpath")?;

        stmt.execute(named_params! { ":cpath": cpath })?;

        Ok(())
    }

    pub fn get_value(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached("SELECT value FROM config WHERE key = :key")?;

        let ret: Option<String> = stmt
            .query_row(named_params! { ":key": key }, |row| row.get("value"))
            .optional()?;

        Ok(ret)
    }

    pub fn put_value(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached("INSERT OR REPLACE INTO config VALUES (:key, :value)")?;

        stmt.execute(named_params! { ":key": key, ":value": value})?;

        Ok(())
    }
}

impl TrackedFilesRepository for SqliteDatabase {
    fn update(&self, info: FileInfo, side: Side) -> anyhow::Result<()> {
        let (local_hash, remote_hash) = match side {
            Side::Local => (info.hash, Vec::new()),
            Side::Remote => (Vec::new(), info.hash),
        };

        let updated = TrackedFile {
            cpath: info.cpath,
            size: info.size,
            modified: info.modified,
            local_hash,
            remote_hash,
        };

        self.update_tracked_file(updated, Some(side))?;

        Ok(())
    }

    fn get_all(&self, cpath_dir: &str, side: Side) -> anyhow::Result<Vec<FileInfo>> {
        let ret = self.get_all_tracked_files(cpath_dir, |tf| FileInfo {
            cpath: tf.cpath,
            size: tf.size,
            modified: tf.modified,
            hash: match side {
                Side::Local => tf.local_hash,
                Side::Remote => tf.remote_hash,
            },
        })?;
        Ok(ret)
    }

    fn delete(&self, cpath: &str) -> anyhow::Result<()> {
        self.delete_tracked_file(cpath)?;
        Ok(())
    }
}

impl KeyValueStore for SqliteDatabase {
    fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.get_value(key).map_err(Into::into)
    }

    fn put(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.put_value(key, value).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn empty_in_memory_repository() -> SqliteDatabase {
        let r = SqliteDatabase::new(Connection::open_in_memory().unwrap());
        r.write_schema().unwrap();

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

        let cpath = "somedir/that/contains/file";
        let res = repo.get_tracked_file(cpath).unwrap();
        assert!(res.is_none());

        let tf = TrackedFile {
            cpath: cpath.into(),
            size: 42069,
            modified: datetime!(2019-12-01 0:00 UTC),
            local_hash: "a0acb1671c1f8829404636476b221cd417fe89bf"
                .to_owned()
                .into_bytes(),
            remote_hash: "asdasdkljhasdkjh".to_owned().into_bytes(),
        };

        repo.update_tracked_file(tf.clone(), None).unwrap();

        let tf_retrieved = repo.get_tracked_file(cpath).unwrap();

        assert_eq!(tf_retrieved, Some(tf));

        repo.delete_tracked_file(cpath).unwrap();

        let tf3 = repo.get_tracked_file(cpath).unwrap();
        assert!(tf3.is_none());
    }

    fn update_sided_impl(side: Side) {
        let db = empty_in_memory_repository();
        let example_info = FileInfo {
            cpath: "somedir/that/contains/file".to_owned(),
            size: 42,
            modified: datetime!(2019-12-01 0:00 UTC),
            hash: "sadjhasdjhg".to_owned().into_bytes(),
        };

        db.update(example_info.clone(), side).unwrap();

        let retrieved = db
            .get_all("somedir/that/contains", side)
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
}
