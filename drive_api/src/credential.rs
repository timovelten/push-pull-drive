use std::sync::Arc;

use anyhow::anyhow;
use oauth2::{basic::BasicClient, AccessToken, RefreshToken, TokenResponse};
use serde::{Deserialize, Serialize};
use time::ext::NumericalDuration;
use time::OffsetDateTime;
use tokio::sync::Mutex as TokioMutex;

use crate::oauth::{new_google_oauth_client, OAuthClientConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FatAccessToken {
    pub(crate) access_token: AccessToken,
    pub(crate) expires: OffsetDateTime,
}

impl FatAccessToken {
    pub fn from_parts(access_token: String, expires: OffsetDateTime) -> Self {
        Self {
            access_token: AccessToken::new(access_token),
            expires
        }
    }

    pub fn secret(&self) -> &str {
        self.access_token.secret()
    }

    pub fn expires(&self) -> &OffsetDateTime {
        &self.expires
    }

    pub fn needs_to_be_refreshed(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        self.expires - now < 60.seconds()
    }

    pub fn is_expired(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        self.expires <= now
    }

    pub async fn try_from_refresh_token(client: &BasicClient, rt: &RefreshToken) -> anyhow::Result<Self> {
        let now = OffsetDateTime::now_utc();
        let token_response = client
            .exchange_refresh_token(rt)
            .request_async(oauth2::reqwest::async_http_client)
            .await
            .map_err(|e| anyhow!(e))?;

        Ok(Self {
            access_token: token_response.access_token().clone(),
            expires: now
                + token_response
                    .expires_in()
                    .expect("expires_in field exists"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GoogleCredential {
    // I really don't know how to get rid of this lock. We definitely need to avoid having
    // two token requests in flight at once.
    inner: Arc<TokioMutex<GoogleCredentialImpl>>,
}

impl GoogleCredential {
    pub fn new<F: Fn(FatAccessToken) + 'static + Send>(
        client_config: OAuthClientConfig,
        refresh_token: RefreshToken,
        last_token: Option<FatAccessToken>,
        new_token_callback: F,
    ) -> Self {
        let client = new_google_oauth_client(client_config);

        let inner = GoogleCredentialImpl {
            client,
            refresh_token,
            token: last_token,
            callback: Box::new(new_token_callback),
        };

        GoogleCredential {
            inner: Arc::new(TokioMutex::new(inner)),
        }
    }

    pub async fn get_or_refresh_token(&self) -> anyhow::Result<AccessToken> {
        self.inner.lock().await.get_or_refresh_token().await
    }
}

const _: () = {
    pub const fn assert_send<T: Send>() {}
    pub const fn assert_sync<T: Sync>() {}
    assert_send::<GoogleCredential>();
    assert_sync::<GoogleCredential>();
};

struct GoogleCredentialImpl {
    client: BasicClient,
    refresh_token: RefreshToken,
    token: Option<FatAccessToken>,
    callback: Box<dyn Fn(FatAccessToken) + Send>,
}

impl std::fmt::Debug for GoogleCredentialImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleCredential")
            .field("client", &self.client)
            .field("refresh_token", &self.refresh_token)
            .field("token", &self.token)
            .finish()
    }
}

impl GoogleCredentialImpl {
    fn needs_to_be_refreshed(&self) -> bool {
        self.token
            .as_ref()
            .map(|x| x.needs_to_be_refreshed())
            .unwrap_or(true)
    }

    pub fn get_token(&self) -> Option<AccessToken> {
        if let Some(fat_token) = self.token.as_ref() {
            if fat_token.is_expired() {
                return None;
            }

            Some(fat_token.access_token.clone())
        } else {
            None
        }
    }

    pub async fn get_or_refresh_token(&mut self) -> anyhow::Result<AccessToken> {
        if self.needs_to_be_refreshed() {
            self.refresh_token().await?;
        }

        Ok(self
            .get_token()
            .expect("there should be a token after successful refresh"))
    }

    pub async fn refresh_token(&mut self) -> anyhow::Result<()> {
        let fat_token = FatAccessToken::try_from_refresh_token(&self.client, &self.refresh_token).await?;
        self.token = Some(fat_token.clone());
        (self.callback)(fat_token);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fat_access_token_expired() {
        let token = AccessToken::new("abc".to_string());

        let f1 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() - time::Duration::SECOND,
        };

        assert!(f1.is_expired());

        let f2 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() + 5 * time::Duration::SECOND,
        };

        assert!(!f2.is_expired());
    }

    #[test]
    fn test_fat_access_token_needs_refresh() {
        let token = AccessToken::new("abc".to_string());

        let f0 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() - 50 * time::Duration::DAY,
        };

        assert!(f0.needs_to_be_refreshed());

        let f1 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() + 50 * time::Duration::SECOND,
        };

        assert!(f1.needs_to_be_refreshed());

        let f1 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() + 100 * time::Duration::SECOND,
        };
        assert!(!f1.needs_to_be_refreshed());
    }
}
