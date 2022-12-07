use std::{task::Poll, pin::Pin};

use pin_project::pin_project;
use futures::AsyncRead;

pub trait ProgressReporter: Send {
    fn report(&self, bytes: usize);
    fn finish(&self);
}

#[pin_project]
pub struct AsyncReadWithProgress<R, P> {
    reporter: P,
    #[pin]
    inner: R,
    cursor: usize,
}

impl <R: AsyncRead, P: ProgressReporter> AsyncReadWithProgress<R, P> {
    pub fn new(reader: R, reporter: P) -> Self {
        Self {
            reporter,
            inner: reader,
            cursor: 0,
        }
    }
}

impl <R: AsyncRead, P: ProgressReporter> AsyncRead for AsyncReadWithProgress<R, P> {
    fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
        if buf.len() == 0 {
            return Poll::Ready(Ok(0));
        }

        let this = self.project();

        match this.inner.poll_read(cx, buf) {
            // By convention, returning 0 here means either EOF or that the buffer
            // has length 0. The case where the buffer has length zero was handled above,
            // so this means EOF.
            Poll::Ready(Ok(0)) => {
                this.reporter.finish();
                Poll::Ready(Ok(0))
            }
            Poll::Ready(Ok(num_bytes_read)) => {
                // Note that we do not report the "new" position, but rather
                // the previous position. We do this in the assumption, that whoever reads this
                // data has certainly already processed the data they received previously (in our case:
                // successfully uploaded this data)
                let old_cursor = *this.cursor;
                *this.cursor +=  num_bytes_read;
                this.reporter.report(old_cursor);

                Poll::Ready(Ok(num_bytes_read))
            },
            other => other
        }
    }
}