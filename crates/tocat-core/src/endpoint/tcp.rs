//! tcp.rs: `tcp:` and `tcp-listen:`.
//!
//! The connecting form is the plain case. The listening form is the one with a
//! second caller: [`TcpListen::bind`] is used both by [`TcpListen::connect`],
//! which accepts exactly one peer and relays it, and by the relay's `fork`
//! loop, which keeps the listener and accepts repeatedly. Sharing `bind` is
//! what keeps the host and port defaults in one place.

use std::num::NonZeroUsize;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tracing::info;

use crate::endpoint::{
    Connection, DEFAULT_HOST, DEFAULT_PORT, EndpointStream,
    parse::{Opt, ParseEndpointError, host_port},
    retry::Retry,
    sockopt::{DEFAULT_BACKLOG, Family, SocketOptions},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct Tcp {
    pub addr: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(flatten)]
    pub options: SocketOptions,
    #[serde(flatten)]
    pub retry: Retry,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TcpListen {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub fork: bool,
    #[serde(default, rename = "max-connections")]
    pub max_connections: Option<NonZeroUsize>,
    #[serde(default)]
    pub backlog: Option<NonZeroUsize>,
    #[serde(flatten)]
    pub options: SocketOptions,
}

impl Tcp {
    const SCHEME: &'static str = "tcp";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        if body.is_empty() {
            return Err(ParseEndpointError::Empty);
        }

        let mut name = None;
        let mut bind = None;
        let mut options = SocketOptions::default();
        let mut retry = Retry::default();

        for opt in opts {
            match normalize(opt.key).as_str() {
                "name" => name = Some(opt.string()?),
                "bind" => bind = Some(opt.string()?),
                _ if options.option(&opt, Family::Tcp)? => {}
                _ if retry.option(&opt)? => {}
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            addr: body.to_owned(),
            name,
            bind,
            options,
            retry,
        })
    }

    pub(super) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("tcp://{}", self.addr))
    }

    /// Dial, applying whatever has to be set before the connect and whatever
    /// belongs to the connection once it exists.
    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let stream = if self.bind.is_some() || self.options.needs_socket() {
            self.connect_from_socket().await?
        } else {
            TcpStream::connect(&self.addr).await?
        };

        self.options.apply(&stream)?;

        Ok(EndpointStream::tcp(stream).into_connection())
    }

    async fn connect_from_socket(&self) -> anyhow::Result<TcpStream> {
        let peer = tokio::net::lookup_host(&self.addr)
            .await
            .with_context(|| format!("resolving {}", self.addr))?
            .next()
            .with_context(|| format!("{} resolved to no address", self.addr))?;

        let socket = if peer.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };

        self.options.apply_before(&socket)?;

        if let Some(local) = &self.bind {
            let local = tokio::net::lookup_host(local)
                .await
                .with_context(|| format!("resolving {local}"))?
                .find(|candidate| candidate.is_ipv4() == peer.is_ipv4())
                .with_context(|| {
                    format!(
                        "{local} has no address in the same family as {peer}; a local address and \
                        its peer have to be both IPv4 or both IPv6",
                    )
                })?;
            socket.bind(local)?;
        }

        Ok(socket.connect(peer).await?)
    }
}

impl TcpListen {
    const SCHEME: &'static str = "tcp-listen";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let (host, port) = host_port(body)?;

        let mut name = None;
        let mut fork = false;
        let mut max_connections = None;
        let mut backlog = None;
        let mut options = SocketOptions::default();

        for opt in opts {
            match normalize(opt.key).as_str() {
                "backlog" => backlog = Some(opt.count()?),
                "fork" => fork = opt.flag()?,
                "maxconnections" | "maxconn" => {
                    max_connections = Some(opt.count()?);
                }
                "name" => name = Some(opt.string()?),
                _ if options.option(&opt, Family::Tcp)? => {}
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            host,
            port,
            name,
            fork,
            max_connections,
            backlog,
            options,
        })
    }

    pub(super) fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }

        let (host, port) = self.addr();
        format!("tcp://{host}:{port}")
    }

    /// Where to bind, with the defaults filled in.
    pub fn addr(&self) -> (&str, u16) {
        (
            self.host.as_deref().unwrap_or(DEFAULT_HOST),
            self.port.unwrap_or(DEFAULT_PORT),
        )
    }

    /// Bind without accepting, for callers that own the accept loop.
    pub async fn bind(&self) -> anyhow::Result<TcpListener> {
        let (host, port) = self.addr();

        let addr = tokio::net::lookup_host((host, port))
            .await
            .with_context(|| format!("resolving {host}:{port}"))?
            .next()
            .with_context(|| format!("{host}:{port} resolved to no address"))?;

        let socket = if addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };

        self.options.apply_before(&socket)?;
        socket.bind(addr)?;

        Ok(socket.listen(self.backlog())?)
    }

    /// How deep to let the kernel queue connections this relay has not yet
    /// accepted.
    pub fn backlog(&self) -> u32 {
        self.backlog.map_or(DEFAULT_BACKLOG, |n| {
            u32::try_from(n.get()).unwrap_or(u32::MAX)
        })
    }

    /// Options an accepted connection carries, applied by whoever accepted it.
    pub fn accepted(&self, stream: &TcpStream) -> std::io::Result<()> {
        self.options.apply(stream)
    }

    /// Bind and take a single peer.
    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let listener = self.bind().await?;
        info!(local = %listener.local_addr()?, "listening");
        let (stream, peer) = listener.accept().await?;
        info!("Accepted connection from {peer}");
        self.accepted(&stream)?;
        Ok(EndpointStream::tcp(stream).into_connection())
    }
}
