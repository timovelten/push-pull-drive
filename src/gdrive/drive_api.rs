use std::{fmt::Display, str::FromStr};

use serde::de;
use serde::Deserialize;
use serde::Serialize;
use time::{OffsetDateTime};
use ureq::{Agent, Response};

use super::GoogleCredential;

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
    pub parents: Option<Vec<String>>
}

/// This struct just exists to extract common paramters that would
/// need to be passed to each function here in any case. It should not be kept
/// around and is generally extremly short lived.
pub struct DriveApi<'a> {
    agent: Agent,
    cred: &'a GoogleCredential
}

impl<'a> DriveApi<'a> {
    // Chunksize of 20 MiB seems like a reasonable choice.
    const CHUNKSIZE: usize = (256 * 1024 * 4) * 20;
    const RETRY_COUNT: usize = 3;

    pub fn new(agent: Agent, cred: &'a GoogleCredential) -> Self {
        Self {
            agent,
            cred
        }
    }

    /// Note: you should probably always add the query parameter ... and trashed = false
    pub fn file_list(&self, q: &str, next_page_token: &str) -> crate::Result<FilesListResp> {
        // Our response type is hardcoded anyways, and we almost always use the same
        // fields anyway. So we just hardcode it here and move one.
        let mut req = self
            .agent
            .get("https://www.googleapis.com/drive/v3/files")
            .query(
                "fields",
                "nextPageToken,files/id,files/name,files/mimeType,files/size,files/sha1Checksum,files/modifiedTime",
            )
            // Our network connection should be stable. Why not use the max page size?
            .query("pageSize", "1000");

        if q.len() > 0 {
            req = req.query("q", q);
        }

        if next_page_token.len() > 0 {
            req = req.query("pageToken", next_page_token);
        }

        Ok(self.cred.authenticate(req)?.call()?.into_json()?)
    }

    /// Wrap file_list and keep reading until nextPageToken is None. Return the end result.
    pub fn file_list_all(&self, q: &str) -> crate::Result<Vec<File>> {
        let mut ret = Vec::new();

        let mut next_token = "".to_string();
        loop {
            let resp = self.file_list(q, &next_token)?;
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
        let req = self
            .agent
            .get(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .query("fields", "id,name,mimeType,size,sha1Checksum,modifiedTime");

        let resp = self.cred.authenticate(req)?.call()?;
        Ok(resp.into_json()?)
    }

    pub fn files_get_alt_media(
        &self,
        id: &str,
        bytes_range: Option<(u64, u64)>,
        dst: &mut Vec<u8>,
    ) -> crate::Result<()> {
        let mut req = self
            .agent
            .get(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .query("acknowledgeAbuse", "true")
            .query("alt", "media");

        if let Some((from, to)) = bytes_range {
            req = req.set("Range", &format!("bytes={from}-{to}"));
        }

        let resp = self.cred.authenticate(req)?.call()?;

        resp.into_reader().read_to_end(dst)?;

        Ok(())
    }

    pub fn files_delete(&self, id: &str) -> crate::Result<()> {
        let req = self
            .agent
            .delete(&format!("https://www.googleapis.com/drive/v3/files/{id}"));

        self.cred.authenticate(req)?.call()?;

        Ok(())
    }

    pub fn files_create(&self, create_request: FilesReq) -> crate::Result<String> {
        let req = self
            .agent
            .post("https://www.googleapis.com/drive/v3/files");
        let resp = self.cred.authenticate(req)?.send_json(create_request)?;
        
        Ok(extract_id(resp))
    }

    pub fn files_create_upload(
        &self,
        create_request: FilesReq,
        bytes: &[u8],
    ) -> crate::Result<String> {
        let req = self
            .agent
            .post("https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable");
        let resp = self.cred.authenticate(req)?.send_json(create_request)?;
        let resumable_session_url = resp.header("Location").unwrap();

        self.upload(resumable_session_url, bytes)
    }

    pub fn files_update_upload(&self, id: &str, bytes: &[u8]) -> crate::Result<String> {
        let req = self
            .agent
            .patch(&format!("https://www.googleapis.com/upload/drive/v3/files/{id}?uploadType=resumable"));
        let resp = self.cred.authenticate(req)?
            // Note that we could send a non empty object here, but for now,
            // we don't. We use another API request to change metadata.
            .set("Content-Type", "application/json")
            .send_string("{}")?;

        let resumable_session_url = resp.header("Location").unwrap();

        let returned_id = self.upload(resumable_session_url, bytes)?;
        debug_assert_eq!(returned_id, id);

        Ok(returned_id)
    }

    pub fn files_update(&self, id: &str, update_req: FilesReq) -> crate::Result<()> {
        let req = self
            .agent
            .patch(&format!("https://www.googleapis.com/drive/v3/files/{id}"));

        let resp = self.cred.authenticate(req)?
            .send_json(update_req)?;

        // Make sure to consume the response in one way or another.
        let returned_id = extract_id(resp);
        debug_assert_eq!(returned_id, id);

        Ok(())
    }

    // Returns the ID google returns afterwards
    fn upload(&self, resumable_session_url: &str, bytes: &[u8]) -> crate::Result<String> {
        const DELAY: [std::time::Duration; DriveApi::RETRY_COUNT] = [
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10)
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

            let req = self.agent.put(resumable_session_url).set("Content-Range", &range_header);

            // Note that ureq sets the content length for us (as is required by the google api)
            match self.cred.authenticate(req)?.send_bytes(chunk) {
                Ok(resp) if resp.status() == 308 => {
                    // Find the range header of the response
                    if let Some(range) = resp.header("Range") {
                        let (from, to) = parse_range_header(range);
                        assert_eq!(from, 0, "range header indicates that remote has not receveived the beginning of a file but has received later parts");

                        cursor = to + 1;
                        failed = 0;
                    } else {
                        // Remote has received no bytes. Loop...
                        // Note that the try counter prevents us from looping indefinitely.
                        failed += 1;
                    }
                }
                Ok(resp) => {
                    // We are done! Extract the file ID.
                    return Ok(extract_id(resp));
                }
                Err(ureq::Error::Status(s, _)) if 500 <= s && s < 600 => {
                    // Retry
                    failed += 1;
                    continue;
                }
                Err(_) => {
                    panic!("everything would be great without errors ._.");
                }
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

fn extract_id(resp: Response) -> String {
    resp.into_json::<serde_json::Value>()
        .expect("google returned invalid json")
        .get("id")
        .expect("no id in file response")
        .as_str()
        .expect("id is not a string")
        .to_string()
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