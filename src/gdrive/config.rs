use crate::context::ExecutionEnvironment;
use crate::gdrive::client::GDriveClient;
use anyhow::{anyhow, Context as _};
use drive_api::oauth::{new_google_oauth_client, ClientConfig};
use drive_api::{FatAccessToken, GoogleCredential};
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use time::OffsetDateTime;

use super::GDriveRepo;

struct Cfg;

const THIS_BACKEND: &str = "gdrive";

impl Cfg {
    pub const BACKEND: &str = "backend";
    pub const CLIENT_SECRET: &str = "s";
    pub const CLIENT_ID: &str = "cid";
    pub const TOKEN: &str = "t";
    pub const TOKEN_EXPIRES: &str = "exp";
    pub const REFRESH_TOKEN: &str = "rt";
}

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
    ctx: &ExecutionEnvironment,
    GoogleDriveInitArgs { secrets_path }: GoogleDriveInitArgs,
) -> anyhow::Result<()> {
    let secrets_bytes = std::fs::read(secrets_path)?;
    let client_config = ClientConfig::try_from_json(&secrets_bytes)?;
    let client = new_google_oauth_client(client_config.clone());

    // Login the user using the OAuth device flow
    let (fat_token, refresh_token) = drive_api::oauth::sign_in(&client)?;

    // Store them in the database and return
    ctx.db.put_value(Cfg::BACKEND, THIS_BACKEND)?;
    ctx.db.put_value(Cfg::CLIENT_ID, &client_config.client_id)?;
    ctx.db
        .put_value(Cfg::CLIENT_SECRET, &client_config.client_secret)?;

    ctx.db
        .put_value(Cfg::REFRESH_TOKEN, refresh_token.secret())?;
    ctx.db.put_value(Cfg::TOKEN, fat_token.secret())?;
    ctx.db.put_value(
        Cfg::TOKEN_EXPIRES,
        &format!("{}", fat_token.expires().unix_timestamp()),
    )?;

    writeln!(ctx.shell.err(), "Success!")?;

    Ok(())
}

pub fn load(ctx: &ExecutionEnvironment) -> anyhow::Result<Option<GDriveRepo>> {
    if ctx.db.get_value(Cfg::BACKEND)?.as_deref() == Some(THIS_BACKEND) {
        Ok(Some(load_internal(ctx)?))
    } else {
        Ok(None)
    }
}

fn load_internal(ctx: &ExecutionEnvironment) -> anyhow::Result<GDriveRepo> {
    log::debug!("Loading google drive backend");

    // Read what we previously saved from the database
    let client_id = ctx.db.must_get_value(Cfg::CLIENT_ID)?;
    let client_secret = ctx.db.must_get_value(Cfg::CLIENT_SECRET)?;

    // Last token may or may not exist
    let last_token = ctx.db.get_value(Cfg::TOKEN)?;
    let last_token_expires = ctx.db.get_value(Cfg::TOKEN_EXPIRES)?;
    let last_token = match (last_token, last_token_expires) {
        (Some(tok), Some(exp)) => {
            let expires = exp
                .parse::<i64>()
                .with_context(|| "failed to parse expiration date")
                .and_then(|val| {
                    OffsetDateTime::from_unix_timestamp(val)
                        .with_context(|| "expiration date is not valid unix timestamp")
                })?;

            Some(FatAccessToken::from_parts(tok, expires))
        }
        (_, _) => None,
    };

    let refresh_token = ctx.db.must_get_value(Cfg::REFRESH_TOKEN)?;

    // Create some objects in a DI kind of way and initialize the backend
    let client_config = ClientConfig {
        client_id,
        client_secret,
    };

    let db = ctx.db.clone();
    let credential: GoogleCredential =
        GoogleCredential::new(client_config, refresh_token, last_token, move |token| {
            // Note how we fail silently here. Maybe we shouldn't (?), we'll see
            let _ = db.put_value(Cfg::TOKEN, token.secret());
            let _ = db.put_value(
                Cfg::TOKEN_EXPIRES,
                &token.expires().unix_timestamp().to_string(),
            );
        });

    Ok(GDriveRepo {
        inner: Arc::new(GDriveClient::new(credential)),
    })
}
