use std::fs::File;

use crate::{context::CoreContext, gdrive::oauth, KEY_BACKEND};
use anyhow::{anyhow, Context};
use oauth2::{AccessToken, ClientId, ClientSecret, RefreshToken};
use time::OffsetDateTime;

use super::{FatAccessToken, GDriveBackend, GoogleCredential};

const GDRIVE_BACKEND: &str = "gdrive";
const KEY_GDRIVE_CLIENT_SECRET: &str = "s";
const KEY_GDRIVE_CLIENT_ID: &str = "cid";
const KEY_GDRIVE_TOKEN: &str = "t";
const KEY_GDRIVE_TOKEN_EXPIRES: &str = "exp";
const KEY_GDRIVE_REFRESH_TOKEN: &str = "rt";

#[derive(clap::Args, Clone)]
pub struct GoogleDriveInitArgs {
    /// Path to a JSON file which contains the two keys `installed/client_id` and
    /// `installed/client_secret` with values the OAuth Client ID and Client Secret respectively.
    /// You can create this file yourself or download it from the Google Cloud Console using the
    /// "Download JSON" button after creating a new Desktop OAuth Client at
    /// <https://console.cloud.google.com/apis/credentials>
    #[arg(value_name = "path")]
    secrets_path: String,
}

pub fn init(
    ctx: &CoreContext,
    GoogleDriveInitArgs { secrets_path }: GoogleDriveInitArgs,
) -> anyhow::Result<()> {
    let f = File::open(secrets_path)?;
    let parsed: serde_json::Value = serde_json::from_reader(f)?;

    let installed = parsed
        .get("installed")
        .ok_or_else(|| anyhow!("Secrets file invalid. Missing 'installed'."))?;

    let client_id = ClientId::new(
        installed
            .get("client_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("Secrets file invalid. Missing 'client_id'."))?
            .to_owned(),
    );

    let client_secret = ClientSecret::new(
        installed
            .get("client_secret")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("Secrets file invalid. Missing 'client_secret'."))?
            .to_owned(),
    );

    // Login the user using the OAuth device flow
    let google_client = oauth::new_google_oauth_client(client_id.clone(), client_secret.clone());

    let (fat_token, refresh_token) = oauth::sign_in(google_client)?;

    // Store them in the database and return
    ctx.key_value_store.put(KEY_BACKEND, GDRIVE_BACKEND)?;
    ctx.key_value_store
        .put(KEY_GDRIVE_CLIENT_ID, client_id.as_str())?;
    ctx.key_value_store
        .put(KEY_GDRIVE_CLIENT_SECRET, client_secret.secret())?;

    ctx.key_value_store
        .put(KEY_GDRIVE_REFRESH_TOKEN, refresh_token.secret())?;
    ctx.key_value_store
        .put(KEY_GDRIVE_TOKEN, fat_token.access_token.secret())?;
    ctx.key_value_store.put(
        KEY_GDRIVE_TOKEN_EXPIRES,
        &format!("{}", fat_token.expires.unix_timestamp()),
    )?;

    println!("Success");
    Ok(())
}

pub fn load(ctx: &CoreContext) -> anyhow::Result<Option<GDriveBackend>> {
    if ctx.key_value_store.get(KEY_BACKEND)?.as_deref() == Some(GDRIVE_BACKEND) {
        Ok(Some(load_internal(ctx)?))
    } else {
        Ok(None)
    }
}

fn load_internal(ctx: &CoreContext) -> anyhow::Result<GDriveBackend> {
    log::debug!("Loading google drive backend");

    // Read what we previously saved from the database
    let client_id = ctx
        .key_value_store
        .get(KEY_GDRIVE_CLIENT_ID)?
        .with_context(|| KEY_GDRIVE_CLIENT_ID)?;

    let client_secret = ctx
        .key_value_store
        .get(KEY_GDRIVE_CLIENT_SECRET)?
        .with_context(|| KEY_GDRIVE_CLIENT_SECRET)?;

    let last_token = ctx
        .key_value_store
        .get(KEY_GDRIVE_TOKEN)
        .with_context(|| KEY_GDRIVE_REFRESH_TOKEN)?;
    let last_token_expires = ctx
        .key_value_store
        .get(KEY_GDRIVE_TOKEN_EXPIRES)
        .with_context(|| KEY_GDRIVE_TOKEN_EXPIRES)?;

    let last_token = match (last_token, last_token_expires) {
        (Some(tok), Some(exp)) => {
            let expires = OffsetDateTime::from_unix_timestamp(
                exp.parse::<i64>()
                    .with_context(|| "while parsing expiration date")?,
            )
            .expect("expiry date should be a unix timestamp");

            Some(FatAccessToken {
                access_token: AccessToken::new(tok),
                expires,
            })
        }
        (_, _) => None,
    };

    let refresh_token = ctx
        .key_value_store
        .get(KEY_GDRIVE_REFRESH_TOKEN)?
        .with_context(|| KEY_GDRIVE_REFRESH_TOKEN)?;

    // Create some objects in a DI kind of way and initialize the backend
    let google_client =
        oauth::new_google_oauth_client(ClientId::new(client_id), ClientSecret::new(client_secret));

    let cloned_kvs = ctx.key_value_store.clone();
    let credential: GoogleCredential = GoogleCredential::new(
        google_client,
        RefreshToken::new(refresh_token),
        last_token,
        move |token| {
            // Note how we fail silently here. Maybe we shouldn't (?), we'll see
            let _ = cloned_kvs.put(KEY_GDRIVE_TOKEN, token.access_token.secret());
            let _ = cloned_kvs.put(
                KEY_GDRIVE_TOKEN_EXPIRES,
                &token.expires.unix_timestamp().to_string(),
            );
        },
    );

    Ok(GDriveBackend::new(credential))
}
