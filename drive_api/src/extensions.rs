use oauth2::AccessToken;
use serde::Serialize;
use url::Url;
pub trait UrlExt {
    fn with_query_pairs<T: Serialize + ?Sized>(self, query: &T) -> Self;
}

impl UrlExt for Url {
    /// This is inspired by the way that reqwest handles query pairs. It was suprisingly convenient.
    /// Note that it (apparently) does not support a single query pair. To add a single one, use
    /// `.with_query_pairs(&[("key", "val")])`.
    fn with_query_pairs<T: Serialize + ?Sized>(mut self, query: &T) -> Self {
        let mut pairs = self.query_pairs_mut();
        let serializer = serde_urlencoded::Serializer::new(&mut pairs);
        query.serialize(serializer).unwrap();
        drop(pairs);
        
        self
    }
}

pub trait RequestBuilderExt {
    fn bearer_auth(self, token: &AccessToken) -> Self;

    fn json_body<T: ?Sized + Serialize>(self, value: &T) -> anyhow::Result<http::Request<Vec<u8>>>;
}

impl RequestBuilderExt for http::request::Builder {
    fn bearer_auth(self, token: &AccessToken) -> Self {
        self.header(http::header::AUTHORIZATION, format!("Bearer {}", token.secret()))
    }

    fn json_body<T: ?Sized + Serialize>(
        mut self,
        value: &T,
    ) -> anyhow::Result<http::Request<Vec<u8>>> {
        self = self.header(http::header::CONTENT_TYPE, "application/json");
        Ok(self.body(serde_json::to_vec(value)?)?)
    }
}
