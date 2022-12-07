use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
};

use oauth2::{
    basic::BasicClient, AuthorizationCode, CsrfToken, PkceCodeChallenge, RedirectUrl,
    RefreshToken, Scope, TokenResponse,
};
use oauth2::{reqwest::http_client, AuthUrl, ClientId, ClientSecret, RevocationUrl, TokenUrl};
use time::OffsetDateTime;
use url::Url;

use super::FatAccessToken;

/// oauth2 BasicClient with Google's AuthUrl, TokenUrl and RevocationUrl set. 
pub fn new_google_oauth_client(client_id: ClientId, client_secret: ClientSecret) -> BasicClient {
    let client = BasicClient::new(
        client_id,
        Some(client_secret),
        AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string()).unwrap(),
        Some(TokenUrl::new("https://www.googleapis.com/oauth2/v3/token".to_string()).unwrap()),
    );

    client.set_revocation_uri(
        RevocationUrl::new("https://oauth2.googleapis.com/revoke".to_string()).unwrap(),
    )
}

pub fn sign_in(client: BasicClient) -> crate::Result<(FatAccessToken, RefreshToken)> {
    // This is very much based on the example from the oauth2 crate
    // https://github.com/ramosbugs/oauth2-rs/blob/main/examples/google.rs

    // Start the listener first, so that we can get a free port guaranteed
    // We need it now, to build the redirect URI
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let client = client.set_redirect_uri(
        RedirectUrl::new(format!("http://localhost:{}", port)).expect("invalid redirect url"),
    );

    let (pkce_code_challenge, pkce_code_verifier) = PkceCodeChallenge::new_random_sha256();

    let (authorize_url, csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(
            "https://www.googleapis.com/auth/drive".to_string(),
        ))
        .set_pkce_challenge(pkce_code_challenge)
        .add_extra_param("access_type", "offline")
        .url();

    println!(
        "Open this URL in tour browser\n\n{}\n",
        authorize_url.to_string()
    );

    let (code, ret_csrf_token) = listen_for_code(listener).unwrap();

    assert_eq!(
        csrf_token.secret(),
        ret_csrf_token.secret(),
        "csrf token mismatch"
    );

    let now = OffsetDateTime::now_utc();
    let token_response = client
        .exchange_code(code)
        .set_pkce_verifier(pkce_code_verifier)
        .request(http_client)
        .unwrap();

    let fat_token = FatAccessToken {
        access_token: token_response.access_token().clone(),
        expires: now + token_response.expires_in().expect("no expires_in field")
    };
    
    Ok((fat_token, token_response.refresh_token().unwrap().clone()))
}

fn listen_for_code(listener: TcpListener) -> Option<(AuthorizationCode, CsrfToken)> {
    for stream in listener.incoming() {
        if let Ok(mut stream) = stream {
            let mut reader = BufReader::new(&stream);

            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();

            // First part ot the request line is the HTTP Verb, we ignore that
            let request_url = request_line.split_ascii_whitespace().nth(1).unwrap();
            let url = Url::parse(&format!("http://localhost{request_url}")).unwrap();

            let code_pair = url
                .query_pairs()
                .find(|pair| {
                    let &(ref key, _) = pair;
                    key == "code"
                })
                .unwrap();

            let (_, value) = code_pair;
            let ret_code = AuthorizationCode::new(value.into_owned());

            let state_pair = url
                .query_pairs()
                .find(|pair| {
                    let &(ref key, _) = pair;
                    key == "state"
                })
                .unwrap();

            let (_, value) = state_pair;
            let ret_state = CsrfToken::new(value.into_owned());

            let message = "Go back to your terminal :)";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
                message.len(),
                message
            );
            stream.write_all(response.as_bytes()).unwrap();

            return Some((ret_code, ret_state));
        }
    }

    None
}
