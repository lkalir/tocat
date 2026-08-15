//! udp.rs: `udp:` and `udp-listen:`.
//!
//! Datagram endpoints, so both resolve to [`EndpointStream::Datagram`] rather
//! than to a byte stream: the pump has to see message boundaries.
//!
//! Neither form has an accept. `udp:` connects the socket so the kernel filters
//! to one peer. `udp-listen:` has two shapes. Without `fork` it peeks the first
//! datagram to learn who the peer is and connects to it, leaving the datagram
//! queued for the relay. With `fork` it hands the unconnected socket to the
//! demultiplexer in [`datagram`], which routes by source address so that every
//! sender gets its own session, its own dialled peer and its own plugin
//! instances. Sessions, their ceiling and what ends one all live there, shared
//! with `unix-dgram-listen:`.
//!
//! [`datagram`]: crate::endpoint::datagram

use std::{num::NonZeroUsize, sync::Arc};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::net::UdpSocket;
use tracing::info;

use crate::{
    endpoint::{
        Connection, DEFAULT_HOST, DEFAULT_MAX_CONNECTIONS, DEFAULT_PORT, EndpointStream,
        datagram::{self, Demux},
        parse::{Opt, ParseEndpointError, host_port},
    },
    shutdown::Shutdown,
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
    pub async fn demux(&self, buffer: usize, shutdown: Shutdown) -> anyhow::Result<Demux> {
        let socket = Arc::new(self.bind().await?);

        info!(local = %socket.local_addr()?, "listening for datagrams");

        Ok(datagram::demux(
            datagram::Socket::Udp(socket),
            self.max_connections(),
            buffer,
            shutdown,
        ))
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
