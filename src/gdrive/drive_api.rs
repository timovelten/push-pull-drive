use std::{fmt::Display, str::FromStr};

use oauth2::AccessToken;
use reqwest::blocking::Client;
use serde::de;
use serde::Deserialize;
use serde::Serialize;
use time::OffsetDateTime;

/// This module is dedicated to actually making the API calls we need to
/// Google Drive. It does not much beyond that. In particular, it
/// does not handle authentication by itself.

// See https://stackoverflow.com/questions/44301748/how-can-i-deserialize-an-optional-field-with-custom-functions-using-serde
// for why we need the [serde(default)]

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub modified_time: OffsetDateTime,

    #[serde(default)]
    #[serde(deserialize_with = "optional_from_str")]
    pub size: Option<u64>,
    pub sha1_checksum: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileId {
    pub id: String,
}

impl File {
    pub fn is_folder(&self) -> bool {
        self.size.is_none() && self.mime_type == "application/vnd.google-apps.folder"
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FilesListResp {
    pub next_page_token: Option<String>,
    pub files: Vec<File>,
}

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FilesReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[serde(with = "time::serde::rfc3339::option")]
    pub modified_time: Option<OffsetDateTime>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub parents: Option<Vec<String>>,
}

/// This struct just exists to extract common paramters that would
/// need to be passed to each function here in any case. It should not be kept
/// around and is generally extremly short lived.
pub struct DriveApi {
    client: Client,
    token: AccessToken,
}

impl DriveApi {
    // Chunksize of 20 MiB seems like a reasonable choice.
    const CHUNKSIZE: usize = (256 * 1024 * 4) * 20;
    const RETRY_COUNT: usize = 3;

    pub fn new(client: Client, token: AccessToken) -> Self {
        Self { client, token }
    }

    /// Note: you should probably always add the query parameter ... and trashed = false
    pub fn file_list(
        &self,
        q: &str,
        next_page_token: Option<&str>,
    ) -> crate::Result<FilesListResp> {
        // Our response type is hardcoded anyways, and we almost always use the same
        // fields anyway. So we just hardcode it here and move one.
        let ret = self.client.get("https://www.googleapis.com/drive/v3/files")
            .query(&[
                Some(("fields", "nextPageToken,files/id,files/name,files/mimeType,files/size,files/sha1Checksum,files/modifiedTime")),
                Some(("pageSize", "1000")),
                // Google does not complain about empty q paramter. Besides, it's never empty in practice
                Some(("q", q)),
                next_page_token.map(|x| ("pageToken", x))
            ])
            .bearer_auth(self.token.secret())
            .send()?
            .error_for_status()?
            .json()?;

        Ok(ret)
    }

    /// Wrap file_list and keep reading until nextPageToken is None. Return the end result.
    pub fn file_list_all(&self, q: &str) -> crate::Result<Vec<File>> {
        let mut ret = Vec::new();

        let mut next_token = "".to_string();
        loop {
            let resp = self.file_list(q, Some(&next_token))?;
            ret.extend(resp.files);
            if let Some(t) = resp.next_page_token {
                next_token = t
            } else {
                break;
            }
        }

        Ok(ret)
    }

    pub fn files_get(&self, id: &str) -> crate::Result<File> {
        let ret = self
            .client
            .get(format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .query(&[("fields", "id,name,mimeType,size,sha1Checksum,modifiedTime")])
            .bearer_auth(self.token.secret())
            .send()?
            .error_for_status()?
            .json()?;

        Ok(ret)
    }

    pub fn files_get_alt_media(
        &self,
        id: &str,
        bytes_range: Option<(u64, u64)>,
        dst: &mut Vec<u8>,
    ) -> crate::Result<()> {
        let range_header = bytes_range.map(|(from, to)| format!("bytes={from}-{to}"));

        let resp = self.client
            .get(format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .query(&[
                Some(("acknowledgeAbuse", "true")),
                Some(("alt", "media")),
                range_header.as_ref().map(|x| ("Range", x.as_str())),
            ])
            .bearer_auth(self.token.secret())
            .send()?
            .error_for_status()?;

        // FIXME: We need to use async here to actually have a progress bar, bytes_stream() is
        // only available on the async client.

        let body = resp.bytes()?;
        dst.extend_from_slice(&body);

        Ok(())
    }

    pub fn files_delete(&self, id: &str) -> crate::Result<()> {
        self.client
            .delete(format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .bearer_auth(self.token.secret())
            .send()?;

        Ok(())
    }

    pub fn files_create(&self, create_request: FilesReq) -> crate::Result<FileId> {
        let ret = self
            .client
            .post("https://www.googleapis.com/drive/v3/files")
            .json(&create_request)
            .bearer_auth(self.token.secret())
            .send()?
            .error_for_status()?
            .json()?;

        Ok(ret)
    }

    pub fn files_create_upload(
        &self,
        create_request: FilesReq,
        bytes: &[u8],
    ) -> crate::Result<FileId> {
        let resp = self
            .client
            .post("https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable")
            .json(&create_request)
            .bearer_auth(self.token.secret())
            .send()?
            .error_for_status()?;

        let resumable_session_url = resp.headers().get("Location").unwrap().to_str().unwrap();

        self.upload(resumable_session_url, bytes)
    }

    pub fn files_update_upload(&self, id: &str, bytes: &[u8]) -> crate::Result<FileId> {
        let resp = self
            .client
            .patch(format!("https://www.googleapis.com/upload/drive/v3/files/{id}?uploadType=resumable"))
            // Note that we could send a non empty object here, but for now,
            // we don't. We use another API request to change metadata.
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{}")
            .bearer_auth(self.token.secret())
            .send()?
            .error_for_status()?;

        let resumable_session_url = resp.headers().get("Location").unwrap().to_str().unwrap();

        self.upload(resumable_session_url, bytes)
    }

    pub fn files_update(&self, id: &str, update_req: FilesReq) -> crate::Result<()> {
        let file_id: FileId = self
            .client
            .patch(format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .json(&update_req)
            .bearer_auth(self.token.secret())
            .send()?
            .error_for_status()?
            // Consume the response just to be sure
            .json()?;

        debug_assert_eq!(&file_id.id, id);

        Ok(())
    }

    // Returns the ID google returns afterwards
    fn upload(&self, resumable_session_url: &str, bytes: &[u8]) -> crate::Result<FileId> {
        const DELAY: [std::time::Duration; DriveApi::RETRY_COUNT] = [
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        ];

        let mut failed = 0;
        let mut cursor = 0;

        while failed < DELAY.len() {
            std::thread::sleep(DELAY[failed]);

            let chunksize = std::cmp::min(Self::CHUNKSIZE, bytes.len() - cursor);
            let chunk = &bytes[cursor..cursor + chunksize];

            println!("Sent chunk...");

            // Upload CHUNKSIZE bytes to the resumable URL
            let range_header = &format!(
                "bytes {}-{}/{}",
                cursor,
                cursor + chunk.len() - 1,
                bytes.len()
            );

            let resp = self
                .client
                .put(resumable_session_url)
                .header(reqwest::header::CONTENT_RANGE, range_header)
                .send()?;

            let status = resp.status().as_u16();

            match status {
                // Success
                200..=299 => {
                    // Success. We are done, extract the field and return it
                    return Ok(resp.json()?);
                }
                // Not yet finished
                308 => {
                    if let Some(range) = resp.headers().get(reqwest::header::RANGE) {
                        let (from, to) = parse_range_header(range.to_str().unwrap());
                        assert_eq!(from, 0, "range header indicates that remote has not receveived the beginning of a file but has received later parts");

                        cursor = to + 1;
                        failed = 0;
                    } else {
                        // Remote has received no bytes. Loop...
                        // Note that the try counter prevents us from looping indefinitely.
                        failed += 1;
                    }
                }
                // Internal server error
                500..=599 => {
                    failed += 1;
                    continue;
                }
                _ => todo!("everything would be great without errors ._."),
            }
        }

        todo!("make an error")
    }
}

fn parse_range_header(s: &str) -> (usize, usize) {
    let s = s.strip_prefix("bytes=").unwrap();
    let (left, right) = s.split_once('-').unwrap();
    (
        usize::from_str(left).unwrap(),
        usize::from_str(right).unwrap(),
    )
}

// Serde functions we need to serialize and deserialize the data correctly

fn optional_from_str<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: FromStr,
    T::Err: Display,
    D: de::Deserializer<'de>,
{
    let s = <Option<String>>::deserialize(deserializer)?;

    let ret: Option<T> = if let Some(s) = s {
        Some(T::from_str(&s).map_err(de::Error::custom)?)
    } else {
        None
    };

    Ok(ret)
}