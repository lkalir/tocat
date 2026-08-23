//! endpoint: what the relay connects to.
//!
//! An endpoint is one end of the byte path: a socket to dial or listen on, a
//! file, a child process, or stdio. [`EndpointSpec`] is the parsed form and can
//! come either from the compact CLI grammar (`tcp-listen:8080,fork`) or from a
//! TOML table, which is why it carries both `FromStr` and `Deserialize`.
//!
//! [`Direction`] here means *role*, which side of the relay this endpoint is,
//! not which way bytes are moving. It matters for `file:` and the other
//! half-duplex endpoints, where the same spec opens for reading as a source and
//! for writing as a sink.
//!
//! # Layout
//!
//! This file holds only what is common to every endpoint: the spec enum, the
//! dispatch, and the shared vocabulary. One transport per module beneath it,
//! each owning its own fields, its parse, its label and its connect, so that
//! adding a scheme is a new file plus one variant and one line in the scheme
//! table in `parse`.
//!
//! A `pty:` endpoint keeps its slave descriptor open for the life of the run.
//! Without a holder the master reports the pair as hung up, and the relay ends
//! before a peer has had a chance to open the device.
//!
//! * `stream` is what an open endpoint hands back.
//! * `parse` is the CLI grammar shared by every scheme.
//! * `sys` is the system plumbing more than one transport needs.
//! * `datagram` is the receive loop a forked connectionless listener needs,
//!   shared by `udp-listen:` and `unix-dgram-listen:`.
//!
//! `unix` is the one module with children of its own, because the three
//! `AF_UNIX` socket types share their address rules and differ only in what
//! they carry.
//!
//! # Things that are easy to lose in a refactor
//!
//! A `unix-listen:` endpoint, or a `pipe:` opened with `unlink`, hands back a
//! [`PathGuard`] that removes the path on drop, so the guard has to outlive the
//! connection. And a FIFO opened without `hold` blocks until a peer appears,
//! which is worth the warning it emits rather than looking like a hang.
//!
//! `pipe:` (alias `fifo:`) defaults to holding the FIFO open read-write, so it
//! outlives its producers. `file:` pointed at a FIFO is the one-shot version
//! and keeps that behaviour.
//!
//! Payload dumping used to be an endpoint option (`dump=`, `format=`). It is
//! now the `tee` plugin, which can sit anywhere in the pipeline rather than
//! only at the ends.

mod chan;
mod datagram;
mod exec;
mod file;
mod layer;
mod parse;
mod pipe;
mod pty;
mod reconnecting;
mod retry;
mod sockopt;
mod stdio;
mod stream;
mod sys;
mod tcp;
mod tty;
mod udp;
mod unix;

use std::{num::NonZeroUsize, sync::Arc};

use serde::{Deserialize, Serialize};

pub use self::{
    chan::{Chan, Channel, DEFAULT_CAPACITY, Message, register, unregister},
    datagram::Demux,
    exec::{Exec, System},
    file::File,
    layer::{ClientAuth, LayerSpec, Tls, Verify, Ws},
    parse::ParseEndpointError,
    pipe::Pipe,
    pty::{Pty, PtyExec},
    reconnecting::Reconnecting,
    retry::{Attempts, Continuity, Forever, Retry},
    sockopt::{Keepalive, SocketOptions},
    stdio::Stdio,
    stream::{
        BoxRead, BoxWrite, Connection, DatagramSocket, EndpointStream, MessageSocket, ReadHalf,
        SyncHalves, SyncRead, SyncWrite, WriteHalf,
    },
    sys::{PathGuard, size_if_pipe},
    tcp::{Tcp, TcpListen},
    tty::Tty,
    udp::{Udp, UdpListen},
    unix::{
        Unix, UnixListen,
        dgram::{UnixDgram, UnixDgramListen},
        seqpacket::{UnixSeqpacket, UnixSeqpacketListen},
    },
};

/// Where a listening endpoint binds when the spec names no host. Loopback, not
/// the wildcard: exposing a relay to the network should be something you asked
/// for.
const DEFAULT_HOST: &str = "127.0.0.1";

/// The port a listening endpoint binds when the spec names none.
const DEFAULT_PORT: u16 = 8000;

/// How many connections a listener will serve at once under `fork`.
const DEFAULT_MAX_CONNECTIONS: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// An endpoint as it appears in the config file, before it is resolved.
///
/// The compact string form and the table form are both valid TOML for the same
/// field, so the config accepts either and [`into_spec`](Self::into_spec)
/// collapses them.
// The two variants are wildly different sizes, a string against a spec of a few
// hundred bytes, and that is fine: there are exactly two of these per run and
// they live only until the config is resolved. Boxing would trade a stack cost
// nobody pays for a heap allocation and an indirection. Revisit if a spec ever
// ends up in a collection, which is what the lint is actually for.
#[expect(
    clippy::large_enum_variant,
    reason = "There are two of these per run, so one being huge is no big deal"
)]
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Endpoint {
    Raw(String),
    Spec(EndpointSpec),
}

impl Endpoint {
    pub fn into_spec(self) -> Result<EndpointSpec, ParseEndpointError> {
        match self {
            Endpoint::Raw(raw) => raw.parse(),
            Endpoint::Spec(spec) => Ok(spec),
        }
    }
}

/// Which side of the relay an endpoint is, not which way bytes move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Source,
    Sink,
}

/// An endpoint: a transport, and later the handshakes stacked over it.
///
/// A newtype for now. It exists because reconnection belongs to the endpoint
/// rather than to the transport: [`Reconnecting`] holds one of these and
/// reopens it, and once layers land a redial has to redo the handshake as well
/// as the connect. Leaving the retry loop on the transport would put it below
/// the thing it has to rebuild.
///
/// # Things that are easy to lose in a refactor
///
/// The predicates here answer for the **top of the stack**, not the transport.
/// A layer decides what the endpoint carries, and an endpoint that answers for
/// its transport instead reports the wrong shape, which is how a datagram relay
/// silently becomes a stream one.
///
/// `flatten` keeps the transport's table where it was, so a config file that
/// names no layers is spelled exactly as before.
#[derive(Debug, Deserialize, Serialize)]
pub struct EndpointSpec {
    #[serde(flatten)]
    pub transport: Transport,

    /// Bottom first. `wss:` will be a TCP transport under `[Tls, Ws]`, and the
    /// wrapping runs in this order, each layer taking what the one below it
    /// produced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<LayerSpec>,
}

impl From<Transport> for EndpointSpec {
    fn from(transport: Transport) -> Self {
        Self {
            transport,
            layers: Vec::new(),
        }
    }
}

impl EndpointSpec {
    pub fn is_listen(&self) -> bool {
        self.transport.is_listen()
    }

    pub fn is_datagram(&self) -> bool {
        self.layers
            .iter()
            .fold(self.transport.is_datagram(), |below, layer| {
                layer.is_datagram(below)
            })
    }

    pub fn is_fork(&self) -> bool {
        self.transport.is_fork()
    }

    pub fn name(&self) -> String {
        self.transport.name()
    }

    pub fn max_connections(&self) -> NonZeroUsize {
        self.transport.max_connections()
    }

    /// A layered endpoint is never blocking-backed: a handshake needs the async
    /// path whatever is underneath it, so the synchronous copy is not an
    /// option.
    pub fn is_blocking_backed(&self) -> bool {
        self.layers.is_empty() && self.transport.is_blocking_backed()
    }

    /// Refuse a stack that cannot work, before anything is opened.
    ///
    /// Called from the parser and again from `Relay::new`, because a spec can
    /// also arrive straight out of a config file without passing the parser.
    pub fn check(&self) -> anyhow::Result<()> {
        let mut carries = self.transport.is_datagram();

        for layer in &self.layers {
            layer.check(carries, self.transport.is_listen())?;
            carries = layer.is_datagram(carries);
        }

        if !self.layers.is_empty() && self.reconnect().keeps() {
            anyhow::bail!(
                "reconnect=keep is not supported under a layer yet: reopening would have to redo \
                 the handshake underneath a pipeline that is mid-stream, and the hole that leaves \
                 lands inside a record rather than between bytes. Use reconnect=restart",
            );
        }

        Ok(())
    }

    /// Put each layer around an open connection, bottom first.
    ///
    /// A listening endpoint layers what it accepted, so the side is decided by
    /// the transport rather than passed in: `tls-listen:` accepts a handshake
    /// where `tls:` starts one.
    pub(crate) async fn wrap(&self, stream: EndpointStream) -> anyhow::Result<EndpointStream> {
        if self.layers.is_empty() {
            return Ok(stream);
        }

        let host = self.transport.host().unwrap_or_default().to_owned();
        let mut stream = stream;

        for layer in &self.layers {
            stream = if self.transport.is_listen() {
                layer.wrap_server(stream).await?
            } else {
                layer.wrap_client(stream, &host).await?
            };
        }

        Ok(stream)
    }

    pub fn connect_sync(&self, dir: Direction, buffer: usize) -> anyhow::Result<SyncHalves> {
        self.transport.connect_sync(dir, buffer)
    }

    /// What this endpoint asked to happen when an established connection
    /// fails. [`Continuity::None`] for anything that cannot be reopened.
    pub fn reconnect(&self) -> Continuity {
        self.transport
            .retry()
            .map_or(Continuity::None, |r| r.reconnect)
    }

    /// How long this endpoint asked to wait before reopening. Zero for one
    /// that cannot be reopened, which never reaches the restart loop anyway.
    pub fn reconnect_delay(&self) -> std::time::Duration {
        self.transport
            .retry()
            .map_or(std::time::Duration::ZERO, |r| r.reconnect_delay())
    }

    /// Open this endpoint, blocking until it has a peer where that applies.
    ///
    /// `dir` decides which way the half-duplex endpoints open. `buffer` is the
    /// relay's copy buffer, passed down so that pipe-backed descriptors can be
    /// sized to match it.
    ///
    /// A scheme that can be reopened is tried as many times as it asked to be.
    /// The loop is here rather than in each scheme so that a scheme only has to
    /// hold a [`Retry`] and parse its options.
    ///
    /// Under `reconnect=keep` the connection comes back wrapped in a
    /// [`Reconnecting`], which reopens it underneath the pipeline. That takes
    /// an `Arc<Self>`, so this is the one entry point that needs the spec to be
    /// shared rather than borrowed.
    pub async fn connect_shared(
        self: &Arc<Self>,
        dir: Direction,
        buffer: usize,
    ) -> anyhow::Result<Connection> {
        let opened = self.connect_inner(dir, buffer).await?;

        if !self.reconnect().keeps() {
            return Ok(opened);
        }

        let reconnecting = Reconnecting::new(self.clone(), dir, buffer, opened)?;

        Ok(EndpointStream::Duplex(Box::new(reconnecting)).into_connection())
    }

    /// Open with the attempt policy applied and nothing wrapped around it.
    ///
    /// The reconnecting stream calls this rather than [`Self::connect_shared`],
    /// which is what stops a redial from wrapping itself again.
    /// The handshake happens after the attempt policy has finished, not inside
    /// it: `retry` counts connections that could not be opened, and a peer that
    /// answered and then failed the handshake is a different problem from one
    /// that is not there yet.
    pub(in crate::endpoint) async fn connect_inner(
        &self,
        dir: Direction,
        buffer: usize,
    ) -> anyhow::Result<Connection> {
        let opened = self.connect_transport(dir, buffer).await?;
        let Connection {
            stream,
            guard,
            keepalive,
        } = opened;

        Ok(Connection {
            stream: self.wrap(stream).await?,
            guard,
            keepalive,
        })
    }

    async fn connect_transport(&self, dir: Direction, buffer: usize) -> anyhow::Result<Connection> {
        let Some(retry) = self.transport.retry() else {
            return self.transport.connect_once(dir, buffer).await;
        };

        let mut attempt = 1;

        loop {
            let opened = match retry.deadline() {
                Some(deadline) => {
                    tokio::time::timeout(deadline, self.transport.connect_once(dir, buffer))
                        .await
                        .unwrap_or_else(|_| {
                            Err(anyhow::anyhow!("connect timed out after {deadline:?}"))
                        })
                }
                None => self.transport.connect_once(dir, buffer).await,
            };

            match opened {
                Ok(connection) => return Ok(connection),
                Err(e) if retry.again(attempt) => {
                    let wait = retry.wait();

                    // A relay waiting for a server that has not started yet
                    // should say so rather than looking hung.
                    tracing::warn!(
                        endpoint = self.transport.name(),
                        attempt,
                        "connect failed, retrying in {wait:?}: {e:#}",
                    );

                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// One transport, parsed.
///
/// The variants are newtypes over the per-transport structs so that the fields
/// live with the code that uses them. The serde representation is unchanged by
/// that: an internally tagged enum flattens a newtype variant's struct into the
/// same table it would have produced inline, so `{ type = "tcp", addr = "…" }`
/// still deserialises.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Transport {
    #[serde(
        alias = "TCP",
        alias = "tcp-connect",
        alias = "TCP-CONNECT",
        alias = "connect",
        alias = "CONNECT",
        alias = "tcpconnect",
        alias = "TCPCONNECT"
    )]
    Tcp(Tcp),
    #[serde(
        alias = "TCP-LISTEN",
        alias = "tcplisten",
        alias = "TCPLISTEN",
        alias = "listen",
        alias = "LISTEN"
    )]
    TcpListen(TcpListen),
    Stdio(Stdio),
    #[serde(
        alias = "UNIX",
        alias = "unix-connect",
        alias = "UNIX-CONNECT",
        alias = "uds",
        alias = "UDS",
        alias = "uds-connect",
        alias = "UDS-CONNECT",
        alias = "udsconnect",
        alias = "UDSCONNECT"
    )]
    Unix(Unix),
    #[serde(
        alias = "UNIX-LISTEN",
        alias = "unixlisten",
        alias = "UNIXLISTEN",
        alias = "uds-listen",
        alias = "UDS-LISTEN",
        alias = "udslisten",
        alias = "UDSLISTEN"
    )]
    UnixListen(UnixListen),
    #[serde(
        alias = "UNIX-SEQPACKET",
        alias = "unix-seqpkt",
        alias = "UNIX-SEQPKT",
        alias = "uds-seqpacket",
        alias = "UDS-SEQPACKET",
        alias = "seqpacket",
        alias = "SEQPACKET"
    )]
    UnixSeqpacket(UnixSeqpacket),
    #[serde(
        alias = "UNIX-SEQPACKET-LISTEN",
        alias = "unix-seqpkt-listen",
        alias = "UNIX-SEQPKT-LISTEN",
        alias = "uds-seqpacket-listen",
        alias = "UDS-SEQPACKET-LISTEN",
        alias = "seqpacket-listen",
        alias = "SEQPACKET-LISTEN"
    )]
    UnixSeqpacketListen(UnixSeqpacketListen),
    #[serde(
        alias = "UNIX-DGRAM",
        alias = "unix-datagram",
        alias = "UNIX-DATAGRAM",
        alias = "uds-dgram",
        alias = "UDS-DGRAM"
    )]
    UnixDgram(UnixDgram),
    #[serde(
        alias = "UNIX-DGRAM-LISTEN",
        alias = "unix-datagram-listen",
        alias = "UNIX-DATAGRAM-LISTEN",
        alias = "uds-dgram-listen",
        alias = "UDS-DGRAM-LISTEN"
    )]
    UnixDgramListen(UnixDgramListen),
    #[serde(alias = "fifo", alias = "FIFO", alias = "PIPE")]
    Pipe(Pipe),
    #[serde(alias = "open", alias = "FILE", alias = "OPEN")]
    File(File),
    Exec(Exec),
    System(System),
    #[serde(alias = "PTY")]
    Pty(Pty),
    #[serde(alias = "PTY-EXEC", alias = "ptyexec", alias = "PTYEXEC")]
    PtyExec(PtyExec),
    #[serde(alias = "TTY", alias = "serial", alias = "SERIAL")]
    Tty(Tty),
    #[serde(
        alias = "UDP",
        alias = "udp-connect",
        alias = "UDP-CONNECT",
        alias = "udpconnect",
        alias = "UDPCONNECT"
    )]
    Udp(Udp),
    #[serde(alias = "UDP-LISTEN", alias = "udplisten", alias = "UDPLISTEN")]
    UdpListen(UdpListen),
    #[serde(alias = "CHAN", alias = "channel", alias = "CHANNEL")]
    Chan(Chan),
}

impl Transport {
    pub fn is_listen(&self) -> bool {
        matches!(
            self,
            Self::TcpListen(_)
                | Self::UnixListen(_)
                | Self::UnixSeqpacketListen(_)
                | Self::UnixDgramListen(_)
                | Self::UdpListen(_)
                | Self::Chan(_)
        )
    }

    /// True where the endpoint carries messages rather than a byte stream.
    ///
    /// Seqpacket is connection oriented and still belongs here: what decides
    /// this is whether boundaries are data, not whether there is an accept.
    pub fn is_datagram(&self) -> bool {
        matches!(
            self,
            Self::Udp(_)
                | Self::UdpListen(_)
                | Self::UnixSeqpacket(_)
                | Self::UnixSeqpacketListen(_)
                | Self::UnixDgram(_)
                | Self::UnixDgramListen(_)
        )
    }

    pub fn is_fork(&self) -> bool {
        match self {
            Self::TcpListen(e) => e.fork,
            Self::UnixListen(e) => e.fork,
            Self::UnixSeqpacketListen(e) => e.fork,
            Self::UnixDgramListen(e) => e.fork,
            Self::UdpListen(e) => e.fork,
            _ => false,
        }
    }

    /// How this endpoint is named in logs and in plugin instance names.
    pub fn name(&self) -> String {
        match self {
            Self::Tcp(e) => e.label(),
            Self::TcpListen(e) => e.label(),
            Self::Stdio(e) => e.label(),
            Self::Unix(e) => e.label(),
            Self::UnixListen(e) => e.label(),
            Self::UnixSeqpacket(e) => e.label(),
            Self::UnixSeqpacketListen(e) => e.label(),
            Self::UnixDgram(e) => e.label(),
            Self::UnixDgramListen(e) => e.label(),
            Self::Pipe(e) => e.label(),
            Self::File(e) => e.label(),
            Self::Exec(e) => e.label(),
            Self::System(e) => e.label(),
            Self::Pty(e) => e.label(),
            Self::PtyExec(e) => e.label(),
            Self::Tty(e) => e.label(),
            Self::Udp(e) => e.label(),
            Self::UdpListen(e) => e.label(),
            Self::Chan(e) => e.label(),
        }
    }

    pub fn max_connections(&self) -> NonZeroUsize {
        match self {
            Self::TcpListen(TcpListen {
                max_connections, ..
            })
            | Self::UnixListen(UnixListen {
                max_connections, ..
            })
            | Self::UnixSeqpacketListen(UnixSeqpacketListen {
                max_connections, ..
            })
            | Self::UnixDgramListen(UnixDgramListen {
                max_connections, ..
            })
            | Self::UdpListen(UdpListen {
                max_connections, ..
            }) => max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS),
            _ => DEFAULT_MAX_CONNECTIONS,
        }
    }

    /// True when tokio has no real async implementation for this endpoint and
    /// serves it from the blocking pool.
    ///
    /// Those wrappers read into their own buffer and then copy into yours, so a
    /// relay between two of them pays two userspace copies of the payload that
    /// a plain `read`/`write` loop does not. When nothing else needs the async
    /// machinery, [`connect_sync`](Self::connect_sync) skips it.
    pub fn is_blocking_backed(&self) -> bool {
        matches!(self, Self::File(_) | Self::Stdio(_) | Self::Pipe(_))
    }

    /// Open this endpoint as plain blocking handles.
    ///
    /// Only valid for endpoints where
    /// [`is_blocking_backed`](Self::is_blocking_backed) holds; sockets are
    /// genuinely async and gain nothing here.
    pub fn connect_sync(&self, dir: Direction, buffer: usize) -> anyhow::Result<SyncHalves> {
        match self {
            Self::Stdio(e) => Ok(e.connect_sync(buffer)),
            Self::Pipe(e) => e.connect_sync(dir),
            Self::File(e) => e.connect_sync(dir),
            other => anyhow::bail!("{} has no synchronous form", other.name()),
        }
    }

    /// The attempt policy for this endpoint, if it is one that can be reopened.
    ///
    /// A listener is not: its answer to a peer that went away is to keep
    /// accepting, and a bind that fails is a configuration error. A `file:` is
    /// not either, because reopening one raises a question about the offset
    /// that nobody has answered yet.
    fn retry(&self) -> Option<&Retry> {
        match self {
            Self::Tcp(e) => Some(&e.retry),
            Self::Unix(e) => Some(&e.retry),
            Self::UnixSeqpacket(e) => Some(&e.retry),
            Self::Exec(e) => Some(&e.retry),
            Self::System(e) => Some(&e.retry),
            _ => None,
        }
    }

    /// The host this transport was pointed at, for a layer that needs a name to
    /// check a certificate against.
    ///
    /// `None` for a transport with no host of its own, which is every transport
    /// a layer cannot sit on anyway.
    pub(in crate::endpoint) fn host(&self) -> Option<&str> {
        match self {
            Self::Tcp(e) => Some(
                e.addr
                    .rsplit_once(':')
                    .map_or(e.addr.as_str(), |(host, _)| host),
            ),
            Self::TcpListen(e) => e.host.as_deref(),
            _ => None,
        }
    }
    /// One open, which is what every scheme implements.
    pub(in crate::endpoint) async fn connect_once(
        &self,
        dir: Direction,
        buffer: usize,
    ) -> anyhow::Result<Connection> {
        match self {
            Self::Tcp(e) => e.connect().await,
            Self::TcpListen(e) => e.connect().await,
            Self::Stdio(e) => e.connect(buffer),
            Self::Unix(e) => e.connect().await,
            Self::UnixListen(e) => e.connect().await,
            Self::UnixSeqpacket(e) => e.connect().await,
            Self::UnixSeqpacketListen(e) => e.connect().await,
            Self::UnixDgram(e) => e.connect().await,
            // The only endpoint that needs the copy buffer to *open*: it
            // learns its peer by receiving a message, and that message has to
            // land somewhere before it can be handed on.
            Self::UnixDgramListen(e) => e.connect(buffer).await,
            Self::Pipe(e) => e.connect(dir).await,
            Self::File(e) => e.connect(dir).await,
            Self::Exec(e) => e.connect(buffer).await,
            Self::System(e) => e.connect(buffer).await,
            Self::Pty(e) => e.connect().await,
            Self::PtyExec(e) => e.connect().await,
            Self::Tty(e) => e.connect().await,
            Self::Udp(e) => e.connect().await,
            Self::UdpListen(e) => e.connect().await,
            Self::Chan(e) => e.connect().await,
        }
    }
}
