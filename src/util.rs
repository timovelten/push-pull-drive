use std::fmt::Write;

use http::Request;
use time::OffsetDateTime;

use crate::PathInfo;

pub fn u8_slice_to_hex_string(slice: &[u8]) -> String {
    let mut ret = String::with_capacity(slice.len() * 2);
    for b in slice {
        write!(ret, "{:x}", b).unwrap();
    }

    ret
}

pub fn datetime_to_filetime(time: OffsetDateTime) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(time.unix_timestamp(), time.nanosecond())
}

pub fn clone_request<T: Clone>(req: &Request<T>) -> Request<T> {
    if req.extensions().len() != 0 {
        panic!("cloning requests with extensions is unsupported")
    }

    let method = req.method().clone();
    let uri = req.uri().clone();
    let version = req.version().clone();
    let headers = req.headers().clone();
    let body = req.body().clone();

    let mut ret = Request::builder()
        .method(method)
        .uri(uri)
        .version(version)
        .body(body)
        .unwrap();

    *ret.headers_mut() = headers;
    ret
}

#[allow(dead_code)]
pub mod assert {
    pub const fn send<T: Send>() {}
    pub const fn sync<T: Send>() {}
    pub const fn clone<T: Clone>() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_hex_lowercase() {
        assert_eq!(
            u8_slice_to_hex_string(&[0xde, 0xad, 0xca, 0xfe, 0x1a, 0x2b, 0x3c, 0x4d]),
            "deadcafe1a2b3c4d"
        )
    }
}

pub trait PathListExt {
    fn sort_by_relative_name(&mut self);
    fn get_by_relative_name(&self, relative_name: &str) -> Option<&PathInfo>;
    fn contains_relative_name(&self, relative_name: &str) -> bool {
        self.get_by_relative_name(relative_name).is_some()
    }
}

impl PathListExt for Vec<PathInfo> {
    fn sort_by_relative_name(&mut self) {
        self.sort_by(|x, y| x.relative_name().cmp(y.relative_name()))
    }

    fn get_by_relative_name(&self, relative_name: &str) -> Option<&PathInfo> {
        self.binary_search_by(|x| x.relative_name().cmp(relative_name))
            .ok()
            .map(|i| &self[i])
    }
}
