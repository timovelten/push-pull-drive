use std::cell::RefCell;

use oauth2::ureq::http_client;
use oauth2::{
    basic::BasicClient, AccessToken, AuthUrl, ClientId, ClientSecret, ExtraTokenFields,
    RefreshToken, StandardTokenResponse, TokenResponse,
    TokenType, TokenUrl,
};
use time::OffsetDateTime;

use crate::Error;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://www.googleapis.com/oauth2/v3/token";

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

impl<EE: TokenType, EF: ExtraTokenFields> From<StandardTokenResponse<EF, EE>> for FatAccessToken {
    fn from(resp: StandardTokenResponse<EF, EE>) -> Self {
        let now = OffsetDateTime::now_utc();
        let expires_in = resp
            .expires_in()
            .expect("expires_in should always be present");

        Self {
            access_token: resp.access_token().clone(),
            expires: now + expires_in,
        }
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
        client_id: ClientId,
        client_secret: ClientSecret,
        refresh_token: RefreshToken,
        last_token: Option<FatAccessToken>,
        new_token_callback: F,
    ) -> Self {
        let client = BasicClient::new(
            client_id,
            Some(client_secret),
            AuthUrl::new(AUTH_URL.to_string()).unwrap(),
            Some(TokenUrl::new(TOKEN_URL.to_string()).unwrap()),
        );

        let ret = Self {
            client,
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
        let token_response = self
            .client
            .exchange_refresh_token(&self.refresh_token)
            .request(http_client)
            .map_err(|e| Error::GoogleAuthError(Box::new(e)))?;

        let fat_token = FatAccessToken::from(token_response);
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
