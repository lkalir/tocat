//! datagram.rs: one socket, many senders.
//!
//! A connectionless listener has no accept: a sender is discovered by
//! receiving from it. `udp-listen:` and `unix-dgram-listen:` both want the same
//! thing under `fork`, so the receive loop lives here once and each scheme
//! supplies the socket. One task owns the socket, a map takes each message to
//! that sender's session, and a sender not in the map becomes a new session
//! with its own dialled peer and its own chain instances, because stage state
//! is per path and per connection.
//!
//! The alternative, and what socat does, is a fresh socket per peer connected
//! to that address, leaving the kernel to route by most specific match. That is
//! one descriptor per peer, it leans on delivery rules that differ across
//! platforms, and messages arriving between the receive and the connect land on
//! the wrong socket anyway.
//!
//! # A session needs a sender with an address
//!
//! The map is keyed by the sender's address, and replies go back to it, so a
//! message from a sender that has none can neither open a session nor be
//! answered. UDP always satisfies this: a socket that never bound is given an
//! ephemeral port on its first send, so every datagram carries a usable source.
//! `AF_UNIX` has no autobind, so an unbound sender arrives anonymous, two of
//! them are indistinguishable, and messages from them are dropped with the
//! same accounting as any other drop. A sender in the abstract namespace is
//! dropped for a narrower reason: tokio's `send_to` takes a path, so there is
//! no way to answer one without reaching past it to the descriptor.
//!
//! [`Socket`] and [`Peer`] are enums rather than a trait with generic sessions
//! for the reason `pump` gives about `Upstream`: two implementations do not
//! earn a type parameter that every other type in the module would have to
//! carry, and a match against a syscall costs nothing.
//!
//! # Ending a session
//!
//! Nothing here ends one. A datagram source has no close to observe, so a
//! session runs until a stage stops it, which is what the `timeout` plugin is
//! for: it is already the thing that ends a path that has gone quiet, it is
//! already boundary preserving, and its halt is already the early end of stream
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

use std::{collections::HashMap, num::NonZeroUsize, path::PathBuf, sync::Arc};

use tokio::sync::{Mutex, mpsc};
use tracing::{debug, error, info, warn};

use crate::{buffer::Buffer, endpoint::EndpointStream, shutdown::Shutdown};

/// Messages queued for one peer before further ones are dropped.
///
/// Dropping is the correct answer here rather than blocking: the receive loop
/// serves every peer, so making it wait on one slow session would stall all of
/// them, and a dropped datagram is a thing the transport already means.
const SESSION_DEPTH: usize = 64;

/// Sessions waiting for the relay's accept loop to pick them up.
const PENDING_SESSIONS: usize = 16;

/// A shared datagram socket, whichever transport it belongs to.
#[derive(Clone)]
pub enum Socket {
    Udp(Arc<tokio::net::UdpSocket>),
    Unix(Arc<tokio::net::UnixDatagram>),
}

/// A sender this socket can both recognise and answer.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Peer {
    Inet(std::net::SocketAddr),
    Path(PathBuf),
}

impl Socket {
    /// One message and whoever sent it, or `None` for a sender with no address
    /// this socket could reply to.
    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, Option<Peer>)> {
        match self {
            Socket::Udp(socket) => {
                let (n, peer) = socket.recv_from(buf).await?;
                Ok((n, Some(Peer::Inet(peer))))
            }
            Socket::Unix(socket) => {
                let (n, peer) = socket.recv_from(buf).await?;
                Ok((n, peer.as_pathname().map(|p| Peer::Path(p.to_path_buf()))))
            }
        }
    }

    async fn send_to(&self, buf: &[u8], peer: &Peer) -> std::io::Result<usize> {
        match (self, peer) {
            (Socket::Udp(socket), Peer::Inet(peer)) => socket.send_to(buf, *peer).await,
            (Socket::Unix(socket), Peer::Path(peer)) => socket.send_to(buf, peer).await,
            // A peer only ever comes from this socket's own receive, so the
            // pairing is fixed when the session is built. Reporting it beats
            // an assertion on the send path.
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the peer address does not belong to this socket",
            )),
        }
    }
}

impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Peer::Inet(peer) => write!(f, "{peer}"),
            Peer::Path(peer) => write!(f, "{}", peer.display()),
        }
    }
}

/// Bind is the caller's; this owns the receive loop from here on.
pub(super) fn demux(socket: Socket, max: NonZeroUsize, buffer: usize, shutdown: Shutdown) -> Demux {
    let (peers, incoming) = mpsc::channel(PENDING_SESSIONS);

    tokio::spawn(demultiplex(socket, peers, max, buffer, shutdown));

    Demux { incoming }
}

/// The accept side of a forked datagram listener.
///
/// Sessions are produced by the receive loop rather than by a call here, so a
/// peer that keeps sending is served whether or not the relay is currently
/// asking for a new one.
pub struct Demux {
    incoming: mpsc::Receiver<(EndpointStream, String)>,
}

impl Demux {
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
                "the datagram receive loop stopped",
            )
        })
    }
}

/// One peer's share of a forked datagram socket.
///
/// Reads come from the queue the receive loop fills for this address; writes
/// go straight back out of the shared socket, since there is no per-peer
/// socket to connect.
///
/// Nothing here ends a session on its own. The queue closes when the receive
/// loop stops, which happens on shutdown, and otherwise a session lasts as
/// long as its task does. Ending one that has gone quiet is the `timeout`
/// plugin's job.
pub struct Session {
    peer: Peer,
    socket: Socket,
    /// Locked rather than owned because [`DatagramSocket`] is cloned into a
    /// read half and a write half. Only the read half ever takes it, so the
    /// lock is uncontended.
    ///
    /// [`DatagramSocket`]: crate::endpoint::DatagramSocket
    queue: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl Session {
    fn new(peer: Peer, socket: Socket, queue: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            peer,
            socket,
            queue: Mutex::new(queue),
        }
    }

    /// One message, or `None` once this session has ended.
    ///
    /// Cancel safe: `Receiver::recv` loses nothing when dropped, which matters
    /// because the pump drops this future every time a ticking stage wakes it.
    pub(super) async fn recv(&self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        let Some(message) = self.queue.lock().await.recv().await else {
            // The receive loop stopped, so nothing further can arrive.
            return Ok(None);
        };

        // Truncating rather than erroring, which is what a receive into an
        // undersized buffer does one layer down.
        let n = message.len().min(buf.len());
        buf[..n].copy_from_slice(&message[..n]);

        Ok(Some(n))
    }

    pub(super) async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.socket.send_to(buf, &self.peer).await
    }
}

/// Receive from every sender, hand each message to that sender's session, and
/// announce a session the first time an address is heard from.
///
/// One task for the whole endpoint. It must never block on anything a single
/// session controls, or one slow peer stalls the rest.
async fn demultiplex(
    socket: Socket,
    peers: mpsc::Sender<(EndpointStream, String)>,
    max: NonZeroUsize,
    buffer: usize,
    mut shutdown: Shutdown,
) {
    let mut sessions: HashMap<Peer, mpsc::Sender<Vec<u8>>> = HashMap::new();
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

        // Anonymous, or in a namespace `send_to` cannot address. Either way
        // there is no key to file it under and nowhere to send a reply.
        let Some(peer) = peer else {
            drop_datagram(&mut dropped, None, "the sender has no address to reply to");
            continue;
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
                drop_datagram(&mut dropped, Some(&peer), "session is not keeping up");
                continue;
            }
            // Idle, or a stage stopped the path. This message opens a new
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
                    Some(&peer),
                    "ceiling reached; add a timeout stage to reclaim quiet sessions",
                );
                continue;
            }
        }

        let (inbox, queue) = mpsc::channel(SESSION_DEPTH);
        let session = Session::new(peer.clone(), socket.clone(), queue);

        match peers.try_send((EndpointStream::datagram_session(session), peer.to_string())) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                drop_datagram(
                    &mut dropped,
                    Some(&peer),
                    "relay is not accepting quickly enough",
                );
                continue;
            }
            // The relay stopped accepting: there is nobody left to serve.
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }

        // The message that opened the session is its first, not a probe.
        // Unlike the unforked path there is no peek, so nothing else would
        // deliver it.
        let _ = inbox.try_send(buf[..n].to_vec());
        sessions.insert(peer.clone(), inbox);

        info!(%peer, sessions = sessions.len(), "new sender");
    }

    debug!(dropped, "datagram receive loop finished");
}

/// Report a message nobody could take.
///
/// Loud once and quiet after: dropping is normal under load and a warning per
/// message would bury everything else, but an operator who never sees the
/// first one has no way to know it is happening.
fn drop_datagram(dropped: &mut u64, peer: Option<&Peer>, reason: &'static str) {
    *dropped += 1;

    // Only on the cold path, which is what makes the allocation affordable.
    let peer = peer.map_or_else(|| "unnamed".to_owned(), Peer::to_string);

    if *dropped == 1 {
        warn!(
            peer,
            reason, "dropping datagram; further drops are logged at debug"
        );
    } else {
        debug!(peer, reason, "dropping datagram");
    }
}
