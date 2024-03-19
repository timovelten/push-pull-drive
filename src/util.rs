use futures::AsyncRead;
use http::Request;
use pin_project_lite::pin_project;
use tokio::sync::OwnedSemaphorePermit;

pin_project! {
    /// Utility type we use to keep the semaphore permit around
    /// as long as its inner type is not dropped.
    #[derive(Debug)]
    pub struct SemaphoreGuarded<T> {
        pub permit: OwnedSemaphorePermit,
        #[pin]
        pub inner: T,
    }
}

impl<T> std::ops::Deref for SemaphoreGuarded<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> AsyncRead for SemaphoreGuarded<T>
where
    T: AsyncRead,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().inner.as_mut().poll_read(cx, buf)
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
