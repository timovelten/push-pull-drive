use std::{io, path::PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    // Config related
    #[error("could not find config (uninitialized)")]
    NoConfigFound,
    #[error("app is already initialized")]
    AlreadyInitialized,
    #[error("a configuration was found, but it is invalid (corrupted?). delete the config folder and re-initialize it.")]
    ConfigInvalid,

    // File state
    #[error("a file or directory was not found")]
    NotFound(String),
    #[error("not allowed")]
    NotAllowed(String),
    #[error("the object at the specified path is not a file {0}")]
    NotAFile(String),
    #[error("the object at is not a folder {0}")]
    NotAFolder(String),

    // Authentication
    #[error("authentication with google failed")]
    GoogleAuthError(Box<dyn std::error::Error + Send + Sync>),

    // Path related issues
    #[error("encountered a path that does not consist of valid unicode characters")]
    PathContainsInvalidUnicode,
    #[error("path needs to be absolute here")]
    PathNotAbsolute(PathBuf),
    #[error("{path:?} is not a subpath of {base:?}")]
    PathIsNotSubpathOf { base: PathBuf, path: PathBuf },

    // Database related issues
    #[error("tried to initialize the database, but it was not empty")]
    DatabaseNotEmpty { path: String },
    #[error("database does not contain schema version (it has likely not been initialzed properly) or does not exist at all")]
    DatabaseNotInitialized,

    // Transparent errors we just pass through
    #[error(transparent)]
    DatabaseError(#[from] rusqlite::Error),
    #[error(transparent)]
    IoError(std::io::Error),
    #[error(transparent)]
    HttpTransportError(ureq::Error),
    #[error("a http request had an unsuccessful status code")]
    HttpStatusError { status: u16, resp: String },
    #[error(transparent)]
    Json(#[from] serde_json::Error),
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

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Status(s, resp) => Self::HttpStatusError {
                status: s,
                resp: resp
                    .into_string()
                    .unwrap_or_else(|e| format!("failed to read content {:?}", e)),
            },
            ureq::Error::Transport(_) => Self::HttpTransportError(e),
        }
    }
}
