use clap::{command, Parser, Subcommand};
use drive_api::{oauth::{new_google_oauth_client, sign_in, OAuthClientConfig}, FatAccessToken};
use oauth2::RefreshToken;

#[derive(Parser, Debug)]
struct Args {
    secrets_path: String,
    #[command(subcommand)]
    cmd: Commands
}

#[derive(Subcommand, Clone, Debug)]
enum Commands {
    /// Retrieve a refresh and access token by going through the installed app OAuth flow. The first output line
    /// is the refresh token, the second line is the expirey date of the access token and the last line is the access token itself.
    Installed,
    /// Exchange the provided refresh token for an access token. Prints expiration time on the first line, and the token itself on the
    /// second line.
    AccessToken {
        refresh_token: String
    }
}

pub fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let secrets_bytes = std::fs::read(args.secrets_path)?;
    let client_config = OAuthClientConfig::try_from_json(&secrets_bytes)?;
    let client = new_google_oauth_client(client_config);

    match args.cmd {
        Commands::Installed => {
            let (at, rt) = sign_in(&client)?;
            println!("{}", rt.secret());
            println!("{:?}", at.expires());
            println!("{}", at.secret());
        }
        Commands::AccessToken { refresh_token } => {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let res = rt.block_on(async {
                FatAccessToken::try_from_refresh_token(&client, &RefreshToken::new(refresh_token)).await
            })?;

            println!("{:?}", res.expires());
            println!("{}", res.secret());
        }
    }

    Ok(())
}
