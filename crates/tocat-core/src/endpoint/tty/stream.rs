//! `AsyncRead`/`AsyncWrite` over a terminal descriptor.
//!
//! A terminal is pollable, so it belongs on the reactor rather than on the
//! blocking pool `file:` would give it. The two directions share one
//! descriptor, which is fine here and is exactly what makes the same treatment
//! wrong for a seekable path: there is no file offset for them to fight over.
use std::{
    io,
    os::fd::{AsFd, OwnedFd},
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

pub struct Stream(pub(super) AsyncFd<OwnedFd>);

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut ready = std::task::ready!(self.0.poll_read_ready(cx))?;

            let read = ready.try_io(|inner| {
                rustix::io::read(inner.get_ref().as_fd(), buf.initialize_unfilled())
                    .map_err(io::Error::from)
            });

            match read {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                // Readiness was stale, so ask again.
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = std::task::ready!(self.0.poll_write_ready(cx))?;

            let written = ready.try_io(|inner| {
                rustix::io::write(inner.get_ref().as_fd(), buf).map_err(io::Error::from)
            });

            match written {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    /// Nothing is buffered on this side: a write is a `write`.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// A terminal has no half-close. Closing the descriptor is the only
    /// shutdown there is, and that happens when the connection drops.
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
