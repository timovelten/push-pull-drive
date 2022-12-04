use serde::Serialize;
use url::Url;
pub trait UrlExt {
    fn with_query_pairs<T: Serialize + ?Sized>(self, query: &T) -> Self;
}

impl UrlExt for Url {
    /// This is inspired by the way that reqwest handles query pairs. It was suprisingly convenient.
    fn with_query_pairs<T: Serialize + ?Sized>(mut self, query: &T) -> Self {
        let mut pairs = self.query_pairs_mut();
        let serializer = serde_urlencoded::Serializer::new(&mut pairs);
        
        if let Err(err) = query.serialize(serializer) {
            todo!("handle serialization error {:?}", err);
        }

        drop(pairs);

        self
    }
}

pub trait RequestBuilderExt {
    fn bearer_auth(self, token: &str) -> Self;

    fn json_body<T: ?Sized + Serialize>(self, value: &T) -> crate::Result<http::Request<Vec<u8>>>;
}

impl RequestBuilderExt for http::request::Builder {
    fn bearer_auth(self, token: &str) -> Self {
        self.header(http::header::AUTHORIZATION, format!("Bearer {}", token))
    }

    fn json_body<T: ?Sized + Serialize>(mut self, value: &T) -> crate::Result<http::Request<Vec<u8>>> {
        self = self.header(http::header::CONTENT_TYPE, "application/json");
        Ok(self.body(serde_json::to_vec(value)?)?)
    }


}

pub trait ResponseExt : Sized {
    fn json_error_for_status(self) -> crate::Result<Self>;
}

impl ResponseExt for http::Response<isahc::Body> {
    fn json_error_for_status(self) -> crate::Result<Self> {
        if self.status().is_client_error() || self.status().is_server_error() {
            Err(crate::Error::HttpStatusError { status: self.status().as_u16(), resp: String::new() })
        } else {
            Ok(self)
        }
    }
}

impl ResponseExt for http::Response<isahc::AsyncBody> {
    fn json_error_for_status(self) -> crate::Result<Self> {
        if self.status().is_client_error() || self.status().is_server_error() {
            Err(crate::Error::HttpStatusError { status: self.status().as_u16(), resp: String::new() })
        } else {
            Ok(self)
        }
    }
}