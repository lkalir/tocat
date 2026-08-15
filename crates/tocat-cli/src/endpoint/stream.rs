//! stream.rs: what an endpoint hands back once it is open.
//!
//! Every endpoint in this module resolves to one of two shapes. The async path
//! produces a [`Connection`]: an [`EndpointStream`] plus the optional
//! [`PathGuard`] whose drop cleans up a path the endpoint created. The
//! synchronous path produces [`SyncHalves`], used when both ends are
//! blocking-backed and the async wrappers would only add copies.
//!
//! [`Connection::keepalive`] is the general form of what [`PathGuard`] does
//! for a path: something the connection created or borrowed, whose cleanup has
//! to wait until the transfer is over.
//!
//! [`EndpointStream::Datagram`] is deliberately not `AsyncRead`/`AsyncWrite`:
//! those traits describe a byte stream and have nowhere to put a message
//! boundary. Everything downstream of an endpoint therefore has to handle both
//! shapes, which is what [`ReadHalf`] and [`WriteHalf`] make explicit.

use std::sync::Arc;

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, UnixStream},
};

use crate::endpoint::{
    datagram::Session,
    sys::PathGuard,
    unix::{dgram::UnixDgramSocket, seqpacket::SeqpacketConn},
};

/// A synchronous endpoint half. See [`EndpointSpec::connect_sync`].
///
/// [`EndpointSpec::connect_sync`]: crate::endpoint::EndpointSpec::connect_sync
pub type SyncRead = Box<dyn std::io::Read + Send>;
pub type SyncWrite = Box<dyn std::io::Write + Send>;

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

/// The blocking halves of an endpoint. Either may be absent: a `file:` source
/// has nothing to write to, and pairing a reader with a missing writer means
/// that direction does not exist and should not be run at all.
#[derive(Default)]
pub struct SyncHalves {
    pub reader: Option<SyncRead>,
    pub writer: Option<SyncWrite>,
    /// Carried for the same reason [`Connection`] carries one: a `pipe:` opened
    /// with `unlink` removes its path when this is dropped, so the caller has
    /// to hold it for as long as the transfer runs.
    pub guard: Option<PathGuard>,
}

pub struct Connection {
    pub stream: EndpointStream,
    pub guard: Option<PathGuard>,
    /// Anything else the connection has to outlive, dropped with it.
    ///
    /// Two endpoints need this and neither needs the same thing, which is why
    /// it is opaque rather than a second typed field. `pty:` keeps its slave
    /// descriptor here, since without a holder the master reports the pair as
    /// hung up. `tty:` keeps the settings it found on the device, since those
    /// belong to the system and have to go back.
    pub keepalive: Option<Box<dyn Send>>,
}

impl Connection {
    /// Attach something whose drop has to wait for the connection to end.
    #[must_use]
    pub fn with_keepalive(mut self, keepalive: impl Send + 'static) -> Self {
        self.keepalive = Some(Box::new(keepalive));
        self
    }
}

pub enum EndpointStream {
    Duplex(Box<dyn AsyncStream>),
    Split(BoxRead, BoxWrite),
    /// Readable only, such as a `file:` or `pipe:` source. The other direction
    /// does not exist, which [`EndpointStream::into_halves`] reports as an
    /// absent half rather than as one that is already finished.
    ReadOnly(BoxRead),
    /// Writable only: the same endpoints, as a sink.
    WriteOnly(BoxWrite),
    /// A datagram socket, not `AsyncRead`/`AsyncWrite` since those traits have
    /// no way to preserve message boundaries
    Datagram(DatagramSocket),
}

/// A message endpoint, either a socket of its own or one peer's share of a
/// shared one.
#[derive(Clone)]
pub enum DatagramSocket {
    Udp(Arc<tokio::net::UdpSocket>),
    /// A `unix-dgram:` or an unforked `unix-dgram-listen:`. See
    /// [`UnixDgramSocket`].
    UnixDgram(Arc<UnixDgramSocket>),
    /// A connected `SOCK_SEQPACKET` socket: the one variant with an end of
    /// stream and a shutdown to send. See [`SeqpacketConn`].
    Seqpacket(Arc<SeqpacketConn>),
    /// One sender's session on a forked datagram listener. See [`Session`].
    Session(Arc<Session>),
}

impl DatagramSocket {
    /// One message, or `None` once the source has ended.
    ///
    /// A message longer than `buf` is **truncated**. Only two variants can
    /// end: a seqpacket socket, whose peer can shut down, and a session, whose
    /// receive loop can stop or whose path a stage can halt. A connectionless
    /// socket of our own has no close to observe and is always `Some`.
    pub async fn recv(&self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        match self {
            DatagramSocket::Udp(socket) => socket.recv(buf).await.map(Some),
            DatagramSocket::UnixDgram(socket) => socket.recv(buf).await.map(Some),
            DatagramSocket::Seqpacket(socket) => socket.recv(buf).await,
            DatagramSocket::Session(session) => session.recv(buf).await,
        }
    }

    pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            DatagramSocket::Udp(socket) => socket.send(buf).await,
            DatagramSocket::UnixDgram(socket) => socket.send(buf).await,
            DatagramSocket::Seqpacket(socket) => socket.send(buf).await,
            DatagramSocket::Session(session) => session.send(buf).await,
        }
    }

    /// End of stream on the writing side.
    ///
    /// Only a connected message socket has one to send: `SHUT_WR` is how
    /// seqpacket says nothing further is coming, and without it a peer that
    /// waits for our end of stream never hears one. The connectionless forms
    /// have nothing to close, which is the same reason they never see an end
    /// of stream either.
    pub fn finish(&self) {
        if let DatagramSocket::Seqpacket(socket) = self {
            socket.finish();
        }
    }
}

/// The reading end of an endpoint, stream or datagram
pub enum ReadHalf {
    Stream(BoxRead),
    Datagram(DatagramSocket),
}

/// The writing end of an endpoint, stream or datagram
pub enum WriteHalf {
    Stream(BoxWrite),
    Datagram(DatagramSocket),
}

impl EndpointStream {
    pub fn into_connection(self) -> Connection {
        Connection {
            stream: self,
            guard: None,
            keepalive: None,
        }
    }

    /// As [`into_connection`](Self::into_connection), for an endpoint that may
    /// have created a path.
    ///
    /// The guard is optional because whether there is one is a property of the
    /// address rather than of the scheme: a `pipe:` without `unlink` and a unix
    /// socket in the abstract namespace both created nothing to remove.
    pub fn into_connection_with_guard(self, guard: Option<PathGuard>) -> Connection {
        Connection {
            stream: self,
            guard,
            keepalive: None,
        }
    }

    pub fn tcp(s: TcpStream) -> Self {
        Self::Duplex(Box::new(s))
    }

    pub fn unix(s: UnixStream) -> Self {
        Self::Duplex(Box::new(s))
    }

    pub fn stdio() -> Self {
        Self::Split(Box::new(tokio::io::stdin()), Box::new(tokio::io::stdout()))
    }

    pub fn read_only(r: impl AsyncRead + Unpin + Send + 'static) -> Self {
        Self::ReadOnly(Box::new(r))
    }

    pub fn write_only(w: impl AsyncWrite + Unpin + Send + 'static) -> Self {
        Self::WriteOnly(Box::new(w))
    }

    pub fn datagram(socket: tokio::net::UdpSocket) -> Self {
        Self::Datagram(DatagramSocket::Udp(Arc::new(socket)))
    }

    /// A `SOCK_SEQPACKET` socket, which is a message endpoint rather than a
    /// byte stream: boundaries are the reason to have chosen it, and
    /// `AsyncRead`/`AsyncWrite` have nowhere to keep one.
    pub fn seqpacket(socket: tokio_seqpacket::UnixSeqpacket) -> Self {
        Self::Datagram(DatagramSocket::Seqpacket(Arc::new(SeqpacketConn::new(
            socket,
        ))))
    }

    pub fn unix_dgram(socket: UnixDgramSocket) -> Self {
        Self::Datagram(DatagramSocket::UnixDgram(Arc::new(socket)))
    }

    pub fn datagram_session(session: Session) -> Self {
        Self::Datagram(DatagramSocket::Session(Arc::new(session)))
    }

    /// The two halves, either of which may be absent.
    ///
    /// `None` is not the same as a half that is already at end of stream: it
    /// means that direction does not exist and must not be run.
    pub fn into_halves(self) -> (Option<ReadHalf>, Option<WriteHalf>) {
        match self {
            Self::Duplex(s) => {
                let (r, w) = tokio::io::split(s);
                (
                    Some(ReadHalf::Stream(Box::new(r))),
                    Some(WriteHalf::Stream(Box::new(w))),
                )
            }
            Self::Split(r, w) => (Some(ReadHalf::Stream(r)), Some(WriteHalf::Stream(w))),
            Self::ReadOnly(r) => (Some(ReadHalf::Stream(r)), None),
            Self::WriteOnly(w) => (None, Some(WriteHalf::Stream(w))),
            Self::Datagram(socket) => (
                Some(ReadHalf::Datagram(socket.clone())),
                Some(WriteHalf::Datagram(socket)),
            ),
        }
    }
}
