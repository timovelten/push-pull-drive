use crate::context::ExecutionEnvironment;
use crate::gdrive::client::GDriveClient;
use anyhow::{anyhow, bail, Context as _};
use drive_api::oauth::{self, new_google_oauth_client, OAuthClientConfig};
use drive_api::{FatAccessToken, GoogleCredential, RefreshToken};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use time::OffsetDateTime;

use super::client::ClientConfig;
use super::GDriveRepo;

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    token: FatAccessToken,
    refresh_token: RefreshToken,
    oauth_client_config: OAuthClientConfig,
    client_config: ClientConfig
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
    let client_config = OAuthClientConfig::try_from_json(&secrets_bytes)?;
    let client = new_google_oauth_client(client_config.clone());

    // Login the user using the OAuth device flow
    let (token, refresh_token) = drive_api::oauth::sign_in(&client)?;
    let config = Config {
        oauth_client_config: client_config,
        token,
        refresh_token,
        client_config: Default::default()
    };

    ctx.db.put_value("gdrive_config", config)?;

    writeln!(ctx.err(), "Success!")?;

    Ok(())
}

pub fn load(ctx: &ExecutionEnvironment) -> anyhow::Result<Option<GDriveRepo>> {
    let config = match ctx.db.get_value::<Config>("gdrive_config")? {
        Some(config) => config,
        _ => return Ok(None),
    };

    let ctx = ctx.clone();
    let on_new_token = move |token: FatAccessToken| {
        let _ = ctx.db.edit_value::<Config, _>("gdrive_config", |mut config| {
            config.token = token;
            Ok(config)
        });
    };

    let cred = GoogleCredential::new(
        config.oauth_client_config,
        config.refresh_token,
        Some(config.token),
        on_new_token,
    );
    let client = GDriveClient::with_default_service_from_config(cred, config.client_config);
    let repo = GDriveRepo {
        inner: Arc::new(client),
    };

    Ok(Some(repo))
}
