//! seqpacket.rs: `unix-seqpacket:` and `unix-seqpacket-listen:`.
//!
//! `SOCK_SEQPACKET` is the one transport tocat speaks that is connection
//! oriented *and* message oriented, so it resolves to
//! [`EndpointStream::Datagram`] like `udp:` does, and not to a byte stream.
//! Boundaries are the whole reason to choose it over `unix:`, and
//! `AsyncRead`/`AsyncWrite` have nowhere to put one: a write that went out in
//! two calls would arrive as two messages. Being a datagram endpoint is also
//! what puts it under the boundary checks in `relay`, so a stage that cannot
//! carry messages is reported here exactly as it is on a UDP path.
//!
//! Two things separate it from the connectionless forms, and both are
//! [`SeqpacketConn`]'s business.
//!
//! **It has an end of stream.** A zero-length receive is the peer's shutdown,
//! which is why this is the one datagram endpoint whose `recv` can return
//! `None` without a stage halting the path first, and why
//! [`SeqpacketConn::finish`] sends `SHUT_WR`: without it a one-way relay
//! leaves the peer parked on a read forever. The cost is that a genuine
//! zero-length message is indistinguishable from that shutdown and ends the
//! path.
//!
//! **Truncation is visible.** The kernel reports a message that did not fit,
//! so an undersized `-b` is a warning rather than silent corruption, which is
//! more than UDP can offer.

use std::{
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio_seqpacket::{UnixSeqpacket as SeqpacketSocket, UnixSeqpacketListener};
use tracing::{debug, info, warn};

use crate::endpoint::{
    Connection, EndpointStream,
    parse::{Opt, ParseEndpointError},
    sys::Mode,
    unix::{SocketPath, apply_mode, unlink_stale},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct UnixSeqpacket {
    pub path: SocketPath,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UnixSeqpacketListen {
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

impl UnixSeqpacket {
    const SCHEME: &'static str = "unix-seqpacket";

    pub(in crate::endpoint) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut name = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            path: SocketPath::from_spec(body),
            name,
        })
    }

    pub(in crate::endpoint) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("unix-seqpacket://{}", self.path))
    }

    pub(in crate::endpoint) async fn connect(&self) -> anyhow::Result<Connection> {
        self.path.supported()?;

        let socket = SeqpacketSocket::connect(self.path.as_path())
            .await
            .with_context(|| format!("connecting to {}", self.path))?;

        Ok(EndpointStream::seqpacket(socket).into_connection())
    }
}

impl UnixSeqpacketListen {
    const SCHEME: &'static str = "unix-seqpacket-listen";

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
            .unwrap_or_else(|| format!("unix-seqpacket://{}", self.path))
    }

    /// Bind without accepting, for callers that own the accept loop.
    ///
    /// The caller is responsible for the [`PathGuard`]: this only creates the
    /// socket.
    ///
    /// [`PathGuard`]: crate::endpoint::PathGuard
    pub async fn bind(&self) -> anyhow::Result<UnixSeqpacketListener> {
        self.path.supported()?;

        if self.unlink {
            unlink_stale(&self.path, async {
                SeqpacketSocket::connect(self.path.as_path())
                    .await
                    .map(drop)
            })
            .await?;
        }

        let listener = UnixSeqpacketListener::bind(self.path.as_path())
            .with_context(|| format!("binding {}", self.path))?;

        apply_mode(&self.path, self.mode)?;

        Ok(listener)
    }

    /// Bind and take a single peer.
    pub(in crate::endpoint) async fn connect(&self) -> anyhow::Result<Connection> {
        let mut listener = self.bind().await?;
        let guard = self.path.guard();
        info!(path = %self.path, "listening");
        let socket = listener.accept().await?;

        Ok(EndpointStream::seqpacket(socket).into_connection_with_guard(guard))
    }
}

/// One connected seqpacket socket, as the datagram half of an endpoint.
///
/// Shared through an `Arc` by the reading and the writing half, which is why
/// every method takes `&self`; the underlying socket is safe to use from both
/// at once and delivers each message intact regardless.
pub struct SeqpacketConn {
    socket: SeqpacketSocket,
    /// Whether a truncated message has already been reported. Truncation is
    /// data loss and the operator needs to hear it, but a small `-b` under a
    /// message flood would otherwise bury every other line in the log.
    truncated: AtomicBool,
}

impl SeqpacketConn {
    pub(in crate::endpoint) fn new(socket: SeqpacketSocket) -> Self {
        Self {
            socket,
            truncated: AtomicBool::new(false),
        }
    }

    /// One message, or `None` once the peer has shut down.
    ///
    /// Cancel safe: the future dropped is a `poll_recv`, which consumes
    /// nothing when it does not complete. The pump drops it every time a
    /// ticking stage wakes up, so this has to stay true.
    pub(in crate::endpoint) async fn recv(&self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        let message = self.socket.recv(buf).await?;

        if message.truncated() {
            self.report_truncation(buf.len());
        }

        // Zero bytes is the peer's `SHUT_WR` arriving, the one end of stream a
        // message endpoint has. A genuinely empty message reads the same way
        // and ends the path with it.
        let bytes = message.bytes_read();

        Ok((bytes > 0).then_some(bytes))
    }

    pub(in crate::endpoint) async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.socket.send(buf).await
    }

    /// Tell the peer that nothing further is coming.
    ///
    /// Half close rather than a drop: the reading half is still live on the
    /// other task, and a peer that is waiting for our end of stream before it
    /// replies would otherwise never hear one.
    pub(in crate::endpoint) fn finish(&self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Write);
    }

    fn report_truncation(&self, buffer: usize) {
        if self.truncated.swap(true, Ordering::Relaxed) {
            debug!(buffer, "truncated a message that did not fit the buffer");
        } else {
            warn!(
                buffer,
                "a message did not fit the buffer and the rest of it is lost; raise -b past the \
                 largest message the peer sends. Further truncations are logged at debug",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointSpec;

    fn dial(s: &str) -> UnixSeqpacket {
        match s.parse::<EndpointSpec>().expect("parses") {
            EndpointSpec::UnixSeqpacket(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn listen(s: &str) -> UnixSeqpacketListen {
        match s.parse::<EndpointSpec>().expect("parses") {
            EndpointSpec::UnixSeqpacketListen(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Every spelling anyone is likely to try has to land on the same scheme,
    /// and the listening ones must not be swallowed by the dialling arm.
    #[test]
    fn the_scheme_answers_to_its_spellings() {
        for spec in [
            "unix-seqpacket:/tmp/tocat.sock",
            "unix-seqpkt:/tmp/tocat.sock",
            "uds-seqpacket:/tmp/tocat.sock",
            "seqpacket:/tmp/tocat.sock",
        ] {
            assert_eq!(dial(spec).path, SocketPath::from_spec("/tmp/tocat.sock"));
        }

        for spec in [
            "unix-seqpacket-listen:/tmp/tocat.sock",
            "seqpacket-listen:/tmp/tocat.sock",
        ] {
            assert!(!listen(spec).fork);
        }
    }

    #[test]
    fn the_listening_options_match_the_stream_form() {
        let e = listen("unix-seqpacket-listen:@tocat,fork,max-connections=8");

        assert!(e.fork);
        assert!(e.path.is_abstract());
        assert_eq!(e.max_connections, NonZeroUsize::new(8));
    }

    /// Dialling accepts nothing but a label, so anything else is a mistake
    /// worth reporting rather than ignoring.
    #[test]
    fn dialling_rejects_the_listening_options() {
        assert!(matches!(
            "unix-seqpacket:/tmp/tocat.sock,unlink"
                .parse::<EndpointSpec>()
                .expect_err("rejected"),
            ParseEndpointError::UnsupportedOption { .. }
        ));
    }

    #[test]
    fn the_label_says_which_socket_type_it_is() {
        assert_eq!(
            dial("unix-seqpacket:/tmp/tocat.sock").label(),
            "unix-seqpacket:///tmp/tocat.sock"
        );
        assert_eq!(
            listen("unix-seqpacket-listen:@tocat").label(),
            "unix-seqpacket://@tocat"
        );
    }
}
