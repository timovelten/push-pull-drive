use crate::context::ExecutionEnvironment;
use crate::gdrive::client::{self, GDriveClient};
use anyhow::{Context as _, anyhow, bail};
use drive_api::oauth::{self, ClientId, ClientSecret, GoogleOAuthClient};
use drive_api::{FatAccessToken, GoogleCredential, RefreshToken};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use time::OffsetDateTime;

use super::GDriveRepo;

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    client_id: ClientId,
    client_secret: ClientSecret,
    client_config: client::Preferences,
    refresh_token: RefreshToken,

    // We do not store the access token on disk. We just request a new one on startup.
    // Its quite a bit simpler and not actually that slow.
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

pub async fn init(
    ctx: &ExecutionEnvironment,
    GoogleDriveInitArgs { secrets_path }: GoogleDriveInitArgs,
) -> anyhow::Result<()> {
    let secrets_bytes = std::fs::read(secrets_path)?;
    let (client_id, client_secret) = load_google_oauth_json(&secrets_bytes)?;
    let client = GoogleOAuthClient::new(client_id.clone(), client_secret.clone());

    // Login the user using the OAuth device flow
    let (token, refresh_token) = client.sign_in().await?;
    let config = Config {
        client_id,
        client_secret,
        refresh_token,
        client_config: Default::default(),
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
    let client = GoogleOAuthClient::new(config.client_id, config.client_secret);

    let cred = GoogleCredential::new(client, config.refresh_token, None);
    let client = GDriveClient::with_default_service_from_config(cred, config.client_config);
    let repo = GDriveRepo {
        inner: Arc::new(client),
    };

    Ok(Some(repo))
}

/// bytes must be a utf8 encoded json file with fields `installed/client_secret` and `installed/client_id`
/// that contain the OAuth client secret and OAuth client id. This file can be crated by hand or downloaded
/// from the google cloud console afrer creating a new Desktop OAuth Client at
/// [https://console.cloud.google.com/apis/credentials]
fn load_google_oauth_json(bytes: &[u8]) -> anyhow::Result<(ClientId, ClientSecret)> {
    #[derive(Deserialize)]
    struct Config {
        installed: InstalledClient,
    }
    #[derive(Deserialize)]
    struct InstalledClient {
        client_id: ClientId,
        client_secret: ClientSecret,
    }

    let cfg: Config = serde_json::from_slice(bytes)?;
    Ok((cfg.installed.client_id, cfg.installed.client_secret))
}
