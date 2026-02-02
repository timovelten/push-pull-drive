use crate::paths::RPathBuf;
use anyhow::{anyhow, bail};
use rusqlite::{named_params, Connection, OpenFlags, OptionalExtension};
use std::{path::Path, sync::Mutex};

// TODO: Track extra metadata per backend. In particular, hashes
#[derive(Debug, PartialEq, Eq, Clone, Hash, PartialOrd, Ord)]
pub enum Tracked {
    Dir {
        path: RPathBuf,
    },
    File {
        path: RPathBuf,
        size: u64,
        modified: time::OffsetDateTime,
    },
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
impl SqliteDatabase {
    pub fn new(c: Connection) -> Self {
        Self {
            raw: Mutex::new(RawConn::new(c)),
        }
    }

    /// Same as new_from_path, but uses the default name of the database as a filename
    pub fn new_in_dir(dir: &Path, create_if_not_exists: bool) -> anyhow::Result<Self> {
        let path = dir.join("database.sqlite3");
        Self::new_from_path(path, create_if_not_exists)
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
        match v {
            Ok(Some(x)) if x == "1" => {
                // The database is (probably) initialized with the correct schema
            }
            Ok(Some(_)) => {
                // The database is not empty, but not initialized with the correct schema
                bail!("Could not create database at {:?}. A database already exists there and is not empty.", path.as_ref())
            }
            _ => {
                // The database might not be intiailized at all
                if create_if_not_exists {
                    raw_conn.write_schema()?;
                } else {
                    bail!("Database at {:?} is invalid", path.as_ref());
                }
            }
        };

        Ok(Self::new(raw_conn.into_connection()))
    }

    pub fn put_value<T: serde::Serialize>(&self, key: &str, val: T) -> anyhow::Result<()> {
        let as_str = serde_json::to_string_pretty(&val)?;
        self.raw.lock().unwrap().put_value(key, &as_str)?;
        Ok(())
    }

    pub fn get_value<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> anyhow::Result<Option<T>> {
        let as_str = self.raw.lock().unwrap().get_value(key)?;
        let ret = match as_str {
            None => None,
            Some(x) => Some(serde_json::from_str(&x)?),
        };
        Ok(ret)
    }

    pub fn edit_value<T, S>(
        &self,
        key: &str,
        func: impl FnOnce(T) -> anyhow::Result<S>,
    ) -> anyhow::Result<()>
    where
        T: serde::de::DeserializeOwned,
        S: serde::Serialize,
    {
        let db = self.raw.lock().unwrap();

        let old_val: T = serde_json::from_str(
            &db.get_value(key)?
                .ok_or_else(|| anyhow!("key {key} does not exist"))?,
        )?;
        let new_val = func(old_val)?;

        db.put_value(key, &serde_json::to_string_pretty(&new_val)?)?;
        
        Ok(())
    }

    /// Convenience method
    pub fn must_get_value(&self, key: &str) -> anyhow::Result<String> {
        match self.get_value(key)? {
            Some(res) => Ok(res),
            None => Err(anyhow::anyhow!("No value for key {key}")),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_in_memory_repository() -> SqliteDatabase {
        let r = SqliteDatabase::new(Connection::open_in_memory().unwrap());
        r.raw.lock().unwrap().write_schema().unwrap();

        r
    }

    #[test]
    fn key_value() {
        let repo = empty_in_memory_repository();

        let v = repo.get_value::<String>("SomeKey").unwrap();
        assert!(v.is_none(), "key should not yet exist");

        repo.put_value("SomeKey", "SomeValue 123456").unwrap();

        let v = repo.get_value::<String>("SomeKey").unwrap();
        assert!(v.is_some());
        assert_eq!(v.unwrap(), "SomeValue 123456");
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
