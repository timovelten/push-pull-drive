#[cfg(not(target_arch = "wasm32"))]
use std::{future::Future, pin::Pin};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
};

use anyhow::{anyhow, bail};
use oauth2::{
    AsyncHttpClient, AuthorizationCode, CsrfToken, EndpointNotSet, EndpointSet, HttpClientError,
    HttpRequest, HttpResponse, PkceCodeChallenge, RedirectUrl, RefreshToken, Scope, TokenResponse,
    basic::BasicClient,
};
pub use oauth2::{AuthUrl, ClientId, ClientSecret, RevocationUrl, TokenUrl};
use time::OffsetDateTime;
use tokio::task::spawn_blocking;
use url::Url;

use crate::credential::FatAccessToken;

#[derive(Debug, Clone)]
pub struct GoogleOAuthClient {
    pub client_secret: ClientSecret,
    pub client_id: ClientId,
    http_client: ReqwestClient,
}

impl GoogleOAuthClient {
    pub fn new(client_id: ClientId, client_secret: ClientSecret) -> Self {
        Self {
            client_id,
            client_secret,
            http_client: Default::default()
        }
    }

    fn oauth_client(
        &self,
    ) -> BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointSet, EndpointSet> {
        let auth_url = AuthUrl::from_url(
            "https://accounts.google.com/o/oauth2/v2/auth"
                .parse()
                .unwrap(),
        );

        let token_url = TokenUrl::from_url(
            "https://www.googleapis.com/oauth2/v3/token"
                .parse()
                .unwrap(),
        );

        let revocation_url =
            RevocationUrl::from_url("https://oauth2.googleapis.com/revoke".parse().unwrap());

        BasicClient::new(self.client_id.to_owned())
            .set_client_secret(self.client_secret.to_owned())
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_revocation_url(revocation_url)
    }

    pub async fn sign_in(&self) -> anyhow::Result<(FatAccessToken, RefreshToken)> {
        // This is very much based on the example from the oauth2 crate
        // https://github.com/ramosbugs/oauth2-rs/blob/main/examples/google.rs

        // Start the listener first, so that we can get a free port guaranteed
        // We need it now, to build the redirect URI
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let client = self.oauth_client().set_redirect_uri(
            RedirectUrl::new(format!("http://localhost:{}", port)).expect("invalid redirect url"),
        );

        let http_client = ReqwestClient::default();

        let (pkce_code_challenge, pkce_code_verifier) = PkceCodeChallenge::new_random_sha256();

        let (authorize_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(
                "https://www.googleapis.com/auth/drive".to_string(),
            ))
            .set_pkce_challenge(pkce_code_challenge)
            .add_extra_param("access_type", "offline")
            .url();

        eprintln!("Open this URL in your browser\n\n{}\n", authorize_url);

        let (code, ret_csrf_token) =
            spawn_blocking(|| listen_for_code(listener).expect("failed to listen for code"))
                .await
                .unwrap();

        assert_eq!(
            csrf_token.secret(),
            ret_csrf_token.secret(),
            "csrf token mismatch"
        );

        let now = OffsetDateTime::now_utc();
        let token_response = client
            .exchange_code(code)
            .set_pkce_verifier(pkce_code_verifier)
            .request_async(&http_client)
            .await
            .unwrap();

        let fat_token = FatAccessToken {
            access_token: token_response.access_token().clone(),
            expires: now
                + token_response
                    .expires_in()
                    .expect("token should have expires_in field"),
        };

        Ok((fat_token, token_response.refresh_token().unwrap().clone()))
    }

    pub async fn exchange_refresh_token_for_access_token(
        &self,
        rt: &RefreshToken,
    ) -> anyhow::Result<FatAccessToken> {
        let now = OffsetDateTime::now_utc();

        let token_response = self
            .oauth_client()
            .exchange_refresh_token(rt)
            .request_async(&self.http_client)
            .await
            .map_err(|e| anyhow!(e))?;

        let fat_token = FatAccessToken {
            access_token: token_response.access_token().clone(),
            expires: now
                + token_response
                    .expires_in()
                    .expect("expires_in field exists"),
        };

        Ok(fat_token)
    }
}

fn listen_for_code(listener: TcpListener) -> anyhow::Result<(AuthorizationCode, CsrfToken)> {
    let Some(mut stream) = listener
        .incoming()
        .flatten() // Take only okay streams
        .next()
    else {
        bail!("Expected exactly one incoming connection, but received none.")
    };

    let mut reader = BufReader::new(&stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();

    // First part ot the request line is the HTTP Verb, we ignore that
    let request_url = request_line.split_ascii_whitespace().nth(1).unwrap();
    let url = Url::parse(&format!("http://localhost{request_url}")).unwrap();

    let (_, code) = url.query_pairs().find(|(key, _)| key == "code").unwrap();
    let ret_code = AuthorizationCode::new(code.into_owned());

    let (_, state) = url.query_pairs().find(|(key, _)| key == "state").unwrap();
    let ret_state = CsrfToken::new(state.into_owned());

    let message = "Go back to your terminal :)";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
        message.len(),
        message
    );
    stream.write_all(response.as_bytes()).unwrap();

    Ok((ret_code, ret_state))
}

// Partly copied from upstream alpha crate to not take the dependency
// (code seems simple enough)
// https://github.com/ramosbugs/oauth2-rs/blob/main/oauth2-reqwest/src/lib.rs

#[derive(Clone, Debug)]
struct ReqwestClient(reqwest::Client);

impl Default for ReqwestClient {
    fn default() -> Self {
        Self(
            reqwest::ClientBuilder::new()
                // Following redirects opens the client up to SSRF vulnerabilities.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("Client should build"),
        )
    }
}

impl From<reqwest::Client> for ReqwestClient {
    fn from(inner: reqwest::Client) -> Self {
        Self(inner)
    }
}

impl<'c> AsyncHttpClient<'c> for ReqwestClient {
    type Error = HttpClientError<reqwest::Error>;

    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let response = self
                .0
                .execute(request.try_into().map_err(Box::new)?)
                .await
                .map_err(Box::new)?;

            let mut builder = http::Response::builder().status(response.status());
            for (name, value) in response.headers().iter() {
                builder = builder.header(name, value);
            }

            builder
                .body(response.bytes().await.map_err(Box::new)?.to_vec())
                .map_err(HttpClientError::Http)
        })
    }
}
