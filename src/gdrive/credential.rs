use std::sync::Mutex;

use anyhow::anyhow;
use oauth2::reqwest::http_client;
use oauth2::{basic::BasicClient, AccessToken, RefreshToken, TokenResponse};
use time::ext::NumericalDuration;
use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub struct FatAccessToken {
    pub access_token: AccessToken,
    pub expires: OffsetDateTime,
}

impl FatAccessToken {
    fn needs_to_be_refreshed(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        self.expires - now < 60.seconds()
    }

    fn is_expired(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        self.expires <= now
    }
}

#[derive(Debug)]
pub struct GoogleCredential {
    inner: Mutex<GoogleCredentialImpl>,
}

impl GoogleCredential {
    pub fn new<F: Fn(FatAccessToken) + 'static + Send>(
        google_client: BasicClient,
        refresh_token: RefreshToken,
        last_token: Option<FatAccessToken>,
        new_token_callback: F,
    ) -> Self {
        let inner = GoogleCredentialImpl {
            client: google_client,
            refresh_token,
            token: last_token,
            callback: Box::new(new_token_callback),
        };

        GoogleCredential {
            inner: Mutex::new(inner),
        }
    }

    pub fn get_or_refresh_token(&self) -> anyhow::Result<AccessToken> {
        self.inner.lock().unwrap().get_or_refresh_token()
    }
}

const _: () = {
    pub const fn assert_send<T: Send>() {}
    pub const fn assert_sync<T: Sync>() {}
    assert_send::<GoogleCredential>();
    assert_sync::<GoogleCredential>();
};

struct GoogleCredentialImpl {
    // Immutable state, this will not be changed after initial creation.
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

    pub fn get_or_refresh_token(&mut self) -> anyhow::Result<AccessToken> {
        if self.needs_to_be_refreshed() {
            self.refresh_token()?;
        }

        Ok(self
            .get_token()
            .expect("there should be a token after successful refresh"))
    }

    pub fn refresh_token(&mut self) -> anyhow::Result<()> {
        let now = OffsetDateTime::now_utc();
        let token_response = self
            .client
            .exchange_refresh_token(&self.refresh_token)
            .request(http_client)
            .map_err(|e| anyhow!(e))?;

        let fat_token = FatAccessToken {
            access_token: token_response.access_token().clone(),
            expires: now
                + token_response
                    .expires_in()
                    .expect("expires_in field exists"),
        };

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
