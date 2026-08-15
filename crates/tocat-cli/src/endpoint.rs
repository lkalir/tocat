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

mod exec;
mod file;
mod parse;
mod pipe;
mod pty;
mod stdio;
mod stream;
mod sys;
mod tcp;
mod tty;
mod udp;
mod unix;

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

pub use self::{
    exec::{Exec, System},
    file::File,
    parse::ParseEndpointError,
    pipe::Pipe,
    pty::{Pty, PtyExec},
    stdio::Stdio,
    stream::{
        BoxRead, BoxWrite, Connection, DatagramSocket, EndpointStream, ReadHalf, SyncHalves,
        SyncRead, SyncWrite, WriteHalf,
    },
    sys::{PathGuard, size_if_pipe},
    tcp::{Tcp, TcpListen},
    tty::Tty,
    udp::{Udp, UdpDemux, UdpListen},
    unix::{Unix, UnixListen},
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

/// One endpoint, parsed.
///
/// The variants are newtypes over the per-transport structs so that the fields
/// live with the code that uses them. The serde representation is unchanged by
/// that: an internally tagged enum flattens a newtype variant's struct into the
/// same table it would have produced inline, so `{ type = "tcp", addr = "…" }`
/// still deserialises.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum EndpointSpec {
    #[serde(
        alias = "TCP",
        alias = "tcp-connect",
        alias = "connect",
        alias = "TCP-CONNECT",
        alias = "CONNECT"
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
    #[serde(alias = "UNIX", alias = "unix-connect", alias = "UNIX-CONNECT")]
    Unix(Unix),
    #[serde(alias = "UNIX-LISTEN", alias = "unixlisten", alias = "UNIXLISTEN")]
    UnixListen(UnixListen),
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
    #[serde(alias = "UDP", alias = "udp-connect", alias = "UDP-CONNECT")]
    Udp(Udp),
    #[serde(alias = "UDP-LISTEN", alias = "udplisten", alias = "UDPLISTEN")]
    UdpListen(UdpListen),
}

impl EndpointSpec {
    pub fn is_listen(&self) -> bool {
        matches!(
            self,
            Self::TcpListen(_) | Self::UnixListen(_) | Self::UdpListen(_)
        )
    }

    pub fn is_datagram(&self) -> bool {
        matches!(self, Self::Udp(_) | Self::UdpListen(_))
    }

    pub fn is_fork(&self) -> bool {
        match self {
            Self::TcpListen(e) => e.fork,
            Self::UnixListen(e) => e.fork,
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
            Self::Pipe(e) => e.label(),
            Self::File(e) => e.label(),
            Self::Exec(e) => e.label(),
            Self::System(e) => e.label(),
            Self::Pty(e) => e.label(),
            Self::PtyExec(e) => e.label(),
            Self::Tty(e) => e.label(),
            Self::Udp(e) => e.label(),
            Self::UdpListen(e) => e.label(),
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

    /// Open this endpoint, blocking until it has a peer where that applies.
    ///
    /// `dir` decides which way the half-duplex endpoints open. `buffer` is the
    /// relay's copy buffer, passed down so that pipe-backed descriptors can be
    /// sized to match it.
    pub async fn connect(&self, dir: Direction, buffer: usize) -> anyhow::Result<Connection> {
        match self {
            Self::Tcp(e) => e.connect().await,
            Self::TcpListen(e) => e.connect().await,
            Self::Stdio(e) => e.connect(buffer),
            Self::Unix(e) => e.connect().await,
            Self::UnixListen(e) => e.connect().await,
            Self::Pipe(e) => e.connect(dir).await,
            Self::File(e) => e.connect(dir).await,
            Self::Exec(e) => e.connect(buffer).await,
            Self::System(e) => e.connect(buffer).await,
            Self::Pty(e) => e.connect().await,
            Self::PtyExec(e) => e.connect().await,
            Self::Tty(e) => e.connect().await,
            Self::Udp(e) => e.connect().await,
            Self::UdpListen(e) => e.connect().await,
        }
    }
}
