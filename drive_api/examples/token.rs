use clap::{Parser, Subcommand};
use drive_api::oauth::GoogleOAuthClient;
use oauth2::{ClientId, ClientSecret, RefreshToken};

#[derive(Parser, Debug)]
struct Args {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand, Clone, Debug)]
enum Commands {
    /// Retrieve a refresh and access token by going through the installed app OAuth flow. The first output line
    /// is the refresh token, the second line is the expirey date of the access token and the last line is the access token itself.
    Installed,
    /// Exchange the provided refresh token for an access token. Prints expiration time on the first line, and the token itself on the
    /// second line.
    AccessToken { refresh_token: String },
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    // I recommend to put these values in a dotenv file, and then running this as
    // dotenv run cargo run --example simple
    env_logger::Builder::from_default_env().init();

    let args = Args::parse();
    let client_id = ClientId::new(std::env::var("GOOGLE_CID").expect("GOOGLE_CID should be set"));
    let client_secret =
        ClientSecret::new(std::env::var("GOOGLE_SECRET").expect("GOOGLE_SECRET should be set"));
    let client = GoogleOAuthClient::new(client_id, client_secret);

    match args.cmd {
        Commands::Installed => {
            let (at, rt) = client.sign_in().await?;
            println!("{}", rt.secret());
            println!("{:?}", at.expires());
            println!("{}", at.secret());
        }
        Commands::AccessToken { refresh_token } => {
            let refresh_token = RefreshToken::new(refresh_token);
            let res = client
                .exchange_refresh_token_for_access_token(&refresh_token)
                .await?;

            println!("{:?}", res.expires());
            println!("{}", res.secret());
        }
    }

    Ok(())
}
