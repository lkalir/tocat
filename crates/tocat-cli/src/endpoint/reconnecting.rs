//! reconnecting.rs: a stream that reopens itself, under a pipeline that never
//! finds out.
//!
//! `reconnect=keep` is the mode where the stages outlive the connection. That
//! is only expressible below them: a chain and a pump hold halves of a stream
//! and copy until it ends, so the reopening has to happen inside the thing they
//! are holding rather than around it.
//!
//! [`EndpointStream::Duplex`] is already split with `tokio::io::split`, which
//! puts both halves behind one lock on one object. That is what makes this
//! tractable: the reader and the writer are views of the same
//! [`Reconnecting`], so whichever of them meets the failure does the redial and
//! the other simply finds a working stream when its turn comes. Two
//! independently reconnecting halves would open two connections.
//!
//! # What survives and what does not
//!
//! Stage state survives, which is the point. Bytes do not. Anything the kernel
//! accepted and had not delivered is gone, and a `write_all` interrupted
//! halfway has put an unknowable prefix on the wire. A reconnect is therefore a
//! hole in the stream, and a stage that needs boundaries across it will resync
//! on rubbish. That belongs in the guide next to this option, not only here.
//!
//! # Things that are easy to lose in a refactor
//!
//! End of stream is not a failure. `Ok(0)` from a read is the peer saying it is
//! finished and it passes straight through; only an error redials. Getting that
//! backwards makes the endpoint impossible to close.
//!
//! The dial future lives in this struct rather than in the caller's, so a
//! `poll_read` that is dropped mid-dial loses nothing: the next poll finds the
//! same future where it left it. That is what keeps `Source::next` cancel safe
//! with one of these underneath it.

use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::warn;

use crate::endpoint::{
    Connection, Direction, EndpointSpec, EndpointStream, PathGuard, stream::AsyncStream,
};

/// One open, with the endpoint's own attempt policy applied.
type Dial = BoxFuture<'static, anyhow::Result<Connection>>;

enum State {
    Connected(Box<dyn AsyncStream>),
    /// Between connections. Reads and writes are `Pending` here, which is the
    /// backpressure that keeps the other side of the relay from running ahead
    /// into a stream that does not exist.
    Dialing(Dial),
    /// Shut down by the pipeline, or given up on. Nothing redials from here.
    Finished,
}

pub struct Reconnecting {
    spec: Arc<EndpointSpec>,
    dir: Direction,
    buffer: usize,
    state: State,

    /// Held on behalf of the current connection and replaced with it.
    _guard: Option<PathGuard>,
    _keepalive: Option<Box<dyn Send>>,
}

impl Reconnecting {
    /// Wrap an already open connection.
    ///
    /// The first connection is opened by the caller, so a relay that cannot
    /// reach its peer at all still fails at startup rather than retreating into
    /// a reconnect loop before it has ever worked.
    pub(in crate::endpoint) fn new(
        spec: Arc<EndpointSpec>,
        dir: Direction,
        buffer: usize,
        first: Connection,
    ) -> anyhow::Result<Self> {
        let EndpointStream::Duplex(stream) = first.stream else {
            anyhow::bail!(
                "reconnect=keep needs an endpoint that opens a two-way stream, and {} does not",
                spec.name(),
            );
        };

        Ok(Self {
            spec,
            dir,
            buffer,
            state: State::Connected(stream),
            _guard: first.guard,
            _keepalive: first.keepalive,
        })
    }

    /// Take the new connection, or report why it cannot be used.
    fn install(&mut self, opened: Connection) -> io::Result<()> {
        let EndpointStream::Duplex(stream) = opened.stream else {
            return Err(io::Error::other(format!(
                "{} reopened as something other than a two-way stream",
                self.spec.name(),
            )));
        };

        self._guard = opened.guard;
        self._keepalive = opened.keepalive;
        self.state = State::Connected(stream);

        Ok(())
    }
}

/// Open again, with the attempt policy the endpoint was given.
///
/// `'static` because the future is parked in [`State::Dialing`] between polls,
/// which is why the spec is behind an `Arc` rather than borrowed.
fn dial(spec: Arc<EndpointSpec>, dir: Direction, buffer: usize) -> Dial {
    Box::pin(async move { spec.connect_inner(dir, buffer).await })
}

/// Whether an error means the connection is gone.
///
/// Everything except the two that mean "ask again": a redial is cheap and
/// bounded by the endpoint's attempt policy, while treating a real failure as
/// transient would spin.
fn lost(e: &io::Error) -> bool {
    !matches!(
        e.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

impl AsyncRead for Reconnecting {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            let next = match &mut this.state {
                // An ended stream stays ended: reporting end of stream is what
                // lets the pipeline run its own end-of-stream path.
                State::Finished => return Poll::Ready(Ok(())),

                State::Connected(stream) => match Pin::new(stream).poll_read(cx, buf) {
                    Poll::Ready(Err(e)) if lost(&e) => {
                        warn!(endpoint = %this.spec.name(), "read failed, reopening: {e}");
                        State::Dialing(dial(this.spec.clone(), this.dir, this.buffer))
                    }
                    other => return other,
                },

                State::Dialing(future) => match future.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(opened)) => {
                        this.install(opened)?;
                        continue;
                    }
                    Poll::Ready(Err(e)) => {
                        this.state = State::Finished;
                        return Poll::Ready(Err(io::Error::other(format!("{e:#}"))));
                    }
                },
            };

            this.state = next;
        }
    }
}

impl AsyncWrite for Reconnecting {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        loop {
            let next = match &mut this.state {
                State::Finished => {
                    return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
                }

                State::Connected(stream) => match Pin::new(stream).poll_write(cx, buf) {
                    Poll::Ready(Err(e)) if lost(&e) => {
                        warn!(endpoint = %this.spec.name(), "write failed, reopening: {e}");
                        State::Dialing(dial(this.spec.clone(), this.dir, this.buffer))
                    }
                    other => return other,
                },

                State::Dialing(future) => match future.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(opened)) => {
                        this.install(opened)?;
                        continue;
                    }
                    Poll::Ready(Err(e)) => {
                        this.state = State::Finished;
                        return Poll::Ready(Err(io::Error::other(format!("{e:#}"))));
                    }
                },
            };

            this.state = next;
        }
    }

    /// A flush that fails is not worth a reconnect: whatever it would have
    /// flushed belongs to a connection that no longer exists, and the write
    /// after it will do the reopening.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().state {
            State::Connected(stream) => Pin::new(stream).poll_flush(cx),
            State::Dialing(_) | State::Finished => Poll::Ready(Ok(())),
        }
    }

    /// End of stream from the pipeline, which is deliberate rather than a
    /// failure, so this is where reconnecting stops.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        let result = match &mut this.state {
            State::Connected(stream) => Pin::new(stream).poll_shutdown(cx),
            State::Dialing(_) | State::Finished => Poll::Ready(Ok(())),
        };

        if result.is_ready() {
            this.state = State::Finished;
        }

        result
    }
}
