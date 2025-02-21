use anyhow::anyhow;
use drive_api::{ApiError, DriveApi, FilesReq, GoogleCredential};
use futures::AsyncRead;
use std::fmt::Write;
use std::future::Future;
use thiserror::Error;

use crate::paths::RPath;

#[derive(Debug)]
pub enum DriveErrorKind {
    NotFound,
    ParentNotAFolder,
    Unknown,
}

#[derive(Error, Debug)]
#[error("{kind:?}: {src}")]
pub struct DriveError {
    pub kind: DriveErrorKind,
    #[source]
    pub src: anyhow::Error,
}

pub type Result<T> = core::result::Result<T, DriveError>;

/// For testing, I do not want to build out the entirety of the Google Drive API. In particular,
/// the querying and searching for files would be fairly complicated to build and require what is essentially
/// a parser and an interpreter. So we reduce the API surface considerably here.
pub trait DriveService: Send + Sync + 'static {
    async fn mkdir(&self, folder_id: &str, name: &str) -> Result<()>;

    fn delete(&self, id: &str) -> impl Future<Output = Result<()>> + Send;

    fn read_dirs(
        &self,
        folder_ids: &[String],
    ) -> impl Future<Output = Result<Vec<Vec<drive_api::File>>>> + Send;

    fn read_dir(
        &self,
        folder_id: &str,
    ) -> impl Future<Output = Result<Vec<drive_api::File>>> + Send {
        let folder_id = folder_id.to_owned();
        async move {
            let contents = self.read_dirs(&[folder_id]).await?;
            Ok(contents.into_iter().next().unwrap())
        }
    }

    /// Note that the id "root" is valid
    fn get_file(&self, id: &str) -> impl Future<Output = Result<drive_api::File>> + Send;

    fn get_body(
        &self,
        id: &str,
        rev_id: Option<&str>,
    ) -> impl Future<Output = Result<Box<dyn AsyncRead + Send>>> + Send;

    fn update_body<R: AsyncRead + Send>(
        &self,
        id: &str,
        src: R,
    ) -> impl Future<Output = Result<drive_api::File>> + Send;

    fn create_file<R: AsyncRead + Send>(
        &self,
        folder_id: &str,
        name: &str,
        src: R,
    ) -> impl Future<Output = Result<drive_api::File>> + Send;

    fn set_modified_time(
        &self,
        id: &str,
        time: time::OffsetDateTime,
    ) -> impl Future<Output = Result<()>> + Send;
}

#[derive(Debug)]
pub struct DriveServiceImpl {
    api_client: DriveApi,
}

impl DriveServiceImpl {
    pub fn new(cred: GoogleCredential) -> Self {
        let api_client = DriveApi::new(Default::default(), cred);

        Self { api_client }
    }
}

impl DriveService for DriveServiceImpl {
    async fn get_file(&self, id: &str) -> Result<drive_api::File> {
        Ok(self.api_client.files_get(id).await?)
    }

    /// Note that folder_ids should generally be short (i.e. contain 12 items or less)
    async fn read_dirs(&self, folder_ids: &[String]) -> Result<Vec<Vec<drive_api::File>>> {
        let mut ret = Vec::new();
        for _ in 0..folder_ids.len() {
            ret.push(Vec::new());
        }

        // Build the google drive query string that looks for children of those ids.
        let query = {
            // Query string is of the form
            // trashed = false and ('abc' in parents or 'cdf' in parents)
            let mut q = String::from("trashed = false and (");
            for (i, id) in folder_ids.iter().enumerate() {
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
        for file in files {
            let parent_id = &file
                .parents
                .as_ref()
                .expect("Google drive files we request have a parent")[0];

            let parent_index = folder_ids
                .iter()
                .position(|s| s == parent_id)
                .expect("files we receive are children of the folders we request");

            ret[parent_index].push(file);
        }

        Ok(ret)
    }

    async fn get_body(&self, id: &str, rev_id: Option<&str>) -> Result<Box<dyn AsyncRead + Send>> {
        Ok(self.api_client.files_get_alt_media(id, rev_id).await?)
    }

    async fn update_body<R: AsyncRead + Send>(&self, id: &str, src: R) -> Result<drive_api::File> {
        let info = self.api_client.files_update_upload(id, src).await?;
        Ok(info)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.api_client.files_delete(id).await?;
        Ok(())
    }

    async fn mkdir(&self, folder_id: &str, name: &str) -> Result<()> {
        let req = FilesReq::new_create_folder(name.to_owned(), folder_id.to_owned());
        let _created_file = self.api_client.files_create(req).await?;

        Ok(())
    }

    async fn create_file<R: AsyncRead + Send>(
        &self,
        folder_id: &str,
        filename: &str,
        src: R,
    ) -> Result<drive_api::File> {
        let file = self
            .api_client
            .files_create_upload(
                FilesReq {
                    name: Some(filename.to_string()),
                    parents: Some(vec![folder_id.to_owned()]),
                    ..Default::default()
                },
                src,
            )
            .await?;

        Ok(file)
    }

    async fn set_modified_time(
        &self,
        id: &str,
        time: time::OffsetDateTime,
    ) -> Result<()> {
        self.api_client
            .files_update(
                id,
                FilesReq {
                    modified_time: Some(time),
                    ..Default::default()
                },
            )
            .await;

        Ok(())

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
            }
            _ => DriveErrorKind::Unknown,
        };

        Self {
            kind,
            src: anyhow!(value),
        }
    }
}
