use crate::oauth::GoogleOAuthClient;
use oauth2::{AccessToken, RefreshToken};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::ext::NumericalDuration;

const _: () = {
    pub const fn assert_send<T: Send>() {}
    pub const fn assert_sync<T: Sync>() {}
    assert_send::<GoogleCredential>();
    assert_sync::<GoogleCredential>();
};

#[derive(Debug)]
pub struct GoogleCredential {
    client: GoogleOAuthClient,
    refresh_token: RefreshToken,
    last_access_token: tokio::sync::Mutex<Option<FatAccessToken>>,
}

impl GoogleCredential {
    pub fn new(
        client: GoogleOAuthClient,
        rt: RefreshToken,
        last_access_token: Option<FatAccessToken>,
    ) -> Self {
        Self {
            client,
            refresh_token: rt,
            last_access_token: last_access_token.into(),
        }
    }

    pub async fn get_token(&self, force_refresh: bool) -> anyhow::Result<AccessToken> {
        let mut state = self.last_access_token.lock().await;
        if let Some(at) = &*state
            && !at.needs_to_be_refreshed()
            && !force_refresh
        {
            return Ok(at.access_token.clone());
        }

        // refresh the token
        let new_token = self
            .client
            .exchange_refresh_token_for_access_token(&self.refresh_token)
            .await?;
        *state = Some(new_token.clone());

        Ok(new_token.access_token)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FatAccessToken {
    pub(crate) access_token: AccessToken,
    pub(crate) expires: OffsetDateTime,
}

impl FatAccessToken {
    pub fn from_parts(access_token: String, expires: OffsetDateTime) -> Self {
        Self {
            access_token: AccessToken::new(access_token),
            expires,
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
