use http::Request;
use tokio::sync::OwnedSemaphorePermit;

/// Utility type we use to keep the semaphore permit around
/// as long as its inner type is not dropped.
#[derive(Debug)]
pub struct SemaphoreGuarded<T> {
    pub permit: OwnedSemaphorePermit,
    pub inner: T,
}

impl<T> std::ops::Deref for SemaphoreGuarded<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> std::ops::DerefMut for SemaphoreGuarded<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub fn clone_request<T: Clone>(req: &Request<T>) -> Request<T> {
    if !req.extensions().is_empty() {
        panic!("cloning requests with extensions is unsupported")
    }

    let method = req.method().clone();
    let uri = req.uri().clone();
    let version = req.version();
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

/// Returns the greatest index i such that a[..i] == b[..i] is a prefix of b.
pub fn find_common_prefix(a: &str, b: &str) -> usize {
    let mut b_chars = b.chars();
    for (i, x) in a.char_indices() {
        match b_chars.next() {
            Some(y) if x == y => continue,
            _ => return i,
        }
    }

    a.len()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_common_prefix() {
        assert_eq!(find_common_prefix("abcdefgh", "abc"), 3);
        assert_eq!(find_common_prefix("abcdefg", "bcd"), 0);
        assert_eq!(find_common_prefix("äü😀äallo", "äü😀"), "äü😀".len());
    }
}
