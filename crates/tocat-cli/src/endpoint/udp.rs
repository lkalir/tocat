//! udp.rs: `udp:` and `udp-listen:`.
//!
//! Datagram endpoints, so both resolve to [`EndpointStream::Datagram`] rather
//! than to a byte stream: the pump has to see message boundaries.
//!
//! Neither form has an accept. `udp:` connects the socket so the kernel filters
//! to one peer, and `udp-listen:` peeks the first datagram to learn who that
//! peer is and connects to it, leaving the datagram queued for the relay.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::net::UdpSocket;
use tracing::info;

use crate::endpoint::{
    Connection, DEFAULT_HOST, DEFAULT_PORT, EndpointStream,
    parse::{Opt, ParseEndpointError, host_port},
};

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

        for opt in opts {
            match opt.key {
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self { host, port, name })
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

    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let socket = UdpSocket::bind(self.addr()).await?;
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
