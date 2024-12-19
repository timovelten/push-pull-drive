use crate::context::ExecutionEnvironment;
use crate::gdrive::client::GDriveClient;
use anyhow::{anyhow, bail, Context as _};
use drive_api::oauth::{self, new_google_oauth_client, ClientConfig};
use drive_api::{FatAccessToken, GoogleCredential, RefreshToken};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use time::OffsetDateTime;

use super::GDriveRepo;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "backend")]
#[serde(rename = "gdrive")]
struct Config {
    client_secret: String,
    cid: String,
    token: FatAccessToken,
    rt: RefreshToken,
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

    let initial_config = Config {
        client_secret: client_config.client_secret,
        cid: client_config.client_id,
        token: fat_token.clone(),
        rt: refresh_token,
    };

    ctx.config.edit_backend_config(|_| Ok(initial_config))?;

    writeln!(ctx.shell.err(), "Success!")?;

    Ok(())
}

pub fn load(ctx: &ExecutionEnvironment) -> anyhow::Result<Option<GDriveRepo>> {
    let Some(config) = ctx.config.backend_config::<Config>()? else {
        return Ok(None);
    };

    let client_config = ClientConfig {
        client_id: config.cid,
        client_secret: config.client_secret,
    };

    let ctx = ctx.clone();
    let on_new_token = move |token: FatAccessToken| {
        let _ = ctx.config.edit_backend_config(|config: Option<Config>| {
            let Some(mut config) = config else {
                bail!("config is initialized");
            };

            config.token = token;
            Ok(config)
        });
    };

    let cred = GoogleCredential::new(client_config, config.rt, Some(config.token), on_new_token);
    let client = GDriveClient::new(cred);
    let repo = GDriveRepo {
        inner: Arc::new(client)
    };

    Ok(Some(repo))
}
