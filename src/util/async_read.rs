//! We use this to track the progress of an asyncread!
use futures::AsyncRead;
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{ready, Poll};

pin_project! {
    pub struct InspectAsyncReader<R, F> {
        f: F,
        #[pin]
        reader: R,
    }
}

impl<R, F> InspectAsyncReader<R, F>
where
    F: FnMut(&[u8]),
{
    pub fn new(reader: R, f: F) -> Self {
        Self { reader, f }
    }
}

impl<R, F> AsyncRead for InspectAsyncReader<R, F>
where
    R: AsyncRead,
    F: FnMut(&[u8]),
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut this = self.project();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let ret = ready!(this.reader.as_mut().poll_read(cx, buf));
        if let Ok(ret) = ret {
            (this.f)(&buf[0..ret]);
        }

        Poll::Ready(ret)
    }
}
