//! udp.rs: `udp:` and `udp-listen:`.
//!
//! Datagram endpoints, so both resolve to [`EndpointStream::Datagram`] rather
//! than to a byte stream: the pump has to see message boundaries.
//!
//! Neither form has an accept. `udp:` connects the socket so the kernel filters
//! to one peer. `udp-listen:` has two shapes. Without `fork` it peeks the first
//! datagram to learn who the peer is and connects to it, leaving the datagram
//! queued for the relay. With `fork` it keeps the socket unconnected and
//! demultiplexes by source address instead, so every sender gets its own
//! session, its own dialled peer and its own plugin instances.
//!
//! # Ending a session
//!
//! Nothing here ends one. A datagram source has no close to observe, so a
//! session runs until a stage stops it, which is what the `timeout` plugin is
//! for: it is already the thing that ends a path that has gone quiet, it is
//! already `datagram_safe`, and its halt is already the early end of stream
//! that cascades `on_eof` and closes the path down normally. Growing a second
//! idle timer here would have been the same feature with a different name and
//! a different set of bugs.
//!
//! What that costs the operator is one declaration, and it has to be on both
//! paths: `timeout:both,timeout=30s`. A forward-only halt leaves the reverse
//! pump reading a sink that may never close, and it is the session task ending
//! that releases the permit and the map entry. A sender that goes quiet and
//! comes back then gets a new session with fresh plugin state, exactly as a
//! reconnecting TCP client would.

use std::{collections::HashMap, net::SocketAddr, num::NonZeroUsize, sync::Arc};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::{
    net::UdpSocket,
    sync::{Mutex, mpsc},
};
use tracing::{debug, error, info, warn};

use crate::{
    buffer::Buffer,
    endpoint::{
        Connection, DEFAULT_HOST, DEFAULT_MAX_CONNECTIONS, DEFAULT_PORT, EndpointStream,
        parse::{Opt, ParseEndpointError, host_port},
    },
    shutdown::Shutdown,
};

/// Datagrams queued for one peer before further ones are dropped.
///
/// Dropping is the correct answer here rather than blocking: the receive loop
/// serves every peer, so making it wait on one slow session would stall all of
/// them, and a dropped datagram is a thing UDP already means.
const SESSION_DEPTH: usize = 64;

/// Sessions waiting for the relay's accept loop to pick them up.
const PENDING_SESSIONS: usize = 16;

#[derive(Debug, Deserialize, Serialize)]
pub struct Udp {
    pub addr: String,
    /// Local address to bind before connecting. Defaults to an ephemeral
    /// port on all interfaces
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UdpListen {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub fork: bool,
    #[serde(
        default,
        rename = "max-connections",
        alias = "max-conn",
        alias = "max-conns"
    )]
    pub max_connections: Option<NonZeroUsize>,
}

impl Udp {
    const SCHEME: &'static str = "udp";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        if body.is_empty() {
            return Err(ParseEndpointError::Empty);
        }

        let mut bind = None;
        let mut name = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "bind" => bind = Some(opt.string()?),
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            addr: body.to_owned(),
            bind,
            name,
        })
    }

    pub(super) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("udp://{}", self.addr))
    }

    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        // Resolve the peer first: the local socket has to be in the same address
        // family, so a v6 peer needs a v6 wildcard. This is the same mismatch that
        // makes a `localhost` listener unreachable, one layer down.
        let peer = tokio::net::lookup_host(&self.addr)
            .await
            .with_context(|| format!("resolving {}", self.addr))?
            .next()
            .with_context(|| format!("{} resolved to no address", self.addr))?;

        let socket = match &self.bind {
            Some(local) => UdpSocket::bind(local.as_str())
                .await
                .with_context(|| format!("binding {local}"))?,
            None => {
                let wildcard: std::net::SocketAddr = if peer.is_ipv4() {
                    (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
                } else {
                    (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
                };

                UdpSocket::bind(wildcard)
                    .await
                    .with_context(|| format!("binding {wildcard}"))?
            }
        };

        socket
            .connect(peer)
            .await
            .with_context(|| format!("connecting to {peer}"))?;

        Ok(EndpointStream::datagram(socket).into_connection())
    }
}

impl UdpListen {
    const SCHEME: &'static str = "udp-listen";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let (host, port) = host_port(body)?;

        let mut name = None;
        let mut fork = false;
        let mut max_connections = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "fork" => fork = opt.flag()?,
                "maxconnections" | "maxconn" | "maxconns" => max_connections = Some(opt.count()?),
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            host,
            port,
            name,
            fork,
            max_connections,
        })
    }

    pub(super) fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }

        let (host, port) = self.addr();
        format!("udp://{host}:{port}")
    }

    /// Where to bind, with the defaults filled in.
    pub fn addr(&self) -> (&str, u16) {
        (
            self.host.as_deref().unwrap_or(DEFAULT_HOST),
            self.port.unwrap_or(DEFAULT_PORT),
        )
    }

    pub fn max_connections(&self) -> NonZeroUsize {
        self.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS)
    }

    /// Bind without peering, for the caller that owns the receive loop.
    pub async fn bind(&self) -> std::io::Result<UdpSocket> {
        UdpSocket::bind(self.addr()).await
    }

    /// Bind and start demultiplexing datagrams by source address.
    ///
    /// The socket stays unconnected, which is the whole trick: a connected UDP
    /// socket has the kernel filter to one peer, and this needs to hear from
    /// all of them.
    pub async fn demux(&self, buffer: usize, shutdown: Shutdown) -> anyhow::Result<UdpDemux> {
        let socket = Arc::new(self.bind().await?);

        info!(local = %socket.local_addr()?, "listening for datagrams");

        let (peers, incoming) = mpsc::channel(PENDING_SESSIONS);

        tokio::spawn(demultiplex(
            socket,
            peers,
            self.max_connections(),
            buffer,
            shutdown,
        ));

        Ok(UdpDemux { incoming })
    }

    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let socket = self.bind().await?;
        info!(local = %socket.local_addr()?, "listening for datagrams");

        // Peek rather than receive: the first datagram tells us who the peer
        // is, and it has to stay queued so the relay does not eat it. There is
        // nothing to accept: the first sender simply becomes the peer for the
        // rest of the run.
        let mut probe = [0u8; 1];
        let (_, peer) = socket.peek_from(&mut probe).await?;
        socket.connect(peer).await?;
        info!("Peered with {peer}");

        Ok(EndpointStream::datagram(socket).into_connection())
    }
}

/// The accept side of a forked `udp-listen:`.
///
/// Sessions are produced by the receive loop rather than by a call here, so a
/// peer that keeps sending is served whether or not the relay is currently
/// asking for a new one.
pub struct UdpDemux {
    incoming: mpsc::Receiver<(EndpointStream, String)>,
}

impl UdpDemux {
    /// The next sender to be heard from, as an endpoint to relay.
    ///
    /// Shaped like `TcpListener::accept` so the relay's loop does not have to
    /// care which transport it is serving. An error here means the receive
    /// loop stopped, which on a signal the caller has already noticed.
    ///
    /// Cancel safe: the only future dropped is an `mpsc::Receiver::recv`.
    pub async fn accept(&mut self) -> std::io::Result<(EndpointStream, String)> {
        self.incoming.recv().await.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the udp receive loop stopped",
            )
        })
    }
}

/// One peer's share of a forked `udp-listen:` socket.
///
/// Reads come from the queue the receive loop fills for this address; writes
/// go straight back out of the shared socket with `send_to`, since there is no
/// per-peer socket to connect.
///
/// Nothing here ends a session on its own. The queue closes when the receive
/// loop stops, which happens on shutdown, and otherwise a session lasts as
/// long as its task does. Ending one that has gone quiet is the `timeout`
/// plugin's job.
pub struct UdpSession {
    peer: SocketAddr,
    socket: Arc<UdpSocket>,
    /// Locked rather than owned because [`DatagramSocket`] is cloned into a
    /// read half and a write half. Only the read half ever takes it, so the
    /// lock is uncontended.
    ///
    /// [`DatagramSocket`]: crate::endpoint::DatagramSocket
    queue: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl UdpSession {
    fn new(peer: SocketAddr, socket: Arc<UdpSocket>, queue: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            peer,
            socket,
            queue: Mutex::new(queue),
        }
    }

    /// One datagram, or `None` once this session has ended.
    ///
    /// Cancel safe: `Receiver::recv` loses nothing when dropped, which matters
    /// because the pump drops this future every time a ticking stage wakes it.
    pub(super) async fn recv(&self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        let Some(datagram) = self.queue.lock().await.recv().await else {
            // The receive loop stopped, so nothing further can arrive.
            return Ok(None);
        };

        // Truncating rather than erroring, which is what a receive into an
        // undersized buffer does one layer down.
        let n = datagram.len().min(buf.len());
        buf[..n].copy_from_slice(&datagram[..n]);

        Ok(Some(n))
    }

    pub(super) async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.socket.send_to(buf, self.peer).await
    }
}

/// Receive from every sender, hand each datagram to that sender's session, and
/// announce a session the first time an address is heard from.
///
/// One task for the whole endpoint. It must never block on anything a single
/// session controls, or one slow peer stalls the rest.
async fn demultiplex(
    socket: Arc<UdpSocket>,
    peers: mpsc::Sender<(EndpointStream, String)>,
    max: NonZeroUsize,
    buffer: usize,
    mut shutdown: Shutdown,
) {
    let mut sessions: HashMap<SocketAddr, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut buf = Buffer::new(buffer);
    let mut dropped = 0u64;

    loop {
        let received = tokio::select! {
            biased;
            received = socket.recv_from(&mut buf) => received,
            () = shutdown.recv() => break,
        };

        let (n, peer) = match received {
            Ok(received) => received,
            Err(e) => {
                error!(error = %e, "receiving datagram failed, stopping");
                break;
            }
        };

        // The send happens inside the lookup so the map is not still borrowed
        // when the closed case removes the entry. The copy is the price of
        // crossing into the session's task, the same one a segment boundary
        // pays in the pump.
        let delivered = sessions
            .get(&peer)
            .map(|session| session.try_send(buf[..n].to_vec()));

        match delivered {
            Some(Ok(())) => continue,
            Some(Err(mpsc::error::TrySendError::Full(_))) => {
                drop_datagram(&mut dropped, peer, "session is not keeping up");
                continue;
            }
            // Idle, or a stage stopped the path. This datagram opens a new
            // session for the same address, with fresh plugin state.
            Some(Err(mpsc::error::TrySendError::Closed(_))) => {
                sessions.remove(&peer);
            }
            None => {}
        }

        if sessions.len() >= max.get() {
            // Reaped here rather than on a timer or on every new peer: a
            // finished session holds nothing open, so it is only worth the
            // sweep when it is occupying a slot somebody else wants.
            sessions.retain(|_, session| !session.is_closed());

            if sessions.len() >= max.get() {
                // A session ends when its task does, and nothing here ends one
                // that has gone quiet, so a ceiling full of live sessions
                // stays full. Naming the fix in the message because this is
                // the only moment an operator finds out they needed it.
                drop_datagram(
                    &mut dropped,
                    peer,
                    "ceiling reached; add a timeout stage to reclaim quiet sessions",
                );
                continue;
            }
        }

        let (inbox, queue) = mpsc::channel(SESSION_DEPTH);
        let session = UdpSession::new(peer, socket.clone(), queue);

        match peers.try_send((EndpointStream::datagram_session(session), peer.to_string())) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                drop_datagram(&mut dropped, peer, "relay is not accepting quickly enough");
                continue;
            }
            // The relay stopped accepting: there is nobody left to serve.
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }

        // The datagram that opened the session is its first message, not a
        // probe. Unlike the unforked path there is no peek, so nothing else
        // would deliver it.
        let _ = inbox.try_send(buf[..n].to_vec());
        sessions.insert(peer, inbox);

        info!(%peer, sessions = sessions.len(), "new sender");
    }

    debug!(dropped, "udp receive loop finished");
}

/// Report a datagram nobody could take.
///
/// Loud once and quiet after: dropping is normal under load and a warning per
/// datagram would bury everything else, but an operator who never sees the
/// first one has no way to know it is happening.
fn drop_datagram(dropped: &mut u64, peer: SocketAddr, reason: &'static str) {
    *dropped += 1;

    if *dropped == 1 {
        warn!(%peer, reason, "dropping datagram; further drops are logged at debug");
    } else {
        debug!(%peer, reason, "dropping datagram");
    }
}
