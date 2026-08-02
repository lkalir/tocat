//! stream.rs: what an endpoint hands back once it is open.
//!
//! Every endpoint in this module resolves to one of two shapes. The async path
//! produces a [`Connection`]: an [`EndpointStream`] plus the optional
//! [`PathGuard`] whose drop cleans up a path the endpoint created. The
//! synchronous path produces [`SyncHalves`], used when both ends are
//! blocking-backed and the async wrappers would only add copies.
//!
//! [`EndpointStream::Datagram`] is deliberately not `AsyncRead`/`AsyncWrite`:
//! those traits describe a byte stream and have nowhere to put a message
//! boundary. Everything downstream of an endpoint therefore has to handle both
//! shapes, which is what [`ReadHalf`] and [`WriteHalf`] make explicit.

use tokio::{
    io::{AsyncRead, AsyncWrite, empty, sink},
    net::{TcpStream, UnixStream},
};

use crate::endpoint::sys::PathGuard;

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
}

pub enum EndpointStream {
    Duplex(Box<dyn AsyncStream>),
    Split(BoxRead, BoxWrite),
    /// A datagram socket, not `AsyncRead`/`AsyncWrite` since those traits have
    /// no way to preserve message boundaries
    Datagram(DatagramSocket),
}

/// A connected datagram socket
#[derive(Clone)]
pub enum DatagramSocket {
    Udp(std::sync::Arc<tokio::net::UdpSocket>),
}

impl DatagramSocket {
    /// One datagram. A message longer than `buf` is **truncated**.
    pub async fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            DatagramSocket::Udp(socket) => socket.recv(buf).await,
        }
    }

    pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            DatagramSocket::Udp(socket) => socket.send(buf).await,
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
        }
    }

    pub fn into_connection_with_guard(self, guard: PathGuard) -> Connection {
        Connection {
            stream: self,
            guard: Some(guard),
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
        Self::Split(Box::new(r), Box::new(sink()))
    }

    pub fn write_only(w: impl AsyncWrite + Unpin + Send + 'static) -> Self {
        Self::Split(Box::new(empty()), Box::new(w))
    }

    pub fn datagram(socket: tokio::net::UdpSocket) -> Self {
        Self::Datagram(DatagramSocket::Udp(std::sync::Arc::new(socket)))
    }

    pub fn into_halves(self) -> (ReadHalf, WriteHalf) {
        match self {
            Self::Duplex(s) => {
                let (r, w) = tokio::io::split(s);
                (
                    ReadHalf::Stream(Box::new(r)),
                    WriteHalf::Stream(Box::new(w)),
                )
            }
            Self::Split(r, w) => (ReadHalf::Stream(r), WriteHalf::Stream(w)),
            Self::Datagram(socket) => (
                ReadHalf::Datagram(socket.clone()),
                WriteHalf::Datagram(socket),
            ),
        }
    }
}
