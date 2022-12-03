use std::io::{Cursor, Read};
use std::{fmt::Display, str::FromStr};

use super::extensions::RequestBuilderExt;
use super::extensions::ResponseExt;
use super::extensions::UrlExt;
use http::Request;
use isahc::HttpClient;
use isahc::ReadResponseExt;
use oauth2::AccessToken;
use serde::de;
use serde::Deserialize;
use serde::Serialize;
use time::OffsetDateTime;
use url::Url;

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
    pub parents: Option<[String; 1]>,
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
/// around and is generally extremely short lived.
pub struct DriveApi {
    client: HttpClient,
    token: AccessToken,
}

impl DriveApi {
    // Chunksize of 20 MiB seems like a reasonable choice.
    const CHUNKSIZE: usize = (256 * 1024 * 4) * 20;
    const RETRY_COUNT: usize = 3;

    pub fn new(client: HttpClient, token: AccessToken) -> Self {
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
        let url = Url::parse("https://www.googleapis.com/drive/v3/files").unwrap()
            .with_query_pairs(&[
                Some(("fields", "nextPageToken,files/id,files/name,files/mimeType,files/size,files/sha1Checksum,files/modifiedTime,files/parents")),
                Some(("pageSize", "1000")),
                // Google does not complain about empty q paramter. Besides, it's never empty in practice
                Some(("q", q)),
                next_page_token.map(|x| ("pageToken", x))
            ]);

        let req = Request::get(url.to_string())
            .bearer_auth(self.token.secret())
            .body(())?;

        Ok(self.client.send(req)?.json_error_for_status()?.json()?)
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
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .unwrap()
            .with_query_pairs(&[(
                "fields",
                "id,name,mimeType,size,sha1Checksum,modifiedTime,parents",
            )]);

        let req = Request::get(url.to_string())
            .bearer_auth(self.token.secret())
            .body(())?;

        Ok(self.client.send(req)?.json_error_for_status()?.json()?)
    }

    pub fn files_get_alt_media(
        &self,
        id: &str,
        bytes_range: Option<(u64, u64)>
    ) -> crate::Result<isahc::Body> {
        let range_header = bytes_range.map(|(from, to)| format!("bytes={from}-{to}"));

        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .unwrap()
            .with_query_pairs(&[
                Some(("acknowledgeAbuse", "true")),
                Some(("alt", "media")),
                range_header.as_ref().map(|x| ("Range", x.as_str())),
            ]);

        let req = Request::get(url.to_string())
            .bearer_auth(self.token.secret())
            .body(())?;

        let resp = self.client.send(req)?.json_error_for_status()?;
        Ok(resp.into_body())
    }

    pub fn files_delete(&self, id: &str) -> crate::Result<()> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}")).unwrap();

        let req = Request::delete(url.to_string())
            .bearer_auth(self.token.secret())
            .body(())?;

        self.client.send(req)?.json_error_for_status()?;

        Ok(())
    }

    pub fn files_create(&self, create_request: FilesReq) -> crate::Result<FileId> {
        let url = Url::parse("https://www.googleapis.com/drive/v3/files").unwrap();

        let req = Request::post(url.to_string())
            .bearer_auth(self.token.secret())
            .json_body(&create_request)?;

        Ok(self.client.send(req)?.json_error_for_status()?.json()?)
    }

    pub fn files_create_upload<R: AsRef<[u8]>>(
        &self,
        create_request: FilesReq,
        src: R,
    ) -> crate::Result<FileId> {
        let url =
            Url::parse("https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable")
                .unwrap();

        let req = Request::post(url.to_string())
            .bearer_auth(self.token.secret())
            .json_body(&create_request)?;

        let resp = self.client.send(req)?.json_error_for_status()?;

        let resumable_session_url = resp.headers().get("Location").unwrap().to_str().unwrap();

        self.upload(resumable_session_url, src)
    }

    pub fn files_update_upload<R: AsRef<[u8]>>(&self, id: &str, src: R) -> crate::Result<FileId> {
        let url = Url::parse(&format!(
            "https://www.googleapis.com/upload/drive/v3/files/{id}?uploadType=resumable"
        )).unwrap();

        let req = Request::patch(url.to_string())
            // Note that we could send a non empty object here, but for now,
            // we don't. We use another API request to change metadata.
            .header(http::header::CONTENT_TYPE, "application/json")
            .bearer_auth(self.token.secret())
            .body("{}")?;

        let resp = self.client.send(req)?.json_error_for_status()?;

        let resumable_session_url = resp.headers().get("Location").unwrap().to_str().unwrap();

        self.upload(resumable_session_url, src)
    }

    pub fn files_update(&self, id: &str, update_req: FilesReq) -> crate::Result<()> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}")).unwrap();

        let req = Request::patch(url.to_string())
            .bearer_auth(self.token.secret())
            .json_body(&update_req)?;

        let file_id: FileId = self.client.send(req)?
            .json_error_for_status()?
            .json()?;

        debug_assert_eq!(&file_id.id, id);

        Ok(())
    }

    // Try and read everything from the reader. Try to not but it into memory.
    // At this point, it's already seekable, so we do not have to do that.
    fn upload<R: AsRef<[u8]>>(&self, resumable_session_url: &str, src: R) -> crate::Result<FileId> {
        let src = src.as_ref();
        let len = src.len();
        let mut src = Cursor::new(src.as_ref());

        let mut backing_buffer = vec![0u8; Self::CHUNKSIZE];

        let mut failed = 0;
        let mut cursor = 0;
        let id: FileId = loop {
            if failed > Self::RETRY_COUNT {
                panic!("failed 3 times");
            }

            let read =  src.read(&mut backing_buffer)?;
            let chunk = &backing_buffer[0..read];

            let range_header = &format!(
                "bytes {}-{}/{}",
                cursor,
                cursor + chunk.len() - 1,
                len
            );

            let req = Request::put(resumable_session_url)
                .header(http::header::CONTENT_RANGE, range_header)
                .body(chunk)?;

            let mut resp = self.client.send(req)?
                // FIXME: This should be retried instead of just failing, but leave it like this for now
                .json_error_for_status()?;

            match resp.status().as_u16() {
                // Success. We are done, extract the field and return it
                200..=299 => break resp.json()?,
                
                // Not yet finished
                308 => {
                    if let Some(range) = resp.headers().get(http::header::RANGE) {
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
                x => panic!("unexpected status code {}", x)
            }
        };

        Ok(id)
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
