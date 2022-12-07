use std::time::Duration;
use std::{fmt::Display, str::FromStr};

use crate::util::clone_request;
use crate::Error;

use super::extensions::RequestBuilderExt;
use super::extensions::UrlExt;
use futures::{AsyncRead, AsyncReadExt};
use http::{Request, Response};
use isahc::AsyncBody;
use isahc::{AsyncReadResponseExt, HttpClient};
use oauth2::AccessToken;
use rand::Rng;
use serde::Serialize;
use serde::{de, Deserialize};
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

    pub fn new(client: HttpClient, token: AccessToken) -> Self {
        Self { client, token }
    }

    async fn send_retry<B: Clone + Into<AsyncBody>>(
        &self,
        req: Request<B>,
    ) -> crate::Result<Response<AsyncBody>> {
        const MAXIMUM_DELAY: Duration = Duration::from_secs(32);

        let mut delay = Duration::from_secs(1);
        loop {
            let resp = self.client.send_async(clone_request(&req)).await;

            let (err, should_retry): (crate::Error, bool) = match resp {
                Ok(resp) => {
                    let status = resp.status();

                    // If we get any status in the range 0 - 300, we return
                    if !status.is_client_error() && !status.is_server_error() {
                        return Ok(resp);
                    }

                    // Otherwise, try to make sense of the error we got
                    let (_, mut body) = resp.into_parts();

                    // This may fail, but we do not care
                    let mut body_str = String::new();
                    let _ = body.read_to_string(&mut body_str).await;

                    let should_retry = match status.as_u16() {
                        403 => {
                            // We are potentially being rate limited, find out if we are.
                            let rate_limited = serde_json::from_str::<serde_json::Value>(&body_str)
                                .ok()
                                .as_ref()
                                .and_then(|x| x.get("error"))
                                .and_then(|x| x.get("errors"))
                                .and_then(|x| x.as_array())
                                .and_then(|x| x.get(0))
                                .and_then(|x| x.get("reason"))
                                .and_then(|x| x.as_str())
                                .map(|x| x == "userRateLimitExceeded")
                                .unwrap_or(false);

                            if rate_limited {
                                println!("We were just rate limited, {:?}", req.uri());
                            }

                            rate_limited
                        }
                        // Otherwise, do not retry client errors
                        400..=499 => false,
                        // ...but everything else
                        _ => true,
                    };
                    let res = Error::HttpStatusError {
                        status: status.as_u16(),
                        resp: body_str,
                    };

                    (res, should_retry)
                }
                Err(err) => (err.into(), true),
            };

            if !should_retry || delay >= MAXIMUM_DELAY {
                return Err(err);
            }

            let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..=1000));
            tokio::time::sleep(delay + jitter).await;
            delay *= 2;
        }
    }

    /// Note: you should probably always add the query parameter ... and trashed = false
    pub async fn file_list(
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

        Ok(self
            .send_retry(req)
            .await?
            .json()
            .await?)
    }

    /// Wrap file_list and keep reading until nextPageToken is None. Return the end result.
    pub async fn file_list_all(&self, q: &str) -> crate::Result<Vec<File>> {
        let mut ret = Vec::new();

        let mut next_token = "".to_string();
        loop {
            let resp = self.file_list(q, Some(&next_token)).await?;
            ret.extend(resp.files);
            if let Some(t) = resp.next_page_token {
                next_token = t
            } else {
                break;
            }
        }

        Ok(ret)
    }

    pub async fn files_get(&self, id: &str) -> crate::Result<File> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .unwrap()
            .with_query_pairs(&[(
                "fields",
                "id,name,mimeType,size,sha1Checksum,modifiedTime,parents",
            )]);

        let req = Request::get(url.to_string())
            .bearer_auth(self.token.secret())
            .body(())?;

        Ok(self
            .send_retry(req)
            .await?
            .json()
            .await?)
    }

    pub async fn files_get_alt_media(
        &self,
        id: &str,
        bytes_range: Option<(u64, u64)>,
    ) -> crate::Result<(isahc::AsyncBody, u64)> {
        let range_header = if let Some((_, _)) = bytes_range {
            todo!(
                "make the output of this work with content length (I think right now it does not)"
            )
        } else {
            "bytes=0-".to_string()
        };

        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}"))
            .unwrap()
            .with_query_pairs(&[Some(("acknowledgeAbuse", "true")), Some(("alt", "media"))]);

        let req = Request::get(url.to_string())
            // We include a range header in any case, so that google always includes a content range
            // response, from which we can know the files length (content-length is unrealiable because
            // the response will almost always be gzip encoded)
            .header(http::header::RANGE, range_header)
            .bearer_auth(self.token.secret())
            .body(())?;

        let resp = self.send_retry(req).await;

        // Including a range header does come with a problem: If the file has 0 bytes,
        // google (more or less correctly) responds with a 416 Range not satisfiable.
        // We just swallow that error if it happens.
        if let Err(crate::Error::HttpStatusError { status: 416, resp: _ }) = &resp {
            let empty_body = AsyncBody::from(Vec::new());
            return Ok((empty_body, 0));
        }

        let resp = resp?;
 
        let len = resp
            .headers()
            .get(http::header::CONTENT_RANGE)
            .and_then(|range| {
                // Try and parse the Content-Range header
                let (_, x) = range.to_str().unwrap().split_once('/')?;
                x.parse::<u64>().ok()
            })
            .or_else(|| {
                // Otherwise, last ditch effort: try and use content length
                resp.body().len()
            })
            .unwrap_or_else(|| {
                panic!(
                    "google sent a file and we have no way of knowing how long it is: {}",
                    id
                )
            });

        Ok((resp.into_body(), len))
    }

    pub async fn files_delete(&self, id: &str) -> crate::Result<()> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}")).unwrap();

        let req = Request::delete(url.to_string())
            .bearer_auth(self.token.secret())
            .body(())?;

        self.send_retry(req).await?;

        Ok(())
    }

    pub async fn files_create(&self, create_request: FilesReq) -> crate::Result<FileId> {
        let url = Url::parse("https://www.googleapis.com/drive/v3/files").unwrap();

        let req = Request::post(url.to_string())
            .bearer_auth(self.token.secret())
            .json_body(&create_request)?;

        Ok(self
            .send_retry(req)
            .await?
            .json()
            .await?)
    }

    pub async fn files_create_upload<R: AsyncRead + Send>(
        &self,
        create_request: FilesReq,
        src: R,
        len: u64,
    ) -> crate::Result<FileId> {
        let url =
            Url::parse("https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable")
                .unwrap();

        let req = Request::post(url.to_string())
            .bearer_auth(self.token.secret())
            .json_body(&create_request)?;

        let resp = self.send_retry(req).await?;

        let resumable_session_url = resp.headers().get("Location").unwrap().to_str().unwrap();

        self.upload(resumable_session_url, src, len).await
    }

    pub async fn files_update_upload<R: AsyncRead + Send>(
        &self,
        id: &str,
        src: R,
        len: u64,
    ) -> crate::Result<FileId> {
        let url = Url::parse(&format!(
            "https://www.googleapis.com/upload/drive/v3/files/{id}?uploadType=resumable"
        ))
        .unwrap();

        let req = Request::patch(url.to_string())
            // Note that we could send a non empty object here, but for now,
            // we don't. We use another API request to change metadata.
            .header(http::header::CONTENT_TYPE, "application/json")
            .bearer_auth(self.token.secret())
            .body("{}")?;

        let resp = self.send_retry(req).await?;

        let resumable_session_url = resp.headers().get("Location").unwrap().to_str().unwrap();

        self.upload(resumable_session_url, src, len).await
    }

    pub async fn files_update(&self, id: &str, update_req: FilesReq) -> crate::Result<()> {
        let url = Url::parse(&format!("https://www.googleapis.com/drive/v3/files/{id}")).unwrap();

        let req = Request::patch(url.to_string())
            .bearer_auth(self.token.secret())
            .json_body(&update_req)?;

        let file_id: FileId = self
            .send_retry(req)
            .await?
            .json()
            .await?;

        debug_assert_eq!(&file_id.id, id);

        Ok(())
    }

    // Try and read everything from the reader. Try to not but it into memory.
    // At this point, it's already seekable, so we do not have to do that.
    async fn upload<R: AsyncRead + Send>(
        &self,
        resumable_session_url: &str,
        src: R,
        len: u64,
    ) -> crate::Result<FileId> {
        // I have no idea what I'm doing here. I should probably read that blog
        // post "Pin and suffering" again (great title btw)
        let mut src = Box::pin(src);

        let mut buf = vec![0u8; Self::CHUNKSIZE].into_boxed_slice();
        let mut offset = 0;

        let mut bytes_sent = 0;
        let id: FileId = loop {
            // It could happen, that we still have data to send from the previous iteration.
            // This is possible, because google might not consume everything we send,
            // i.e. indicate with the range header that it has not consumed the entire bytes.
            // We carry over the remaining bytes into the next iteration and send them again
            // along with new data, if possible.
            let read = src.read(&mut buf[offset..]).await?;
            let send_buffer = &buf[..offset + read];

            let range_header = &format!(
                "bytes {}-{}/{}",
                bytes_sent,
                bytes_sent + send_buffer.len() - 1,
                len
            );

            let req = Request::put(resumable_session_url)
                .header(http::header::CONTENT_RANGE, range_header)
                .body(send_buffer)?;

            // Note how the retrying will end up cloning the body
            // contained in this request. But the "body" is just a *reference* to an array,
            // and cloning a reference is cheap.
            let mut resp = self
                .send_retry(req)
                .await?;

            match resp.status().as_u16() {
                // Success. We are done, extract the field and return it
                200..=299 => break resp.json().await?,

                // Not yet finished
                308 => {
                    if let Some(range) = resp.headers().get(http::header::RANGE) {
                        let (from, to) = parse_range_header(range.to_str().unwrap());
                        assert_eq!(from, 0, "range header indicates that remote has not receveived the beginning of a file but has received later parts");

                        let bytes_received = 1 + to - bytes_sent;
                        bytes_sent += bytes_received;

                        if bytes_received < buf.len() {
                            // FIXME: This path is very cold and completely untested

                            // If remote has not processed the whole data, carry this over into the
                            // next iteration...
                            buf.copy_within(0..bytes_received, 0);

                            //...but we also have to care to not overwrite this data with fresh data
                            offset = bytes_received;
                        } else {
                            // Remote has processed everything we sent
                            offset = 0;
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
