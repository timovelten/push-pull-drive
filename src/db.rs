use crate::{
    filetree::{self, Node},
    paths::{RPath, RPathBuf},
};
use anyhow::bail;
use rusqlite::{named_params, types::Null, Connection, OpenFlags, OptionalExtension, Row};
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

impl filetree::Node for Tracked {
    fn path(&self) -> &RPath {
        match self {
            Self::Dir { path } => path,
            Self::File { path, .. } => path,
        }
    }

    fn is_terminal(&self) -> bool {
        match self {
            Self::Dir { .. } => false,
            Self::File { .. } => true,
        }
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

    delegate::delegate! {
        to self.raw.lock().unwrap() {
            pub fn get_value(&self, key: &str) -> rusqlite::Result<Option<String>>;

            pub fn put_value(&self, key: &str, value: &str) -> rusqlite::Result<()>;

            pub fn update_tracked(&self, tracked: &Tracked) -> rusqlite::Result<()>;

            pub fn delete_tracked(&self, path: &RPath) -> rusqlite::Result<()>;

            pub fn get_all_tracked(
                &self,
                prefix: &RPath
            ) -> rusqlite::Result<Vec<Tracked>>;
        }
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
            DROP TABLE IF EXISTS tracked_files;
            CREATE TABLE tracked (
                path 	        BLOB,
                type            INTEGER,
                size    		INTEGER,
                modified		DATETIME,
                PRIMARY KEY (path)
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
    fn delete_tracked(&self, path: &RPath) -> rusqlite::Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("DELETE FROM tracked WHERE path = ?1")?;

        stmt.execute((path,))?;

        Ok(())
    }

    fn tracked_file_from_row(row: &Row<'_>) -> rusqlite::Result<Tracked> {
        // Keep in sync with update_tracked
        let ty: u64 = row.get("type")?;
        let ret = match ty {
            0 => Tracked::Dir {
                path: row.get("path")?,
            },
            1 => Tracked::File {
                path: row.get("path")?,
                size: row.get("size")?,
                modified: row.get("modified")?,
            },
            t => panic!("Illegal type value {t} should be 0 or 1"),
        };

        Ok(ret)
    }

    fn update_tracked(&self, t: &Tracked) -> rusqlite::Result<()> {
        if t.path().is_empty() {
            assert!(matches!(t, Tracked::Dir {.. }), "The root must be a directory");
            return Ok(());
        }

        // Keep in sync with tracked_file_from_row
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR REPLACE INTO tracked (path, type, size, modified) VALUES (:path, :type, :size, :modified)"
        )?;

        let _rows = match &t {
            Tracked::Dir { path } => stmt.execute(named_params! {
                ":path": path,
                ":type": 0,
                ":size": Null,
                ":modified": Null
            }),
            Tracked::File {
                path,
                size,
                modified,
            } => stmt.execute(named_params! {
                ":path": path,
                ":type": 1,
                ":size": size,
                ":modified": modified
            }),
        }?;

        Ok(())
    }

    /// Returns all tracked files with rpaths that start with the given prefix and an additional
    /// separator (i.e. if prefix == "test" then return all files that have rpath "test/**". Of course,
    /// the real separator is a null byte currently)
    fn get_all_tracked(&self, prefix: &RPath) -> rusqlite::Result<Vec<Tracked>> {
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
            let lower = prefix.serialize();
            let lower_without_trailing_sep = match lower.rsplit_once('/') {
                Some((x, _)) => x,
                None => "",
            };
            let upper = format!("{lower_without_trailing_sep}0");

            let mut stmt = self
                .conn
                .prepare("SELECT * FROM tracked WHERE path >= ?1 AND path < ?2")?;

            let rows = stmt.query((lower, upper))?;
            rows.mapped(Self::tracked_file_from_row)
                .collect()
        } else {
            let mut stmt = self.conn.prepare("SELECT * FROM tracked")?;
            let rows = stmt.query([])?;
            let mut all = rows
                .mapped(Self::tracked_file_from_row)
                .collect::<Result<Vec<_>, _>>()?;

            // Pretend as if the empty directory is also tracked...
            all.push(Tracked::Dir {
                path: RPathBuf::empty(),
            });

            Ok(all)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::assert_equal_when_sorted;

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

        repo.put_value("SomeKey", "SomeValue 123456").unwrap();

        let v = repo.get_value("SomeKey").unwrap();
        assert!(v.is_some());
        assert_eq!(v.unwrap(), "SomeValue 123456");
    }

    #[test]
    fn file_tracking() {
        let repo = empty_in_memory_repository();

        let rpath = &RPathBuf::from_components(["somedir", "that", "contains", "file"]);
        let res = repo.get_all_tracked(rpath).unwrap();
        assert!(res.is_empty());

        let tf = Tracked::File {
            path: rpath.to_owned(),
            size: 42069,
            modified: datetime!(2019-12-01 0:00 UTC),
        };

        repo.update_tracked(&tf).unwrap();

        let tf_retrieved = repo.get_all_tracked(rpath).unwrap();

        assert_eq!(tf_retrieved, [tf]);

        repo.delete_tracked(rpath).unwrap();

        let tf3 = repo.get_all_tracked(rpath).unwrap();
        assert!(tf3.is_empty());
    }

    #[test]
    fn update_() {
        let db = empty_in_memory_repository();
        let example_info = Tracked::File {
            path: ["somedir", "that", "contains", "file"].into(),
            size: 42,
            modified: datetime!(2019-12-01 0:00 UTC),
        };

        db.update_tracked(&example_info).unwrap();

        let retrieved = db
            .get_all_tracked(&RPathBuf::from(["somedir", "that", "contains"]))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(retrieved, example_info);
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

        let tf1 = Tracked::File {
            path: RPathBuf::from_components(["test_ends_with_/", "file1_with_/"]),
            size: 43,
            modified: OffsetDateTime::now_utc(),
        };
        let tf2 = Tracked::File {
            path: RPathBuf::from_components(["test_ends_with_/", "file2_with_\\and_/"]),
            size: 44,
            modified: OffsetDateTime::now_utc(),
        };
        let tf3 = Tracked::File {
            path: RPathBuf::from_components(["test_ends_with//", "a_file"]),
            size: 45,
            modified: OffsetDateTime::now_utc(),
        };

        db.update_tracked(&tf1).unwrap();
        db.update_tracked(&tf2).unwrap();
        db.update_tracked(&tf3).unwrap();

        let prefix = RPathBuf::from_components(["test_ends_with_/"]);

        assert_equal_when_sorted(&db.get_all_tracked(&prefix).unwrap(), &[tf1, tf2]);
    }
}
