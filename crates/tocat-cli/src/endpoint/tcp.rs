//! tcp.rs: `tcp:` and `tcp-listen:`.
//!
//! The connecting form is the plain case. The listening form is the one with a
//! second caller: [`TcpListen::bind`] is used both by [`TcpListen::connect`],
//! which accepts exactly one peer and relays it, and by the relay's `fork`
//! loop, which keeps the listener and accepts repeatedly. Sharing `bind` is
//! what keeps the host and port defaults in one place.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::net::{TcpListener, TcpStream};
use tracing::info;

use crate::endpoint::{
    Connection, DEFAULT_HOST, DEFAULT_PORT, EndpointStream,
    parse::{Opt, ParseEndpointError, host_port},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct Tcp {
    pub addr: String,
    #[serde(default)]
    pub name: Option<String>,
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

        for opt in opts {
            match normalize(opt.key).as_str() {
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            addr: body.to_owned(),
            name,
        })
    }

    pub(super) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("tcp://{}", self.addr))
    }

    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let stream = TcpStream::connect(&self.addr).await?;
        Ok(EndpointStream::tcp(stream).into_connection())
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

        for opt in opts {
            match normalize(opt.key).as_str() {
                "fork" => fork = opt.flag()?,
                "maxconnections" | "maxconn" => {
                    max_connections = Some(opt.count()?);
                }
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
    pub async fn bind(&self) -> std::io::Result<TcpListener> {
        TcpListener::bind(self.addr()).await
    }

    /// Bind and take a single peer.
    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let (host, port) = self.addr();
        let listener = self.bind().await?;
        info!("Listening for connection on {host}:{port}");
        let (stream, peer) = listener.accept().await?;
        info!("Accepted connection from {peer}");
        Ok(EndpointStream::tcp(stream).into_connection())
    }
}
