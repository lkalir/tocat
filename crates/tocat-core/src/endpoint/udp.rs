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

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    sync::Arc,
};

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
        sockopt::{Family, SocketOptions},
    },
    shutdown::Shutdown,
};

/// Create, configure, and bind, in that order.
///
/// `UdpSocket::bind` does the first and the last in one step, which leaves no
/// moment for `reuseaddr`, so the socket is built by hand. Nothing here is
/// datagram specific except the socket type; the shape is the same on the unix
/// listener uses.
async fn bind_datagram(addr: SocketAddr, options: &SocketOptions) -> std::io::Result<UdpSocket> {
    use rustix::net::{AddressFamily, SocketType, socket};

    let family = if addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };

    let fd = socket(family, SocketType::DGRAM, None)?;
    options.apply_before(&fd)?;
    rustix::net::bind(&fd, &addr)?;

    // tokio requires this of anything handed to `from_std`, and forgetting it
    // blocks the runtime rather than failing.
    let socket = std::net::UdpSocket::from(fd);
    socket.set_nonblocking(true)?;

    UdpSocket::from_std(socket)
}

/// Group membership and the knobs that go with it.
///
/// Joining is a receiver's business and the TTL is a sender's, but both live
/// here because one endpoint is often both: a relay that answers on a group
/// address sends from the same socket it joined on.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Multicast {
    /// The group to join. Without this the rest still applies, which is what a
    /// sender that never joins anything wants.
    pub multicast_group: Option<IpAddr>,

    /// Which interface to join on, and to send from. An address for IPv4, an
    /// interface index for IPv6, because that is what the two kernels take.
    /// Unset lets the routing table choose, which on a host with more than one
    /// interface is a coin toss worth avoiding.
    pub multicast_interface: Option<String>,

    /// Hops a datagram may take. One by default in the kernel, which keeps
    /// multicast on the local segment; raising it is how it leaves.
    pub multicast_ttl: Option<u32>,

    /// Whether a sender also receives its own datagrams. On by default in the
    /// kernel, and the usual reason to turn it off is a relay that would
    /// otherwise hear itself.
    pub multicast_loop: Option<bool>,
}

impl Multicast {
    pub(in crate::endpoint) fn option(
        &mut self,
        opt: &Opt<'_>,
    ) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "multicastgroup" | "multicast" | "group" => {
                self.multicast_group = Some(address(opt)?);
            }
            "multicastinterface" | "interface" | "iface" => {
                self.multicast_interface = Some(opt.string()?);
            }
            "multicastttl" | "ttl" => self.multicast_ttl = Some(opt.count()?.get() as u32),
            "multicastloop" | "loop" => self.multicast_loop = Some(opt.flag()?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// Join and configure, on a socket that is already bound.
    ///
    /// Membership is a property of the socket rather than of the bind, so this
    /// belongs after it. What does not work after the bind is the address the
    /// socket is bound to: joining a group on a socket bound to a specific
    /// unicast address will not receive the group's traffic, so a receiver
    /// binds the wildcard or the group address itself.
    fn apply(&self, socket: &UdpSocket) -> anyhow::Result<()> {
        // The v4 and v6 setters are separate syscalls on separate options, and
        // a socket only has the pair its family uses. The group is what says
        // which; without one, a sender is assumed to be IPv4, which is what an
        // unqualified `ttl=` has always meant elsewhere.
        let v6 = matches!(self.multicast_group, Some(IpAddr::V6(_)));

        if let Some(ttl) = self.multicast_ttl {
            if v6 {
                // tokio has the v6 loop setter but not the hop limit, so this one goes through
                // rustix, as the keepalive timers do.
                rustix::net::sockopt::set_ipv6_multicast_hops(socket, ttl)?;
            } else {
                socket.set_multicast_ttl_v4(ttl)?;
            }
        }

        if let Some(on) = self.multicast_loop {
            if v6 {
                socket.set_multicast_loop_v6(on)?;
            } else {
                socket.set_multicast_loop_v4(on)?;
            }
        }

        let Some(group) = self.multicast_group else {
            return Ok(());
        };

        match group {
            IpAddr::V4(group) => {
                let interface = match &self.multicast_interface {
                    Some(text) => text
                        .parse::<Ipv4Addr>()
                        .with_context(|| format!("{text} is not an IPv4 interface address"))?,
                    None => Ipv4Addr::UNSPECIFIED,
                };

                socket
                    .join_multicast_v4(group, interface)
                    .with_context(|| format!("joining {group} on {interface}"))?;
            }
            IpAddr::V6(group) => {
                let interface = match &self.multicast_interface {
                    Some(text) => text
                        .parse::<u32>()
                        .with_context(|| format!("{text} is not an IPv6 interface index"))?,
                    None => 0,
                };

                socket
                    .join_multicast_v6(&group, interface)
                    .with_context(|| format!("joining {group} on interface {interface}"))?;
            }
        }

        Ok(())
    }
}

fn address(opt: &Opt<'_>) -> Result<IpAddr, ParseEndpointError> {
    opt.text()?
        .parse()
        .map_err(|_| ParseEndpointError::InvalidFlag(format!("not an IP address: {}", opt.key)))
}

/// The first address a name resolves to, or an error naming what failed to
/// resolve.
async fn resolve(addr: impl tokio::net::ToSocketAddrs, what: &str) -> std::io::Result<SocketAddr> {
    tokio::net::lookup_host(addr)
        .await?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("{what} resolved to no address")))
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Udp {
    pub addr: String,
    /// Local address to bind before connecting. Defaults to an ephemeral
    /// port on all interfaces
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(flatten)]
    pub options: SocketOptions,
    #[serde(flatten)]
    pub multicast: Multicast,
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
    #[serde(flatten)]
    pub options: SocketOptions,
    #[serde(flatten)]
    pub multicast: Multicast,
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
        let mut options = SocketOptions::default();
        let mut multicast = Multicast::default();

        for opt in opts {
            match normalize(opt.key).as_str() {
                "bind" => bind = Some(opt.string()?),
                "name" => name = Some(opt.string()?),
                _ if options.option(&opt, Family::Datagram)? => {}
                _ if multicast.option(&opt)? => {}
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            addr: body.to_owned(),
            bind,
            name,
            options,
            multicast,
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

        let local = match &self.bind {
            Some(local) => resolve(local.as_str(), local)
                .await
                .with_context(|| format!("resolving {local}"))?,
            None if peer.is_ipv4() => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
            None => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
        };

        let socket = bind_datagram(local, &self.options)
            .await
            .with_context(|| format!("binding {local}"))?;

        self.options.apply(&socket)?;
        self.multicast.apply(&socket)?;

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
        let mut options = SocketOptions::default();
        let mut multicast = Multicast::default();

        for opt in opts {
            match normalize(opt.key).as_str() {
                "fork" => fork = opt.flag()?,
                "maxconnections" | "maxconn" | "maxconns" => max_connections = Some(opt.count()?),
                "name" => name = Some(opt.string()?),
                _ if options.option(&opt, Family::Datagram)? => {}
                _ if multicast.option(&opt)? => {}
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            host,
            port,
            name,
            fork,
            max_connections,
            options,
            multicast,
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
        let (host, port) = self.addr();
        let addr = resolve((host, port), &format!("{host}:{port}")).await?;

        let socket = bind_datagram(addr, &self.options).await?;
        self.options.apply(&socket)?;
        self.multicast
            .apply(&socket)
            .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        Ok(socket)
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
