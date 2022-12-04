use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use crate::Error;

use lazy_static::lazy_static;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

#[derive(Debug, Clone, PartialEq)]
pub struct TrackedFile {
    pub relative_name: String,
    pub size: u64,
    // Modification time, stored as UNIX timestamp (with millisecond precision)
    pub modified: time::OffsetDateTime,
    pub hash: Option<crate::HashValue>,
}

const TBL_CONFIG: &str = "config";
const COL_KEY: &str = "key";
const COL_VALUE: &str = "value";

const TBL_TRACKED_FILES: &str = "tracked_files";
const COL_RELATIVE_NAME: &str = "relative_name";
const COL_SIZE: &str = "size";
const COL_MODIFIED: &str = "modified";
const COL_HASH: &str = "hash";
const KEY_SCHEMA_VERSION: &str = "schema_version";

lazy_static! {
    static ref SCHEMA: String = format!(
        "BEGIN;
        DROP TABLE IF EXISTS {TBL_TRACKED_FILES};
        
        CREATE TABLE {TBL_TRACKED_FILES} (
            {COL_RELATIVE_NAME} 	TEXT,
            {COL_SIZE}		INTEGER,
            {COL_MODIFIED}		DATETIME,
            {COL_HASH}		TEXT,
            PRIMARY KEY ({COL_RELATIVE_NAME})
        );
        
        DROP TABLE IF EXISTS {TBL_CONFIG};
        CREATE TABLE {TBL_CONFIG} (
            {COL_KEY}		TEXT,
            {COL_VALUE}		TEXT,
            PRIMARY KEY ({COL_KEY})						
        );
        
        INSERT INTO Config VALUES ('{KEY_SCHEMA_VERSION}', '1');
        COMMIT;"
    );
}

#[derive(Clone)]
pub struct DbRepository {
    db: Arc<Mutex<Connection>>,
}

const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assert_clone<T: Clone>() {}

    fn assert_all() {
        assert_send::<DbRepository>();
        assert_sync::<DbRepository>();
        assert_clone::<DbRepository>();
    }
};

impl DbRepository {
    pub fn new(c: Connection) -> Self {
        return Self {
            db: Arc::new(Mutex::new(c)),
        };
    }

    /// Creates a repository for the database named DATABASE_NAME in the given directory path.
    pub fn new_from_path<P: AsRef<Path>>(dir: P, name: &str) -> crate::Result<Self> {
        let path = dir.as_ref().join(name);

        let mut flags = OpenFlags::default();
        flags.remove(OpenFlags::SQLITE_OPEN_CREATE);

        let db = Connection::open_with_flags(path, flags)?;

        let repo = Self::new(db);

        // This is not strictly necessary, but we do it anyways
        let v = repo.get_value(KEY_SCHEMA_VERSION)?;
        match v {
            Some(x) if x == "1" => Ok(repo),
            _ => Err(Error::DatabaseNotInitialized),
        }
    }

    fn write_schema(&self) -> crate::Result<()> {
        Ok(self.db.lock().unwrap().execute_batch(&SCHEMA)?)
    }

    #[allow(dead_code)]
    pub fn get_tracked_file(&self, relative_name: &str) -> crate::Result<Option<TrackedFile>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached(
            &format!("SELECT {COL_RELATIVE_NAME}, {COL_SIZE}, {COL_MODIFIED}, {COL_HASH} FROM {TBL_TRACKED_FILES} WHERE {COL_RELATIVE_NAME} = ?")
        )?;

        let tf: Option<TrackedFile> = stmt
            .query_row((relative_name,), |row| {
                Ok(TrackedFile {
                    relative_name: row.get(COL_RELATIVE_NAME)?,
                    size: row.get(COL_SIZE)?,
                    modified: row.get(COL_MODIFIED)?,
                    hash: row.get(COL_HASH)?,
                })
            })
            .optional()?;

        Ok(tf)
    }

    pub fn get_all_tracked_files(
        &self,
        relative_name_prefix: &str,
    ) -> crate::Result<Vec<TrackedFile>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached(&format!("SELECT {COL_RELATIVE_NAME}, {COL_SIZE}, {COL_MODIFIED}, {COL_HASH} FROM {TBL_TRACKED_FILES} WHERE {COL_RELATIVE_NAME} GLOB ?"))?;

        let pattern = format!("{}*", relative_name_prefix);

        let res = stmt
            .query_map((pattern,), |row| {
                Ok(TrackedFile {
                    relative_name: row.get(COL_RELATIVE_NAME)?,
                    size: row.get(COL_SIZE)?,
                    modified: row.get(COL_MODIFIED)?,
                    hash: row.get(COL_HASH)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(res)
    }

    pub fn update_tracked_file(&self, t: TrackedFile) -> crate::Result<()> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached(
            &format!("INSERT OR REPLACE INTO {TBL_TRACKED_FILES} ({COL_RELATIVE_NAME}, {COL_SIZE}, {COL_MODIFIED}, {COL_HASH}) VALUES (?, ?, ?, ?)")
        )?;

        stmt.execute((t.relative_name, t.size, t.modified, t.hash))?;

        Ok(())
    }

    /// This will not return an error if the specified entry does not exist.
    pub fn delete_tracked_file(&self, relative_name: &str) -> crate::Result<()> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached(&format!(
            "DELETE FROM {TBL_TRACKED_FILES} WHERE {COL_RELATIVE_NAME} = ?"
        ))?;

        stmt.execute((relative_name,))?;

        Ok(())
    }

    pub fn get_value(&self, key: &str) -> crate::Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached(&format!(
            "SELECT {COL_VALUE} FROM {TBL_CONFIG} WHERE {COL_KEY} = ?"
        ))?;

        let ret: Option<String> = stmt
            .query_row((key,), |row| row.get(COL_VALUE))
            .optional()?;

        Ok(ret)
    }

    pub fn put_value(&self, key: &str, value: &str) -> crate::Result<()> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached(&format!(
            "INSERT OR REPLACE INTO {TBL_CONFIG} VALUES (?, ?)"
        ))?;

        stmt.execute((key, value))?;

        Ok(())
    }
}

/// Creates a database at the specified path. Note that path is a directory, the actual
/// filename of the database is hardcoded. The given path needs to exist.
pub fn initialize_database<P: AsRef<Path>>(dir: P, filename: &str) -> crate::Result<DbRepository> {
    let path = dir.as_ref().join(filename);

    let db = Connection::open(&path)?;
    let repo = DbRepository::new(db);

    // Make sure that the database is not yet initialized
    if let Ok(Some(_)) = repo.get_value(KEY_SCHEMA_VERSION) {
        return Err(Error::DatabaseNotEmpty {
            path: path.to_string_lossy().to_string(),
        });
    }

    repo.write_schema()?;

    Ok(repo)
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn empty_in_memory_repository() -> DbRepository {
        let r = DbRepository::new(Connection::open_in_memory().unwrap());
        r.write_schema().unwrap();

        r
    }

    #[test]
    fn test_key_value() {
        let repo = empty_in_memory_repository();

        let v = repo.get_value("SomeKey").unwrap();
        assert!(v.is_none(), "key should not yet exist");

        let _ = repo.put_value("SomeKey", "SomeValue 123456").unwrap();

        let v = repo.get_value("SomeKey").unwrap();
        assert!(v.is_some());
        assert_eq!(v.unwrap(), "SomeValue 123456");
    }

    #[test]
    fn test_file_tracking() {
        let repo = empty_in_memory_repository();

        let rel_name = "somedir/that/contains/file";
        let res = repo.get_tracked_file(rel_name).unwrap();
        assert!(res.is_none());

        let tf = TrackedFile {
            relative_name: rel_name.into(),
            size: 42069,
            modified: datetime!(2019-12-01 0:00 UTC),
            hash: Some("a0acb1671c1f8829404636476b221cd417fe89bf".into()),
        };

        repo.update_tracked_file(tf.clone()).unwrap();

        let tf_retrieved = repo.get_tracked_file(rel_name).unwrap();

        assert_eq!(tf_retrieved, Some(tf));

        repo.delete_tracked_file(rel_name).unwrap();

        let tf3 = repo.get_tracked_file(rel_name).unwrap();
        assert!(tf3.is_none());
    }

    #[test]
    fn test_fail_for_non_existing() {
        let p = "/tmp/3e3a7d09-2c8d-4382-b8aa-688fdbaffbc4eca4ad30-879c-4b73-9485-d278502538d8";
        let x = DbRepository::new_from_path(p, "no such file");
        assert!(
            x.is_err(),
            "new_from_path should not create databases that do not exist"
        )
    }
}
