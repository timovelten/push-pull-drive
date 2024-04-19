mod async_read;
mod shell;

use std::fmt::Display;

pub use async_read::InspectAsyncReader;
pub use shell::*;

use http::Request;
use tokio::sync::OwnedSemaphorePermit;

pub struct WithoutNewlines<'a>(pub &'a str);

impl Display for WithoutNewlines<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in self.0.lines() {
            write!(f, "{}", line)?;
        }
        Ok(())
    }
}

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

pub fn human_readable_bytes(bs: u64) -> (f64, &'static str) {
    static UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let i = if bs == 0 {
        0
    } else {
        (bs.ilog2() as usize / 10).min(UNITS.len() - 1)
    };
    let magnitude = 1 << (10 * i);

    (bs as f64 / magnitude as f64, UNITS[i])
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

pub fn hex_must_decode(s: impl AsRef<[u8]>) -> Vec<u8> {
    hex::decode(s).expect("hex is valid")
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

    #[test]
    fn test_human_readable_bytes() {
        assert_eq!(human_readable_bytes(42), (42f64, "B"));
        assert_eq!(human_readable_bytes(1024), (1f64, "KiB"));
        assert_eq!(human_readable_bytes(5 * 1024 * 1024), (5f64, "MiB"));
        assert_eq!(
            human_readable_bytes(1024 * 1024 * 1024 + 42),
            (
                (1024.0 * 1024.0 * 1024.0 + 42.0) / (1024.0 * 1024.0 * 1024.0),
                "GiB"
            )
        );
    }

    #[test]
    fn test_without_newlines() {
        use std::fmt::Write;

        let x = "A text\n with at least\n three lines";
        let mut out = String::new();

        write!(out, "{}", WithoutNewlines(x)).unwrap();

        assert_eq!(out, "A text with at least three lines");
    }
}
