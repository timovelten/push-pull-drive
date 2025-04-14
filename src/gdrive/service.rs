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
    fn mkdir(&self, folder_id: &str, name: &str) -> impl Future<Output = Result<drive_api::File>>;

    fn delete(&self, id: &str, use_trash: bool) -> impl Future<Output = Result<()>> + Send;

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

    async fn delete(&self, id: &str, use_trash: bool) -> Result<()> {
        if use_trash {
            self.api_client.files_trash(id).await?;
        } else {
            self.api_client.files_delete(id).await?;
        }
        
        Ok(())
    }

    async fn mkdir(&self, folder_id: &str, name: &str) -> Result<drive_api::File> {
        let req = FilesReq::new_create_folder(name.to_owned(), folder_id.to_owned());
        let created_file = self.api_client.files_create(req).await?;

        Ok(created_file)
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

    async fn set_modified_time(&self, id: &str, time: time::OffsetDateTime) -> Result<()> {
        self.api_client
            .files_update(
                id,
                FilesReq {
                    modified_time: Some(time),
                    ..Default::default()
                },
            )
            .await?;

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

#[cfg(test)]
pub mod fake {
    use std::{
        collections::HashMap,
        pin::pin,
        sync::{Arc, Mutex},
    };

    use super::*;
    use base64::{prelude::BASE64_URL_SAFE, Engine};
    use drive_api::MoreFileFields;
    use futures::{io::Cursor, AsyncReadExt as _};
    use rand::{rng, RngCore as _};
    use time::OffsetDateTime;

    #[derive(Default, Debug)]
    pub struct FakeDriveService {
        state: Arc<Mutex<InnerFakeDriveService>>,
    }

    #[derive(Debug)]
    struct InnerFakeDriveService {
        files: HashMap<String, drive_api::File>,
        blobs: HashMap<String, Vec<u8>>,
        root_id: String,
    }

    impl Default for InnerFakeDriveService {
        fn default() -> Self {
            let root_id = generate_id();
            let root = drive_api::File {
                id: root_id.clone(),
                parents: None,
                name: "Drive".to_owned(),
                mime_type: drive_api::FOLDER_MIME_TYPE.to_owned(),
                modified_time: OffsetDateTime::now_utc(),
                more: None,
            };

            let mut files = HashMap::new();
            files.insert(root.id.to_owned(), root);

            Self {
                files,
                blobs: Default::default(),
                root_id,
            }
        }
    }

    impl InnerFakeDriveService {
        pub fn get_file_mut(&mut self, id: &str) -> Result<&mut drive_api::File> {
            let maybe_file = if id == "root" {
                self.files.get_mut(&self.root_id)
            } else {
                self.files.get_mut(id)
            };

            maybe_file.ok_or_else(|| err(DriveErrorKind::NotFound))
        }

        pub fn ensure_parent_folder_exists(&mut self, parent_id: &str) -> Result<String> {
            let parent = self.get_file_mut(parent_id)?;

            if !parent.is_folder() {
                return Err(err(DriveErrorKind::ParentNotAFolder));
            }

            Ok(parent.id.to_owned())
        }

        pub fn insert(&mut self, file: &drive_api::File) {
            self.files.insert(file.id.to_owned(), file.clone());
        }
    }

    fn err(kind: DriveErrorKind) -> DriveError {
        // The src is meant primarily as a source of debugging information.
        // Our callers should not really rely on it anyway, so we do not have to fake it here
        DriveError {
            kind: DriveErrorKind::NotFound,
            src: anyhow!("TEST"),
        }
    }

    fn sha1(bytes: &[u8]) -> String {
        // Actually fake this if we ever need to..
        "abc".to_owned()
    }

    fn generate_id() -> String {
        let mut rand = [0; 32];
        rng().fill_bytes(&mut rand);

        BASE64_URL_SAFE.encode(rand)
    }

    async fn read_body<R: AsyncRead + Send>(src: R) -> (Vec<u8>, u64, String) {
        let mut src = pin!(src);
        let mut body = Vec::new();
        src.read_to_end(&mut body).await.unwrap();

        let len = body.len() as u64;
        let sha1 = sha1(&body);
        (body, len, sha1)
    }

    impl DriveService for FakeDriveService {
        async fn mkdir(&self, folder_id: &str, name: &str) -> Result<drive_api::File> {
            let mut state = self.state.lock().unwrap();
            let folder_id = state.ensure_parent_folder_exists(folder_id)?;

            let folder = drive_api::File {
                id: generate_id(),
                parents: Some([folder_id.to_owned()]),
                name: name.to_owned(),
                mime_type: drive_api::FOLDER_MIME_TYPE.to_owned(),
                modified_time: OffsetDateTime::now_utc(),
                more: None,
            };

            state.insert(&folder);

            Ok(folder)
        }

        async fn delete(&self, id: &str, _use_trash: bool) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            let removed = state.files.remove(id);
            if removed.is_none() {
                return Err(err(DriveErrorKind::NotFound));
            }
            Ok(())
        }

        async fn read_dirs(&self, folder_ids: &[String]) -> Result<Vec<Vec<drive_api::File>>> {
            let state = self.state.lock().unwrap();

            let mut ret = Vec::new();
            for id in folder_ids {
                // This is horribly inefficient, but it is just test code
                let files: Vec<_> = state
                    .files
                    .values()
                    .filter(|f| f.parent() == Some(id))
                    .cloned()
                    .collect();

                ret.push(files);
            }

            Ok(ret)
        }

        async fn get_file(&self, id: &str) -> Result<drive_api::File> {
            let mut state = self.state.lock().unwrap();
            state.get_file_mut(id).cloned()
        }

        async fn get_body(
            &self,
            id: &str,
            rev_id: Option<&str>,
        ) -> Result<Box<dyn futures::AsyncRead + Send>> {
            let state = self.state.lock().unwrap();
            let body = state
                .blobs
                .get(id)
                .cloned()
                .ok_or_else(|| err(DriveErrorKind::NotFound))?;

            let cursor = Box::new(Cursor::new(body));
            Ok(cursor)
        }

        async fn update_body<R: futures::AsyncRead + Send>(
            &self,
            id: &str,
            src: R,
        ) -> Result<drive_api::File> {
            let (body, len, sha1) = read_body(src).await;

            let mut state = self.state.lock().unwrap();

            // Insert body into blob storage
            let blob_id = generate_id();
            state.blobs.insert(blob_id.clone(), body);

            let mut file = state.get_file_mut(id)?;

            let Some(MoreFileFields::BinaryFile {
                head_revision_id,
                sha1_checksum,
                size,
            }) = &mut file.more
            else {
                panic!("Has no body")
            };

            *head_revision_id = blob_id;
            *size = len as u64;
            *sha1_checksum = sha1;

            Ok(file.clone())
        }

        async fn create_file<R: futures::AsyncRead + Send>(
            &self,
            folder_id: &str,
            name: &str,
            src: R,
        ) -> Result<drive_api::File> {
            let (body, size, sha1_checksum) = read_body(src).await;

            let mut state = self.state.lock().unwrap();

            state.ensure_parent_folder_exists(&folder_id)?;

            let blob_id = generate_id();
            state.blobs.insert(blob_id.clone(), body);

            let file = drive_api::File {
                id: generate_id(),
                name: name.to_owned(),
                mime_type: "application/octet-stream".to_owned(),
                modified_time: OffsetDateTime::now_utc(),
                parents: Some([folder_id.to_owned()]),
                more: Some(MoreFileFields::BinaryFile {
                    head_revision_id: blob_id,
                    sha1_checksum,
                    size,
                }),
            };

            state.insert(&file);

            Ok(file)
        }

        async fn set_modified_time(&self, id: &str, time: time::OffsetDateTime) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            let file = state.get_file_mut(id)?;

            file.modified_time = time;

            Ok(())
        }
    }
}
