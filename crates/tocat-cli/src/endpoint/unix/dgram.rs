//! dgram.rs: `unix-dgram:` and `unix-dgram-listen:`.
//!
//! Connectionless message sockets, so both resolve to
//! [`EndpointStream::Datagram`] and neither ever reaches end of stream on its
//! own: ending a quiet path is the `timeout` plugin's job here exactly as it is
//! on UDP.
//!
//! # Why this file talks to std
//!
//! tokio's datagram entry points take a path and reject the NUL byte an
//! abstract name starts with, and it has no equivalent of `connect_addr`. Both
//! gaps are covered by taking the address from [`SocketPath::addr`] and going
//! through std, which is what [`bind_datagram`] and [`connect_addr`] are for.
//! The round trip through `into_std` happens before any I/O and costs a
//! deregister and a register; connecting a datagram socket is a local
//! operation with no handshake, so there is nothing in flight to lose.
//!
//! # Identity
//!
//! Unlike UDP, `AF_UNIX` has no autobind: a sender that never bound has no
//! address, so it cannot be replied to and two of them cannot be told apart.
//! That single fact shapes both schemes.
//!
//! `unix-dgram:` therefore binds a local address by default rather than
//! leaving the socket anonymous. Without one the peer's replies have nowhere
//! to go and the reverse direction waits for something the kernel cannot
//! deliver, which looks like a hang rather than a misconfiguration. `bind=`
//! names the address instead; the default is a path in the temporary directory
//! that the connection's guard removes.
//!
//! `unix-dgram-listen:` learns its peer from the first message rather than
//! peeking at it, because tokio's datagram socket has no peek: the message is
//! taken off the queue and carried into the session as its first, which is
//! what [`UnixDgramSocket::pending`] holds. Under `fork` the same rule decides
//! who can be served at all, and the receive loop in `datagram` drops messages
//! from senders it cannot name.

use std::{
    num::NonZeroUsize,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::net::UnixDatagram;
use tracing::{info, warn};

use crate::{
    endpoint::{
        Connection, DEFAULT_MAX_CONNECTIONS, EndpointStream,
        datagram::{self, Demux},
        parse::{Opt, ParseEndpointError},
        sys::Mode,
        unix::{SocketPath, apply_mode, unlink_stale},
    },
    shutdown::Shutdown,
};

#[derive(Debug, Deserialize, Serialize)]
pub struct UnixDgram {
    pub path: SocketPath,
    /// Local address to bind before connecting. Defaults to a path in the
    /// temporary directory, removed when the connection ends.
    #[serde(default)]
    pub bind: Option<SocketPath>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub unlink: bool,
    #[serde(default)]
    pub mode: Option<Mode>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UnixDgramListen {
    pub path: SocketPath,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub fork: bool,
    #[serde(default, rename = "max-connections")]
    pub max_connections: Option<NonZeroUsize>,
    #[serde(default)]
    pub unlink: bool,
    #[serde(default)]
    pub mode: Option<Mode>,
}

impl UnixDgram {
    const SCHEME: &'static str = "unix-dgram";

    pub(in crate::endpoint) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut bind = None;
        let mut name = None;
        let mut unlink = false;
        let mut mode = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "bind" => bind = Some(SocketPath::from_spec(opt.text()?)),
                "mode" => mode = Some(opt.mode()?),
                "name" => name = Some(opt.string()?),
                "unlink" => unlink = opt.flag()?,
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        // The generated local address is fresh every run, so there is never a
        // stale one to clear and `unlink` can only have meant the peer, which
        // is not ours to remove.
        if unlink && bind.is_none() {
            return Err(ParseEndpointError::Conflict {
                scheme: Self::SCHEME,
                reason: "unlink clears a stale local address, and there is no bind= to clear",
            });
        }

        Ok(Self {
            path: SocketPath::from_spec(body),
            bind,
            name,
            unlink,
            mode,
        })
    }

    pub(in crate::endpoint) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("unix-dgram://{}", self.path))
    }

    pub(in crate::endpoint) async fn connect(&self) -> anyhow::Result<Connection> {
        let local = match &self.bind {
            Some(bind) => bind.clone(),
            None => temp_socket_path(),
        };

        if self.unlink {
            unlink_stale(&local, async {
                UnixDatagram::unbound().and_then(|probe| probe.connect(local.as_path()))
            })
            .await?;
        }

        let socket = bind_datagram(&local)?;

        // A generated address lives in a directory everyone can write to, so
        // it is closed to everyone but its owner unless the spec said
        // otherwise. An address the operator named is left to the operator.
        let mode = match self.bind {
            Some(_) => self.mode,
            None => self.mode.or(Some(Mode::PRIVATE)),
        };

        apply_mode(&local, mode)?;

        let socket = connect_datagram(socket, &self.path)?;
        info!(local = %local, peer = %self.path, "sending datagrams");

        Ok(EndpointStream::unix_dgram(UnixDgramSocket::dialled(socket))
            .into_connection_with_guard(local.guard()))
    }
}

impl UnixDgramListen {
    const SCHEME: &'static str = "unix-dgram-listen";

    pub(in crate::endpoint) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut name = None;
        let mut fork = false;
        let mut max_connections = None;
        let mut unlink = false;
        let mut mode = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "fork" => fork = opt.flag()?,
                "maxconnections" | "maxconn" => {
                    max_connections = Some(opt.count()?);
                }
                "mode" => mode = Some(opt.mode()?),
                "name" => name = Some(opt.string()?),
                "unlink" => unlink = opt.flag()?,
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            path: SocketPath::from_spec(body),
            name,
            fork,
            max_connections,
            unlink,
            mode,
        })
    }

    pub(in crate::endpoint) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("unix-dgram://{}", self.path))
    }

    pub fn max_connections(&self) -> NonZeroUsize {
        self.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS)
    }

    /// Bind without peering, for the caller that owns the receive loop.
    ///
    /// The caller is responsible for the [`PathGuard`]: this only creates the
    /// socket.
    ///
    /// [`PathGuard`]: crate::endpoint::PathGuard
    pub async fn bind(&self) -> anyhow::Result<UnixDatagram> {
        if self.unlink {
            unlink_stale(&self.path, async {
                UnixDatagram::unbound().and_then(|probe| probe.connect(self.path.as_path()))
            })
            .await?;
        }

        let socket = bind_datagram(&self.path)?;
        apply_mode(&self.path, self.mode)?;

        Ok(socket)
    }

    /// Bind and start demultiplexing messages by sender.
    pub async fn demux(&self, buffer: usize, shutdown: Shutdown) -> anyhow::Result<Demux> {
        let socket = Arc::new(self.bind().await?);
        info!(path = %self.path, "listening for datagrams");

        Ok(datagram::demux(
            datagram::Socket::Unix(socket),
            self.max_connections(),
            buffer,
            shutdown,
        ))
    }

    /// Bind and take the first sender as the peer.
    pub(in crate::endpoint) async fn connect(&self, buffer: usize) -> anyhow::Result<Connection> {
        let socket = self.bind().await?;
        let guard = self.path.guard();
        info!(path = %self.path, "listening for datagrams");

        // Received rather than peeked, which is the one place this differs
        // from `udp-listen:`: tokio's datagram socket has no peek, so the
        // message that identifies the sender comes off the queue and is
        // carried into the session instead of being left for the relay.
        let mut first = vec![0u8; buffer];
        let (n, peer) = socket.recv_from(&mut first).await?;
        first.truncate(n);

        let connected = !peer.is_unnamed();

        let socket = if connected {
            info!(peer = ?peer, "peered");
            connect_addr(socket, &peer.into()).context("peering with the first sender")?
        } else {
            // Nothing to connect to and nothing to reply to. Receiving still
            // works, from anyone, which is what the syslog-shaped case wants.
            warn!(
                "the first sender has no address of its own, so this path can receive but not \
                 send; a peer that expects replies has to bind before it sends",
            );

            socket
        };

        Ok(
            EndpointStream::unix_dgram(UnixDgramSocket::listening(socket, first, connected))
                .into_connection_with_guard(guard),
        )
    }
}

/// A datagram socket serving one peer.
///
/// Two endpoints share it. `unix-dgram:` has its peer from the spec and is
/// connected before any message moves. An unforked `unix-dgram-listen:` has to
/// receive one message to find out who its peer is, so it arrives here holding
/// that message.
pub struct UnixDgramSocket {
    socket: UnixDatagram,
    /// The message that identified the peer, waiting to be handed over as the
    /// session's first.
    ///
    /// Locked rather than owned because the read and the write half share this
    /// through an `Arc`. Only the reader ever takes it, and only once, so the
    /// lock is uncontended and gone after the first message.
    pending: Mutex<Option<Vec<u8>>>,
    /// False when the peer turned out to be unnamed, which makes this a
    /// receive-only endpoint.
    connected: bool,
}

impl UnixDgramSocket {
    pub(in crate::endpoint) fn dialled(socket: UnixDatagram) -> Self {
        Self {
            socket,
            pending: Mutex::new(None),
            connected: true,
        }
    }

    /// `connected` is false when the first sender had no address, which is the
    /// one case where a bound socket cannot answer its peer.
    pub(in crate::endpoint) fn listening(
        socket: UnixDatagram,
        first: Vec<u8>,
        connected: bool,
    ) -> Self {
        Self {
            socket,
            pending: Mutex::new(Some(first)),
            connected,
        }
    }

    /// One message.
    ///
    /// Cancel safe: the future dropped is a `poll_recv`, and the pending
    /// message is taken and delivered without awaiting anything, so nothing is
    /// lost when the pump drops this for a tick.
    pub(in crate::endpoint) async fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let pending = self
            .pending
            .lock()
            .expect("the pending message lock is never held across a panic")
            .take();

        if let Some(message) = pending {
            // Truncating rather than erroring, which is what a receive into an
            // undersized buffer does one layer down.
            let n = message.len().min(buf.len());
            buf[..n].copy_from_slice(&message[..n]);

            return Ok(n);
        }

        self.socket.recv(buf).await
    }

    pub(in crate::endpoint) async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.connected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the peer is unnamed, so there is no address to send to",
            ));
        }

        self.socket.send(buf).await
    }
}

/// A local address for a socket only this run will use.
///
/// Three parts, each covering what the others cannot: the pid separates
/// concurrent runs, the counter separates endpoints within one run, and the
/// clock separates this run from a path a killed one left behind.
fn temp_socket_path() -> SocketPath {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();

    SocketPath::from_path(std::env::temp_dir().join(format!(
        "tocat-{}-{since_epoch}-{seq}.sock",
        std::process::id()
    )))
}

/// Bind a datagram socket, abstract names included.
///
/// See the module header: the address is built by std because tokio's
/// `UnixDatagram::bind` takes a path and an abstract name is not one.
fn bind_datagram(path: &SocketPath) -> anyhow::Result<UnixDatagram> {
    path.supported()?;

    let addr = path
        .addr()
        .with_context(|| format!("{path} is not a usable socket address"))?;

    let socket = std::os::unix::net::UnixDatagram::bind_addr(&addr)
        .with_context(|| format!("binding {path}"))?;

    socket
        .set_nonblocking(true)
        .with_context(|| format!("setting {path} non-blocking"))?;

    UnixDatagram::from_std(socket).with_context(|| format!("registering {path}"))
}

/// Point a bound socket at the peer a spec named.
fn connect_datagram(socket: UnixDatagram, peer: &SocketPath) -> anyhow::Result<UnixDatagram> {
    peer.supported()?;

    let addr = peer
        .addr()
        .with_context(|| format!("{peer} is not a usable socket address"))?;

    connect_addr(socket, &addr).with_context(|| format!("connecting to {peer}"))
}

/// Point a bound socket at a peer address.
///
/// Connecting is what makes the kernel filter incoming messages to one sender
/// and gives `send` somewhere to go, exactly as on `udp-listen:`. It goes
/// through std because tokio has no `connect_addr`, and an address is the only
/// form that can name an abstract peer or one learned from a receive.
fn connect_addr(
    socket: UnixDatagram,
    addr: &std::os::unix::net::SocketAddr,
) -> std::io::Result<UnixDatagram> {
    let socket = socket.into_std()?;
    socket.connect_addr(addr)?;

    UnixDatagram::from_std(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointSpec;

    fn dial(s: &str) -> UnixDgram {
        match s.parse::<EndpointSpec>().expect("parses") {
            EndpointSpec::UnixDgram(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn listen(s: &str) -> UnixDgramListen {
        match s.parse::<EndpointSpec>().expect("parses") {
            EndpointSpec::UnixDgramListen(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn the_scheme_answers_to_its_spellings() {
        for spec in [
            "unix-dgram:/dev/log",
            "unix-datagram:/dev/log",
            "uds-dgram:/dev/log",
        ] {
            assert_eq!(dial(spec).path, SocketPath::from_spec("/dev/log"));
        }

        assert!(listen("unix-dgram-listen:/tmp/tocat.sock,fork").fork);
    }

    /// The local address is what makes replies possible, so it has to be
    /// nameable and has to have a default.
    #[test]
    fn the_local_address_is_optional_and_explicit_when_given() {
        assert_eq!(dial("unix-dgram:/dev/log").bind, None);
        assert_eq!(
            dial("unix-dgram:/dev/log,bind=@tocat").bind,
            Some(SocketPath::from_spec("@tocat"))
        );
    }

    /// Nothing to unlink without a local address, and the peer is not ours to
    /// remove.
    #[test]
    fn unlink_without_a_local_address_is_a_contradiction() {
        assert!(matches!(
            "unix-dgram:/dev/log,unlink"
                .parse::<EndpointSpec>()
                .expect_err("rejected"),
            ParseEndpointError::Conflict { .. }
        ));

        assert!(dial("unix-dgram:/dev/log,bind=/tmp/reply.sock,unlink").unlink);
    }

    #[test]
    fn the_listening_options_match_the_other_listening_schemes() {
        let e = listen("unix-dgram-listen:/tmp/tocat.sock,fork,unlink,mode=660,max-conn=4");

        assert!(e.fork);
        assert!(e.unlink);
        assert_eq!(e.max_connections, NonZeroUsize::new(4));
        assert_eq!(e.mode, Some("660".parse().expect("valid mode")));
    }

    /// A generated address is fresh, so a temporary path is never reused by
    /// two endpoints in the same run.
    #[test]
    fn generated_addresses_are_distinct() {
        assert_ne!(temp_socket_path(), temp_socket_path());
        assert!(!temp_socket_path().is_abstract());
    }
}
