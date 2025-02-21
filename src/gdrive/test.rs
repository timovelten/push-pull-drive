#![cfg(test)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::anyhow;
use base64::{alphabet::URL_SAFE, prelude::BASE64_URL_SAFE, Engine};
use drive_api::MoreFileFields;
use futures::io::{Cursor, AsyncRead as _, AsyncReadExt as _};
use rand::{rng, RngCore};
use time::OffsetDateTime;
use std::pin::pin;

use super::service::{DriveError, DriveErrorKind, DriveService, Result};

struct FakeDriveService {
    state: Arc<Mutex<InnerFakeDriveService>>,
}

struct InnerFakeDriveService {
    files: HashMap<String, drive_api::File>,
    blobs: HashMap<String, Vec<u8>>,
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
    let mut rand = [0; 8];
    rng().fill_bytes(&mut rand);

    // Clear the last two bits, see here https://stackoverflow.com/a/52822722
    // (not that it is really necessary to be accurate here, but whatever)
    rand[8] &= 0xfc;

    BASE64_URL_SAFE.encode(rand)
}

impl DriveService for FakeDriveService {
    async fn mkdir(&self, folder_id: &str, name: &str) -> Result<()> {
        let folder = drive_api::File {
            id: generate_id(),
            parents: Some([folder_id.to_owned()]),
            name: name.to_owned(),
            mime_type: drive_api::FOLDER_MIME_TYPE.to_owned(),
            modified_time: OffsetDateTime::now_utc(),
            more: None,
        };

        self.state
            .lock()
            .unwrap()
            .files
            .insert(folder_id.to_owned(), folder);

        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let removed = state.files.remove(id);
        if removed.is_none() {
            return Err(err(DriveErrorKind::NotFound));
        }
        Ok(())
    }

    async fn read_dirs(&self, folder_ids: &[String]) -> Result<Vec<Vec<drive_api::File>>> {
        todo!()
    }

    async fn get_file(&self, id: &str) -> Result<drive_api::File> {
        let state = self.state.lock().unwrap();
        state
            .files
            .get(id)
            .cloned()
            .ok_or_else(|| err(DriveErrorKind::NotFound))
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
        // Actually, read the body first to allow for paralellism
        let mut src = pin!(src);
        let mut body = Vec::new();
        src.read_to_end(&mut body).await.unwrap();
        let sha1 = sha1(&body);
        let len = body.len();

        let mut state = self.state.lock().unwrap();

        // Insert body into blob storage
        let blob_id = generate_id();
        state.blobs.insert(blob_id.clone(), body);

        let mut file = state.files
            .get_mut(id)
            .ok_or_else(|| err(DriveErrorKind::NotFound))?;

        let Some(MoreFileFields::BinaryFile { head_revision_id, sha1_checksum, size }) = &mut file.more else {
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
        todo!()
    }

    async fn set_modified_time(&self, id: &str, time: time::OffsetDateTime) -> Result<()> {
        todo!()
    }
}
