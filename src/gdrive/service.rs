use drive_api::{ApiError, DriveApi};
use futures::AsyncRead;
use thiserror::Error;
use std::future::Future;
use std::fmt::Write;
use anyhow::anyhow;

#[derive(Debug)]
pub enum DriveErrorKind {
    NotFound,
    ParentNotAFolder,
    Unknown
}

#[derive(Error, Debug)]
#[error("{kind:?}: {src}")]
pub struct DriveError {
    pub kind: DriveErrorKind,
    #[source]
    pub src: anyhow::Error
}

type Result<T> = core::result::Result<T, DriveError>;

/// For testing, I do not want to build out the entirety of the Google Drive API. In particular,
/// the querying and searching for files would be fairly complicated to build and require what is essentially
/// a parser and an interpreter. So we reduce the API surface considerably here.
pub trait DriveService {
    fn read_dirs(
        &self,
        folder_ids: &[String],
    ) -> impl Future<Output = Result<Vec<drive_api::File>>> + Send;

    /// Note that the id "root" is valid
    fn get_file(&self, id: &str) -> impl Future<Output = Result<drive_api::File>> + Send;

    fn get_body(&self, id: &str, rev_id: Option<&str>) -> impl Future<Output = Result<Box<dyn AsyncRead + Send>>> + Send;
}

#[derive(Debug)]
pub struct DriveServiceImpl {
    api_client: DriveApi
}

impl DriveService for DriveServiceImpl {
    async fn get_file(&self, id: &str) -> Result<drive_api::File> {
        todo!()
    }

    async fn read_dirs(
        &self,
        folder_ids: &[String],
    ) -> Result<Vec<drive_api::File>> {
        let mut ret = Vec::new();

        // Build the google drive query string that looks for children of those ids.
        let query = {
            // Query string is of the form
            // trashed = false and ('abc' in parents or 'cdf' in parents)
            let mut q = String::from("trashed = false and (");
            for (i, id) in folder_ids.into_iter().enumerate() {
                if i > 0 {
                    write!(q, " or ").unwrap();
                }
                write!(q, "'{}' in parents", id).unwrap();
            }
            // Don't forget the closing brace
            write!(q, ")").unwrap();

            q
        };

        log::debug!("Requesting files with query {query}");

        let files = self.api_client.file_list_all(&query).await?;

        return Ok(files);

        Ok(ret)
    }

    async fn get_body(&self, id: &str, rev_id: Option<&str>) -> Result<Box<dyn AsyncRead + Send>> {
        todo!()
    }
}

impl From<drive_api::ApiError> for DriveError {
    fn from(value: drive_api::ApiError) -> Self {
        let kind = match &value {
            ApiError::RequestFailed { inner, retries } => {
                if inner.has_reason("parentNotAFolder") {
                    DriveErrorKind::ParentNotAFolder
                } else if inner.has_reason("notFound") {
                    DriveErrorKind::NotFound
                } else {
                    DriveErrorKind::Unknown
                }
            },
            _ => DriveErrorKind::Unknown
        };

        Self {
            kind, src: anyhow!(value)
        }
    }
}
