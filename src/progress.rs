use std::{task::Poll, pin::Pin, sync::atomic::{AtomicUsize, Ordering}};

use futures::AsyncRead;

pub trait ProgressReporter: Send {
    fn report(&self, bytes: usize);
    fn finish(&self);
}

pub struct AsyncReadWithProgress<R, P> {
    reporter: P,
    inner: R,
    /// This is a dirty hack so that we do not have to worry about borrowing this struct mutably.
    cursor: AtomicUsize,
}

impl <R: AsyncRead, P: ProgressReporter> AsyncReadWithProgress<R, P> {
    pub fn new(reader: R, reporter: P) -> Self {
        Self {
            reporter,
            inner: reader,
            cursor: AtomicUsize::new(0),
        }
    }

    // SAFTEY: Refer to the pin_utils documentation. This struct does not implement drop, does
    // not implement Unpin and is not repr(packed).
    pin_utils::unsafe_pinned!(inner: R);

    fn update(&self, num_bytes_read: usize) {
        // Note that we do not report the "new" position, but rather
        // the previous position. We do this in the assumption, that whoever reads this
        // data has certainly already processed the data they received previously (in our case:
        // successfully uploaded this data)
        let old_cursor = self.cursor.fetch_add(num_bytes_read, Ordering::SeqCst);
        self.reporter.report(old_cursor);
    }

    fn finish(&self) {
        self.reporter.finish();
    }
}

impl <R: AsyncRead, P: ProgressReporter> AsyncRead for AsyncReadWithProgress<R, P> {
    fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
        if buf.len() == 0 {
            return Poll::Ready(Ok(0));
        }

        match self.as_mut().inner().poll_read(cx, buf) {
            // By convention, returning 0 here means either EOF or that the buffer
            // has length 0. The case where the buffer has length zero was handled above,
            // so this means EOF.
            Poll::Ready(Ok(0)) => {
                self.finish();
                Poll::Ready(Ok(0))
            }
            Poll::Ready(Ok(num_bytes_read)) => {
                self.update(num_bytes_read);
                Poll::Ready(Ok(num_bytes_read))
            },
            other => other
        }
    }
}