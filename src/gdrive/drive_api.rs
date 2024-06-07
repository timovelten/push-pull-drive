use std::cell::OnceCell;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::pin::pin;
use std::time::Duration;
use std::{fmt::Display, str::FromStr};

use crate::util::clone_request;
use crate::{hex_must_decode, FileInfo};

use super::extensions::RequestBuilderExt;
use super::extensions::UrlExt;
use super::{Error, GoogleCredential};
use anyhow::{anyhow, Context};
use futures::{AsyncRead, AsyncReadExt, TryStreamExt};
use http::{Request, StatusCode};
use oauth2::AccessToken;
use once_cell::sync::Lazy;
use rand::Rng;
use serde::de::DeserializeOwned;
use serde::{de, Deserialize};
use serde::{Deserializer, Serialize};
use time::OffsetDateTime;
use url::Url;

/// This module is dedicated to actually making the API calls we need to
/// Google Drive. It does not much beyond that. In particular, it
/// does not handle authentication by itself.

// See https://stackoverflow.com/questions/44301748/how-can-i-deserialize-an-optional-field-with-custom-functions-using-serde
// for why we need the [serde(default)]

const EMPTY_BODY: &[u8] = &[];
const REQUEST_IS_VALID: &str = "request is valid";

pub const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

#[derive(Debug, Error)]
pub(super) enum ApiError {
    #[error(transparent)]
    HttpError(reqwest::Error),
    #[error(transparent)]
    IoError(futures::io::Error),
    #[error("HTTP request had non 2xx status code (retries: {retries}): {inner:?}")]
    RequestFailed { inner: GoogleError, retries: u32 },
    #[error(transparent)]
    Other(anyhow::Error),
}

#[derive(Deserialize, Debug)]
pub(super) struct GoogleErrorDetails {
    domain: String,
    reason: String,
    message: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(super) enum GoogleError {
    Error {
        code: u16,
        message: String,
        errors: Vec<GoogleErrorDetails>,
    },
}

impl GoogleError {
    fn code(&self) -> u16 {
        let Self::Error { code, .. } = self;
        *code
    }

    fn message(&self) -> &str {
        let Self::Error { message, .. } = self;
        message
    }

    fn errors(&self) -> &[GoogleErrorDetails] {
        let Self::Error { errors, .. } = self;
        errors
    }

    fn is_rate_limit_exceeded(&self) -> bool {
        if self.code() != 403 {
            return false;
        }

        for error in self.errors() {
            if error.reason == "userRateLimitExceeded" {
                return true;
            }
        }

        false
    }
}

const FILE_FIELDS: [&str; 8] = [
    "id",
    "name",
    "mimeType",
    "sha1Checksum",
    "modifiedTime",
    "parents",
    "headRevisionId",
    "size",
];
static FILE_FIELDS_QUERY: Lazy<[(&'static str, &'static str); 1]> = Lazy::new(|| {
    let fields_str = FILE_FIELDS.join(",").leak();
    [("fields", fields_str)]
});

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub modified_time: OffsetDateTime,

    /// Careful: Not all folders have parents. For instance, the 'root' folder does not have a parent
    /// (and possibly others, I'm not quite sure)
    pub parents: Option<[String; 1]>,

    /// Consumers can use this field to determine the type of the File. If this is `Some(MoreFileFields::BinaryFile { .., })`, then
    /// this is a binary file. If this is a directory, this is None. Other combinations are possible too...
    #[serde(flatten)]
    pub more: Option<MoreFileFields>,
}

#[derive(Deserialize, Debug, PartialEq, Clone)]
#[serde(untagged)]
pub enum MoreFileFields {
    #[serde(rename_all = "camelCase")]
    BinaryFile {
        head_revision_id: String,
        sha1_checksum: String,
        #[serde(deserialize_with = "from_str")]
        size: u64,
    },
    #[serde(rename_all = "camelCase")]
    Sized {
        #[serde(deserialize_with = "from_str")]
        size: u64,
    },
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileId {
    pub id: String,
}

impl File {
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME_TYPE
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

impl FilesReq {
    pub fn new_create_folder(name: String, parent_id: String) -> Self {
        Self {
            name: Some(name),
            parents: Some(vec![parent_id]),
            mime_type: Some(FOLDER_MIME_TYPE.to_owned()),
            modified_time: None,
        }
    }
}

/// This struct just exists to extract common paramters that would
/// need to be passed to each function here in any case. It should not be kept
/// around and is generally extremely short lived.
#[derive(Debug)]
pub struct DriveApi<'a> {
    client: reqwest::Client,
    credential: &'a GoogleCredential,
}

impl<'a> DriveApi<'a> {
    // Chunksize of 20 MiB seems like a reasonable choice.
    const CHUNKSIZE: usize = (256 * 1024 * 4) * 20;

    pub fn new(client: reqwest::Client, credential: &'a GoogleCredential) -> Self {
        Self { client, credential }
    }

    async fn token(&self) -> Result<AccessToken, ApiError> {
        self.credential.get_or_refresh_token().await
            .map_err(ApiError::Other)
    }

    async fn send_retry<B: Clone + Into<reqwest::Body>>(
        &self,
        req: Request<B>,
    ) -> Result<reqwest::Response, ApiError> {
        const MAXIMUM_RETRIES: u32 = 3;

        log::debug!("Request: {} {}", req.method(), req.uri());

        let mut retries = 0u32;
        loop {
            let converted_req: reqwest::Request = clone_request(&req).try_into().unwrap();
            let resp = self.client.execute(converted_req).await;

            let (err, retryable): (_, bool) = match resp {
                Ok(resp) => {
                    let status = resp.status();

                    // If we got anything except an actual, confirmed error we return the result
                    // for the client to deal with.
                    if !status.is_client_error() && !status.is_server_error() {
                        return Ok(resp);
                    }

                    // Otherwise, try to parse the error
                    let err_str = resp.text().await.map_err(ApiError::HttpError)?;
                    let err = match serde_json::from_str::<GoogleError>(&err_str) {
                        Ok(val) => {
                            assert_eq!(
                                val.code(),
                                status.as_u16(),
                                "Google returned two different status codes"
                            );
                            ApiError::RequestFailed {
                                inner: val,
                                retries,
                            }
                        }
                        Err(err) => ApiError::Other(anyhow!(
                            "HTTP request failed with code {}. Error: {err_str}",
                            status
                        )),
                    };

                    // Decide if this request is worth retrying (most 4xx status codes should not be retried)
                    let retryable = match status.as_u16() {
                        403 => {
                            if let ApiError::RequestFailed { inner, .. } = &err {
                                inner.is_rate_limit_exceeded()
                            } else {
                                false
                            }
                        }
                        // Otherwise, do not retry client errors
                        400..=499 => false,
                        // ...but everything else
                        _ => true,
                    };

                    (err, retryable)
                }
                Err(err) => (ApiError::HttpError(err), true),
            };

            log::warn!("A request to {} failed: {}", req.uri(), err);

            let should_retry = retryable && retries <= MAXIMUM_RETRIES;
            if !should_retry {
                return Err(err);
            }

            let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..=1000));
            // Note: For the third fourth try, retries is 3 and we wait 16 seconds
            let delay = Duration::from_secs(2u64.pow(retries + 1));
            tokio::time::sleep(delay + jitter).await;

            retries += 1;
        }
    }

    async fn send_retry_json<T, B>(&self, req: Request<B>) -> Result<T, ApiError>
    where
        B: Clone + Into<reqwest::Body>,
        T: DeserializeOwned,
    {
        let ret = self
            .send_retry(req)
            .await?
            .json()
            .await
            .map_err(ApiError::HttpError)?;

        Ok(ret)
    }

    /// Note: you should probably always add the query parameter ... and trashed = false
    pub async fn file_list(
        &self,
        q: &str,
        next_page_token: Option<&str>,
    ) -> Result<FilesListResp, ApiError> {
        static PREFIXED_FIELDS: Lazy<String> = Lazy::new(|| {
            let mut s = "nextPageToken".to_owned();
            // The files are "nested", and therefore the fields need to be prefixed,
            // i.e. files/mimeType instead of just mimeType
            for field in FILE_FIELDS {
                write!(&mut s, ",files/{}", field).unwrap();
            }
            s
        });

        // Our response type is hardcoded anyways, and we almost always use the same
        // fields anyway. So we just hardcode it here and move one.
        let url = Url::parse("https://www.googleapis.com/drive/v3/files")
            .unwrap()
            .with_query_pairs(&[
                Some(("fields", PREFIXED_FIELDS.as_str())),
                Some(("pageSize", "1000")),
                // Google does not complain about empty q paramter. Besides, it's never empty in practice
                Some(("q", q)),
                next_page_token.map(|x| ("pageToken", x)),
            ]);

        let req = Request::get(url.to_string())
            .bearer_auth(&self.token().await?)
            .body(EMPTY_BODY)
            .expect("request is valid");

        let ret = self.send_retry_json::<FilesListResp, _>(req).await?;
        Ok(ret)
    }

    /// Wrap file_list and keep reading until nextPageToken is None. Return the end result.
    pub async fn file_list_all(&self, q: &str) -> Result<Vec<File>, ApiError> {
        let mut ret = Vec::new();

        let mut next_page_token: Option<String> = None;
        loop {
            let resp = self.file_list(q, next_page_token.as_deref()).await?;
            ret.extend(resp.files);
            if let Some(t) = resp.next_page_token {
                next_page_token = Some(t);
            } else {
                break;
            }
        }

        Ok(ret)
    }

    pub async fn files_get(&self, id: &str) -> Result<File, ApiError> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .unwrap()
            .with_query_pairs(&*FILE_FIELDS_QUERY);

        let req = Request::get(url.to_string())
            .bearer_auth(&self.token().await?)
            .body(EMPTY_BODY)
            .expect(REQUEST_IS_VALID);

        self.send_retry_json(req).await
    }

    pub async fn files_get_alt_media(
        &self,
        id: &str,
        revision_id: Option<&str>,
    ) -> Result<Box<dyn AsyncRead + Send>, ApiError> {
        let url = if let Some(revision_id) = revision_id {
            format!("https://www.googleapis.com/drive/v3/files/{id}/revisions/{revision_id}")
        } else {
            format!("https://www.googleapis.com/drive/v3/files/{id}")
        };

        let url = Url::parse(&url)
            .unwrap()
            .with_query_pairs(&[("acknowledgeAbuse", "true"), ("alt", "media")]);

        // At some point, we included a a range header in the request, so that Google sends a Content-Range
        // header from which we can know the (uncompressed) size of the body before downloading it.
        // That is kind of flaky, because a) the spec allows them to send the content in multiple parts,
        // which we would probably have to deal with and b) if the file has zero bytes, the range header
        // bytes=0- results in 416 Content Range Not Satisfiable (which we had to work around...)
        let req = Request::get(url.to_string())
            .bearer_auth(&self.token().await?)
            .body(EMPTY_BODY)
            .expect(REQUEST_IS_VALID);

        let resp = self.send_retry(req).await?;

        let ret = resp
            .bytes_stream()
            .map_err(|err| std::io::Error::new(ErrorKind::Other, err))
            .into_async_read();

        Ok(Box::new(ret))
    }

    pub async fn files_delete(&self, id: &str) -> Result<(), ApiError> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}")).unwrap();

        let req = Request::delete(url.to_string())
            .bearer_auth(&self.token().await?)
            .body(EMPTY_BODY)
            .expect(REQUEST_IS_VALID);

        self.send_retry(req).await?;

        Ok(())
    }

    pub async fn files_create(&self, create_request: FilesReq) -> Result<File, ApiError> {
        let url = Url::parse("https://www.googleapis.com/drive/v3/files")
            .unwrap()
            .with_query_pairs(&*FILE_FIELDS_QUERY);

        let req = Request::post(url.to_string())
            .bearer_auth(&self.token().await?)
            .json_body(&create_request)
            .expect(REQUEST_IS_VALID);

        self.send_retry_json(req).await
    }

    pub async fn files_create_upload<R: AsyncRead + Send>(
        &self,
        create_request: FilesReq,
        src: R,
        len: u64,
    ) -> Result<File, ApiError> {
        let url =
            Url::parse("https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable")
                .unwrap()
                .with_query_pairs(&*FILE_FIELDS_QUERY);

        let req = Request::post(url.to_string())
            .bearer_auth(&self.token().await?)
            .json_body(&create_request)
            .expect(REQUEST_IS_VALID);

        let resp = self.send_retry(req).await?;

        let resumable_session_url = resp.headers().get("Location").unwrap().to_str().unwrap();

        let value = self.upload(resumable_session_url, src, len).await?;
        serde_json::from_value(value).map_err(|err| ApiError::Other(err.into()))
    }

    pub async fn files_update_upload<R: AsyncRead + Send>(
        &self,
        id: &str,
        src: R,
        len: u64,
    ) -> Result<File, ApiError> {
        let url = Url::parse(&format!(
            "https://www.googleapis.com/upload/drive/v3/files/{id}?uploadType=resumable"
        ))
        .unwrap()
        .with_query_pairs(&*FILE_FIELDS_QUERY);

        let req = Request::patch(url.to_string())
            // Note that we could send a non empty object here, but for now,
            // we don't. We use another API request to change metadata.
            .header(http::header::CONTENT_TYPE, "application/json")
            .bearer_auth(&self.token().await?)
            .body("{}")
            .expect(REQUEST_IS_VALID);

        let resp = self.send_retry(req).await?;

        let resumable_session_url = resp.headers().get("Location").unwrap().to_str().unwrap();

        let value = self.upload(resumable_session_url, src, len).await?;
        serde_json::from_value(value).map_err(|err| ApiError::Other(err.into()))
    }

    pub async fn files_update(&self, id: &str, update_req: FilesReq) -> anyhow::Result<()> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .unwrap()
            .with_query_pairs(&*FILE_FIELDS_QUERY);

        let req = Request::patch(url.to_string())
            .bearer_auth(&self.token().await?)
            .json_body(&update_req)?;

        let file_id: FileId = self.send_retry(req).await?.json().await?;

        debug_assert_eq!(&file_id.id, id);

        Ok(())
    }

    // Try and read everything from the reader. Try to not put it into memory.
    // At this point, it's already seekable, so we do not have to do that.
    //
    // This is a helper method, and we technically do not really know what it returns, so we use the
    // generic serde_json::Value, and let the caller deal with it...
    async fn upload<R: AsyncRead + Send>(
        &self,
        resumable_session_url: &str,
        src: R,
        len: u64,
    ) -> Result<serde_json::Value, ApiError> {
        let mut src = pin!(src);

        let mut buf = vec![0u8; Self::CHUNKSIZE].into_boxed_slice();
        let mut buf_len = 0;

        let mut resend = 0;
        let mut cursor = 0;
        let value: serde_json::Value = loop {
            if resend == 0 {
                buf_len = 0;
                while buf_len < Self::CHUNKSIZE {
                    let just_read = src
                        .read(&mut buf[buf_len..])
                        .await
                        .map_err(ApiError::IoError)?;
                    if just_read == 0 {
                        break;
                    }

                    buf_len += just_read;
                }
            }
            let send_buffer = &buf[0..buf_len];

            let mut req = Request::put(resumable_session_url)
                .header(http::header::CONTENT_LENGTH, send_buffer.len());
            if !send_buffer.is_empty() {
                let range_header = &format!(
                    "bytes {}-{}/{}",
                    cursor,
                    cursor + send_buffer.len() - 1,
                    len
                );

                req = req.header(http::header::CONTENT_RANGE, range_header);
            }
            let req = req.body(send_buffer.to_vec()).expect(REQUEST_IS_VALID);

            // Note how the retrying will end up cloning the body contained in this request.
            // But the "body" is just a *reference* to an array, and cloning a reference is cheap.
            let resp = self
                .client
                .execute(req.try_into().unwrap())
                .await
                .map_err(ApiError::HttpError)?;

            match resp.status().as_u16() {
                // Success. We are done, extract the field and return it
                200..=299 => break resp.json().await.map_err(ApiError::HttpError)?,

                // Not yet finished
                308 => {
                    if let Some(range) = resp.headers().get(http::header::RANGE) {
                        let (from, to) = parse_range_header(range.to_str().unwrap());
                        assert_eq!(from, 0, "range header indicates that remote has not receveived the beginning of a file but has received later parts");

                        let bytes_received = 1 + to - cursor;

                        if bytes_received < send_buffer.len() {
                            resend += 1;
                        } else {
                            cursor += bytes_received;
                            resend = 0;
                        }
                    } else {
                        // Remote has received no bytes. Loop...
                        // Note that the try counter prevents us from looping indefinitely.
                        todo!("Remote has received no bytes.")
                        // FIXME: Retry here, but be careful: We must ensure that the old bytes are
                        // not overwritten, perhaps by setting offset to its maximum?
                    }
                }
                x => panic!("unexpected status code {}", x),
            }
        };

        log::debug!("Uploaded file and got {:?}", value);

        Ok(value)
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

fn from_str<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: FromStr,
    T::Err: Display,
    D: Deserializer<'de>,
{
    // See https://github.com/serde-rs/json/issues/317#issuecomment-300251188

    let s = String::deserialize(deserializer)?;
    T::from_str(&s).map_err(de::Error::custom)
}

#[cfg(test)]
mod test {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_file_parse() {
        let resp = r#"
            {
              "mimeType": "application/pdf",
              "parents": [
                "1OxsJmztdignVn-FX1qQidCFxP114z50F"
              ],
              "headRevisionId": "0B2qh_6UFaLFoZ0NWQ1NiNWVCNzl0dWZvaERWeHQ0TTVNZWlVPQ",
              "size": "26656929",
              "id": "0B2qh_6UFaLFoOFRyYTEya2FjQ1E",
              "name": "Book.pdf",
              "modifiedTime": "2017-10-24T20:00:23.000Z",
              "sha1Checksum": "91a4f57c4a3a75f472eb86f261b374a764d883fc"
            }
        "#;

        let test: File = serde_json::from_str(resp).unwrap();
        let attributes_should_be = MoreFileFields::BinaryFile {
            head_revision_id: "0B2qh_6UFaLFoZ0NWQ1NiNWVCNzl0dWZvaERWeHQ0TTVNZWlVPQ".to_owned(),
            sha1_checksum: "91a4f57c4a3a75f472eb86f261b374a764d883fc".to_owned(),
            size: 26656929,
        };
        assert_eq!(test.more, Some(attributes_should_be));
    }

    #[test]
    fn test_folder_parse() {
        let resp = r#"
            {
              "mimeType": "application/vnd.google-apps.folder",
              "parents": [
                "12QIsh600sDMID96vFu0EZKN_5S8FW8Qw"
              ],
              "id": "1OxsJmztdignVn-FX1qQidCFxP114z50F",
              "name": "Algebra",
              "modifiedTime": "2018-11-06T00:58:12.616Z"
            }
        "#;

        let test: File = serde_json::from_str(resp).unwrap();
        assert_eq!(test.more, None);
        assert_eq!(test.mime_type, "application/vnd.google-apps.folder");
        assert!(test.is_folder());
    }

    #[test]
    fn errors_can_be_parsed_test() {
        // See https://developers.google.com/drive/api/guides/handle-errors#user-rate-limit
        let data = r#"
            {
               "error":{
                  "errors":[
                     {
                        "domain":"usageLimits",
                        "reason":"userRateLimitExceeded",
                        "message":"User Rate Limit Exceeded"
                     }
                  ],
                  "code":403,
                  "message":"User Rate Limit Exceeded"
               }
            }
        "#;

        let val: GoogleError = serde_json::from_str(data).unwrap();

        assert_eq!(val.code(), 403);
        assert_eq!(val.message(), "User Rate Limit Exceeded");
        assert_eq!(val.errors()[0].reason, "userRateLimitExceeded");
    }
}
