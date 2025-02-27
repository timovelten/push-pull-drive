//! This module is dedicated to actually making the API calls we need to
//! Google Drive. It does not much beyond that. In particular, it
//! does not handle authentication by itself.

use std::fmt::Write as _;
use std::io::ErrorKind;
use std::pin::{pin, Pin};
use std::sync::LazyLock;
use std::time::Duration;
use std::{fmt::Display, str::FromStr};

use super::extensions::RequestBuilderExt;
use super::extensions::UrlExt;
use super::GoogleCredential;
use anyhow::anyhow;
use futures::{AsyncRead, AsyncReadExt, TryStreamExt};
use http::Request;
use oauth2::AccessToken;
use rand::Rng;
use serde::de::DeserializeOwned;
use serde::{de, Deserialize};
use serde::{Deserializer, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

const REQUEST_IS_VALID: &str = "request is valid";

pub const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

#[derive(Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    HttpError(reqwest::Error),
    #[error(transparent)]
    IoError(futures::io::Error),
    #[error("HTTP request had non 2xx status code (retries: {retries}): {inner:?}")]
    RequestFailed { inner: GoogleError, retries: u32 },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Deserialize, Debug)]
pub struct GoogleErrorDetails {
    pub domain: String,
    pub reason: String,
    pub message: String,
}

/// I can't seem to find an official document stating that all errors of all Google APIs look
/// like this. The Google Drive ones are (kind of) detailed here [https://developers.google.com/drive/api/guides/handle-errors].
///
/// Also the official go client api library also assumes this structure [https://github.com/googleapis/google-api-go-client/blob/e3868f25dbd081e7d28a8d75d0ea4a5364027f80/googleapi/googleapi.go#L136]
/// so it probably is specified somehwere (at least internally).
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum GoogleError {
    Error {
        code: u16,
        message: String,
        errors: Vec<GoogleErrorDetails>,
    },
}

impl GoogleError {
    pub fn code(&self) -> u16 {
        let Self::Error { code, .. } = self;
        *code
    }

    pub fn message(&self) -> &str {
        let Self::Error { message, .. } = self;
        message
    }

    pub fn errors(&self) -> &[GoogleErrorDetails] {
        let Self::Error { errors, .. } = self;
        errors
    }

    pub fn is_rate_limit_exceeded(&self) -> bool {
        self.code() == 403 && self.has_reason("userRateLimitExceeded")
    }

    /// The reasons are not well documented. I am aware of the following:
    /// - All the reasons listed here https://developers.google.com/drive/api/guides/handle-errors
    /// - parentNotAFolder
    pub fn has_reason(&self, reason: &str) -> bool {
        for error in self.errors() {
            if error.reason == reason {
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
static FILE_FIELDS_QUERY: LazyLock<[(&'static str, &'static str); 1]> = LazyLock::new(|| {
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

    pub fn parent(&self) -> Option<&str> {
        match &self.parents {
            Some([p]) => Some(p.as_str()),
            None => None,
        }
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

#[derive(Debug)]
pub struct DriveApi {
    client: reqwest::Client,
    credential: GoogleCredential,
}

impl DriveApi {
    pub fn new(client: reqwest::Client, credential: GoogleCredential) -> Self {
        Self { client, credential }
    }

    async fn token(&self) -> Result<AccessToken, ApiError> {
        self.credential
            .get_or_refresh_token()
            .await
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
            let converted_req: reqwest::Request = req.clone().try_into().unwrap();
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
                        Err(_) => ApiError::Other(anyhow!(
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

            let jitter = Duration::from_millis(rand::rng().random_range(0..=1000));

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
        static PREFIXED_FIELDS: LazyLock<String> = LazyLock::new(|| {
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
            .empty_body()
            .expect(REQUEST_IS_VALID);

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
            .empty_body()
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
            .empty_body()
            .expect(REQUEST_IS_VALID);

        let resp = self.send_retry(req).await?;

        let ret = resp
            .bytes_stream()
            .map_err(|err| std::io::Error::new(ErrorKind::Other, err))
            .into_async_read();

        Ok(Box::new(ret))
    }

    /// Note:
    /// - This will error "notFound" when the id does not exist
    pub async fn files_delete(&self, id: &str) -> Result<(), ApiError> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}")).unwrap();

        let req = Request::delete(url.to_string())
            .bearer_auth(&self.token().await?)
            .empty_body()
            .expect(REQUEST_IS_VALID);

        self.send_retry(req).await?;

        Ok(())
    }

    /// Note that this will error if the specified parent is not a folder (403, with reason parentNotAFolder)
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

        let value = self.upload(resumable_session_url, src).await?;
        serde_json::from_value(value).map_err(|err| ApiError::Other(err.into()))
    }

    pub async fn files_update_upload<R: AsyncRead + Send>(
        &self,
        id: &str,
        src: R,
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

        let value = self.upload(resumable_session_url, src).await?;
        serde_json::from_value(value).map_err(|err| ApiError::Other(err.into()))
    }

    pub async fn files_update(&self, id: &str, update_req: FilesReq) -> Result<(), ApiError> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .unwrap()
            .with_query_pairs(&*FILE_FIELDS_QUERY);

        let req = Request::patch(url.to_string())
            .bearer_auth(&self.token().await?)
            .json_body(&update_req)?;

        let file_id: FileId = self
            .send_retry(req)
            .await?
            .json()
            .await
            .map_err(|err| ApiError::Other(err.into()))?;

        debug_assert_eq!(&file_id.id, id);

        Ok(())
    }

    pub async fn files_trash(&self, id: &str) -> Result<(), ApiError> {
        let url = Url::parse(&format!(
            "https://www.googleapis.com/drive/v2/files/{id}/trash"
        ))
        .unwrap();

        let req = Request::post(url.to_string())
            .bearer_auth(&self.token().await?)
            .empty_body()   
            .expect(REQUEST_IS_VALID);

        let _id: FileId = self
            .send_retry(req)
            .await?
            .json()
            .await
            .map_err(|err| ApiError::Other(err.into()))?;

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
    ) -> Result<serde_json::Value, ApiError> {
        /// Google requires all but the last request to have at least a body length of 256 KB,
        /// and also for buffer lengths to be multiples of 256 kb.
        const MIN_SIZE: usize = 256 * 1024;

        /// Chunksize of 20 MiB seems like a reasonable choice. Should be quite a bit larger than [Self::MIN_SIZE].
        const CHUNKSIZE: usize = (256 * 1024 * 4) * 20;

        // See https://developers.google.com/drive/api/guides/manage-uploads#http---multiple-requests
        let mut reader = pin!(src);
        let mut upload_buffer = pin!(UploadBuffer::new(MIN_SIZE, CHUNKSIZE));

        let ret: serde_json::Value = loop {
            upload_buffer
                .as_mut()
                .fill(&mut reader)
                .await
                .map_err(ApiError::IoError)?;

            let (send_buf, last) = match upload_buffer.buffer() {
                Ok(send_buf) => (send_buf, false),
                Err([]) => return self.upload_empty_file(resumable_session_url).await,
                Err(send_buf) => (send_buf, true),
            };

            let pos = upload_buffer.global_pos();
            let end_pos = pos + send_buf.len() - 1;

            // Build the PUT request, making sure to not generate an invalid content range header
            let range_header = if last {
                format!("bytes {pos}-{end_pos}/{}", end_pos + 1)
            } else {
                format!("bytes {pos}-{end_pos}/*")
            };

            let req = Request::put(resumable_session_url)
                .header(http::header::CONTENT_RANGE, range_header)
                .header(http::header::CONTENT_LENGTH, send_buf.len())
                .body(NoDebugBody(send_buf.to_vec()))
                .unwrap();

            log::debug!("UPLOAD {req:?}");

            // 3. Send the request
            let resp = self
                .client
                .execute(req.try_into().unwrap())
                .await
                .map_err(ApiError::HttpError)?;

            // 4. Process the response
            let bytes_received: usize = match resp.status().as_u16() {
                200..=299 => break resp.json().await.map_err(ApiError::HttpError)?,
                // 308 is actually 'Permanent Redirect' but Google uses it as 'Resume Incomplete'
                308 => {
                    match resp.headers().get(http::header::RANGE) {
                        Some(range_header) => {
                            // Ignore the "from" part of a range. I believe these should start at zero,
                            // but if they don't there is also nothing we can really do with that information
                            // except crash.
                            let (_, to) = parse_range_header(range_header.to_str().unwrap());

                            // We know that to >= consumed - 1, so this value is >= 0
                            (1 + to) - upload_buffer.global_pos
                        }
                        None => 0,
                    }
                }
                404 => return Err(ApiError::Other(anyhow!("Expired upload session"))),

                x => {
                    let mut err = anyhow!("Unexpected status code: {x}");
                    if let Ok(body) = resp.text().await {
                        err = err.context(body);
                    }
                    return Err(ApiError::Other(err));
                }
            };

            upload_buffer.as_mut().consume(bytes_received);
        };

        Ok(ret)
    }

    async fn upload_empty_file(
        &self,
        resumable_session_url: &str,
    ) -> Result<serde_json::Value, ApiError> {
        // https://developers.google.com/drive/api/guides/manage-uploads#http---single-request
        let req = Request::put(resumable_session_url)
            .header(http::header::CONTENT_LENGTH, 0)
            .empty_body()
            .expect(REQUEST_IS_VALID);

        self.client
            .execute(req.try_into().unwrap())
            .await
            .map_err(ApiError::HttpError)?
            .json()
            .await
            .map_err(ApiError::HttpError)
    }
}

struct NoDebugBody<T>(T);

impl<T: Into<reqwest::Body>> From<NoDebugBody<T>> for reqwest::Body {
    fn from(value: NoDebugBody<T>) -> Self {
        value.0.into()
    }
}

impl<T> std::fmt::Debug for NoDebugBody<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[omitted]")
    }
}

struct UploadBuffer {
    buf: Box<[u8]>,
    pos: usize,
    end: usize,
    /// Number of bytes that have been consumed so far.
    global_pos: usize,

    /// The preferred chunksize. Every single chunk that is emitted by this (via
    /// a call to [Self::buffer]) should ideally be a multiple of this value.
    unit: usize,
    /// Indicates whether the reader is exhausted.
    exhausted: bool,
}

impl UploadBuffer {
    fn new(unit: usize, capacity: usize) -> Self {
        let cap = if capacity % unit == 0 {
            capacity + 1
        } else {
            capacity
        };
        let buffer = vec![0; cap];

        Self {
            buf: buffer.into_boxed_slice(),
            end: 0,
            pos: 0,
            global_pos: 0,
            unit,
            exhausted: false,
        }
    }

    async fn fill<R: AsyncRead>(&mut self, reader: &mut Pin<&mut R>) -> Result<(), std::io::Error> {
        // This function tries to ensure that after it finishes, the buffers length is not a multiple of
        // of unit. So that buffer can give out some multiple of unit without giving away the whole buffer.
        let remaining_buf_len = self.end - self.pos;
        if remaining_buf_len >= self.unit {
            return Ok(());
        }

        // This is what is still in the buffer, preserve it to be emitted another time.
        // This is definitely < unit. And in fact, usually, it will only be a single byte
        // (because we chose capacity that way).
        self.buf.copy_within(self.pos..self.end, 0);
        self.end = remaining_buf_len;
        self.pos = 0;

        while self.end < self.unit || self.end % self.unit == 0 {
            let r = reader.read(&mut self.buf[self.end..]).await?;
            self.end += r;
            if r == 0 {
                self.exhausted = true;
                break;
            }
        }

        // At this point either
        // 1) The buffer is >= self.unit and its length is not divisible by unit
        // 2) The reader is exhausted

        Ok(())
    }

    /// Ok(buf) is guaranteed to not be the last buffer, and also
    /// to be a multiple of `min_size`.
    ///
    /// Err(buf) is guranteed to be the last buffer (i.e. the inner reader has reached EOF).
    /// It may or may not be a multiple of `min_size`.
    fn buffer(&self) -> Result<&[u8], &[u8]> {
        if self.exhausted {
            Err(&self.buf[self.pos..self.end])
        } else {
            // If exhausted is false, then fill was definitely able to make sure that
            // end is not a multiple of min_size, and we can leave a bit in the buffer
            let end = round_down_to_multiple(self.end, self.unit);
            Ok(&self.buf[self.pos..end])
        }
    }

    fn global_pos(&self) -> usize {
        self.global_pos
    }

    /// Note that this may consume fewer bytes if bytes is not a multiple of `unit``
    fn consume(&mut self, bytes: usize) {
        let bytes = round_down_to_multiple(bytes, self.unit);

        self.pos += bytes;
        self.global_pos += bytes;
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

fn round_down_to_multiple(a: usize, b: usize) -> usize {
    (a / b) * b
}

// Serde functions we need to serialize and deserialize the data correctly
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

    #[test]
    fn errors_can_be_parsed_2() {
        let data = r#"
            {
                "error": {
                "code": 400,
                "message": "Invalid Value",
                "errors": [
                    {
                    "message": "Invalid Value",
                    "domain": "global",
                    "reason": "invalid",
                    "location": "q",
                    "locationType": "parameter"
                    }
                ]
                }
            }
        "#;

        let _: GoogleError = serde_json::from_str(data).unwrap();
    }

    const MIB: usize = 256 * 1024 * 4;

    #[tokio::test]
    async fn upload_buffer_exact_match() {
        let n = 256 * 1024;

        let mut reader = pin!(&vec![0; n][..]);
        let mut buffer = pin!(UploadBuffer::new(n, 10 * n));

        buffer.fill(&mut reader).await.unwrap();
        assert_eq!(buffer.buffer().unwrap_err().len(), n);
    }

    #[tokio::test]
    async fn large() {
        let n = 256 * 1024;
        let src = vec![0; MIB * 500];

        let mut reader = pin!(&src[..]);
        let mut buffer = UploadBuffer::new(n, 20 * MIB);

        let mut len = 0;
        loop {
            buffer.fill(&mut reader).await.unwrap();
            let (end, buf_len) = match buffer.buffer() {
                Ok(buf) => (false, buf.len()),
                Err(buf) => (true, buf.len()),
            };
            len += buf_len;
            println!("{len}");

            if end {
                break;
            }

            buffer.consume(buf_len);
        }

        assert_eq!(len, src.len());
    }

    #[tokio::test]
    async fn not_as_large() {
        let n = 256 * 1024;
        let mut reader = pin!(&vec![0; n + 1][..]);
        let mut buffer = pin!(UploadBuffer::new(n, 20 * MIB));

        buffer.fill(&mut reader).await.unwrap();
        assert_eq!(buffer.buffer().unwrap().len(), n);
        buffer.as_mut().consume(n);

        buffer.fill(&mut reader).await.unwrap();
        assert_eq!(buffer.buffer().unwrap_err().len(), 1);
    }

    #[tokio::test]
    async fn empty_reader() {
        let n = 256 * 1024;
        let mut reader = pin!(&vec![0; 0][..]);
        let mut buffer = pin!(UploadBuffer::new(n, 20 * MIB));

        buffer.fill(&mut reader).await.unwrap();
        assert_eq!(buffer.buffer().unwrap_err().len(), 0);
    }
}
