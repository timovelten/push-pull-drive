use std::{io, path::PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("could not find config (uninitialized)")]
    NoConfigFound,
    #[error("app is already initialized")]
    AlreadyInitialized,
    #[error("a file or directory was not found")]
    NotFound(String),
    #[error("not allowed")]
    NotAllowed(String),
    #[error("encountered a path that does not consist of valid unicode characters")]
    PathContainsInvalidUnicode,
    #[error("path needs to be absolute here")]
    PathNotAbsolute(PathBuf),
    #[error("{path:?} is not a subpath of {base:?}")]
    PathIsNotSubpathOf { base: PathBuf, path: PathBuf },
    #[error("tried to initialize the database, but it was not empty")]
    DatabaseNotEmpty { path: String },
    #[error("database does not contain schema version (it has likely not been initialzed properly) or does not exist at all")]
    DatabaseNotInitialized,
    #[error("a configuration was found, but it is invalid (corrupted?). delete the config folder and re-initialize it.")]
    ConfigInvalid,
    // Transparent errors
    #[error(transparent)]
    DatabaseError(#[from] rusqlite::Error),
    #[error(transparent)]
    IoError(std::io::Error),
    #[error(transparent)]
    HttpError(#[from] ureq::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl<'a> From<(io::Error, &str)> for Error {
    fn from((err, msg): (io::Error, &str)) -> Self {
        let msg = msg.to_owned();

        if err.kind() == io::ErrorKind::NotFound {
            Error::NotFound(msg)
        } else if err.kind() == io::ErrorKind::PermissionDenied {
            Error::NotAllowed(msg)
        } else {
            Error::IoError(err)
        }
    }
}

impl From<io::Error> for Error {
    fn from(x: io::Error) -> Self {
        (x, "").into()
    }
}
