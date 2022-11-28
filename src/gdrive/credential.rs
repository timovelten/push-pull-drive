use std::cell::RefCell;

use oauth2::reqwest::http_client;
use oauth2::{
    basic::BasicClient, AccessToken, ClientId,
    RefreshToken, TokenResponse
};
use time::OffsetDateTime;

use crate::Error;

#[derive(Debug, Clone)]
pub struct FatAccessToken {
    pub access_token: AccessToken,
    pub expires: OffsetDateTime,
}

impl FatAccessToken {
    fn needs_to_be_refreshed(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        self.expires - now < time::Duration::SECOND * 60
    }

    fn is_expired(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        self.expires <= now
    }
}

pub struct GoogleCredential {
    // Immutable state, this will not be changed after initial creation.
    client: BasicClient,
    refresh_token: RefreshToken,

    token: RefCell<Option<FatAccessToken>>,

    callback: Box<dyn Fn(FatAccessToken)>,
}

impl std::fmt::Debug for GoogleCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleCredential")
            .field("client", &self.client)
            .field("refresh_token", &self.refresh_token)
            .field("token", &self.token).finish()
    }
}

impl GoogleCredential {
    pub fn new<F: Fn(FatAccessToken) + 'static>(
        google_client: BasicClient,
        refresh_token: RefreshToken,
        last_token: Option<FatAccessToken>,
        new_token_callback: F,
    ) -> Self {
        let ret = Self {
            client: google_client,
            refresh_token,
            token: RefCell::new(last_token),
            callback: Box::new(new_token_callback),
        };

        ret
    }

    pub fn authenticate(&self, req: ureq::Request) -> crate::Result<ureq::Request> {
        let token = self.get_or_refresh_token()?;
        
        let ret = req.query("client", &self.client_id().as_str())
            .set("Authorization", &format!("Bearer {}", token.secret()));

        Ok(ret)
    }

    fn needs_to_be_refreshed(&self) -> bool {
        self.token
            .borrow()
            .as_ref()
            .map(|x| x.needs_to_be_refreshed())
            .unwrap_or(true)
    }

    pub fn get_token(&self) -> Option<AccessToken> {
        if let Some(fat_token) = self.token.borrow().as_ref() {
            if fat_token.is_expired() {
                return None;
            }

            Some(fat_token.access_token.clone())
        } else {
            None
        }
    }

    pub fn get_or_refresh_token(&self) -> crate::Result<AccessToken> {
        if self.needs_to_be_refreshed() {
            self.refresh_token()?;
        }

        Ok(self
            .get_token()
            .expect("token refresh succeeded but got no new token (or token is already expired)"))
    }

    pub fn refresh_token(&self) -> crate::Result<()> {
        let now = OffsetDateTime::now_utc();
        let token_response = self
            .client
            .exchange_refresh_token(&self.refresh_token)
            .request(http_client)
            .map_err(|e| Error::GoogleAuthError(Box::new(e)))?;

        let fat_token = FatAccessToken {
            access_token: token_response.access_token().clone(),
            expires: now + token_response.expires_in().expect("no expires_in field")
        };

        self.token.replace(Some(fat_token.clone()));
        
        (self.callback)(fat_token);

        Ok(())
    }

    pub fn client_id(&self) -> &ClientId {
        self.client.client_id()
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
            expires: OffsetDateTime::now_utc() - time::Duration::SECOND
        };

        assert!(f1.is_expired());

        let f2 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() + 5 * time::Duration::SECOND
        };

        assert!(!f2.is_expired());
    }

    #[test]
    fn test_fat_access_token_needs_refresh() {
        let token = AccessToken::new("abc".to_string());

        let f0 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() - 50 * time::Duration::DAY
        };
        
        assert!(f0.needs_to_be_refreshed());

        let f1 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() + 50 * time::Duration::SECOND
        };

        assert!(f1.needs_to_be_refreshed());

        let f1 = FatAccessToken {
            access_token: token.clone(),
            expires: OffsetDateTime::now_utc() + 100 * time::Duration::SECOND
        };
        assert!(!f1.needs_to_be_refreshed());
    }
}
