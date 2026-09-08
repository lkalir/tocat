//! layer.rs: a handshake stacked on a transport.
//!
//! A transport opens a connection; a layer takes one and returns another.
//! TLS, proxy CONNECT, SOCKS and WebSocket are all that shape, which is why
//! this exists once rather than as four near-identical schemes with their own
//! copies of the TCP connect path.
//!
//! A closed enum dispatched by match, like [`Transport`], rather than a trait
//! object: the set of layers is as closed as the set of schemes, and an
//! `async fn` in a trait is not dyn compatible, so the alternative is a
//! procedural macro dependency bought for nothing.
//!
//! [`Transport`]: crate::endpoint::Transport
//!
//! # Things that are easy to lose in a refactor
//!
//! The predicates read the **top of the stack**, not the transport. A layer
//! decides what the endpoint carries: today TLS fuses message boundaries, and
//! a WebSocket layer will restore them. An endpoint that consults its transport
//! instead reports the wrong shape, and reporting the wrong shape is how a
//! datagram relay silently becomes a stream one.
//!
//! Layers are ordered bottom first. `wss:` is a TCP transport under
//! `[Tls, Ws]`, so `wrap_client` runs in order and each layer wraps what the
//! one below it produced.

pub(in crate::endpoint) mod noise;
pub(in crate::endpoint) mod proxy;
pub(in crate::endpoint) mod socks;
mod tls;
pub(in crate::endpoint) mod ws;

use anyhow::bail;
use serde::{Deserialize, Serialize};

pub use self::{
    noise::{Cipher, Handshake, Hash, KeyFormat, Noise, Static},
    proxy::Proxy,
    socks::Socks,
    tls::{ClientAuth, Tls, Verify},
    ws::Ws,
};
use crate::endpoint::EndpointStream;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LayerSpec {
    Noise(Noise),
    Proxy(Proxy),
    #[serde(rename = "socks5")]
    Socks(Socks),
    Tls(Tls),
    Ws(Ws),
}

impl LayerSpec {
    /// What this layer carries, given what is under it.
    ///
    /// TLS is `Fuse`: a record is not a message, so whatever was underneath,
    /// what comes out is a byte stream.
    pub(in crate::endpoint) fn is_datagram(&self, below: bool) -> bool {
        match self {
            // A tunnel is transparent: what comes out is what went in.
            LayerSpec::Proxy(_) | LayerSpec::Socks(_) => below,
            // Noise messages are discrete, but a record here is a buffer's
            // worth of whatever was written rather than an application message,
            // so this fuses for the same reason TLS does.
            LayerSpec::Noise(_) | LayerSpec::Tls(_) => false,
            // Preserve: one message in is one message out, which is the whole
            // reason to put this over a byte transport.
            LayerSpec::Ws(_) => true,
        }
    }

    /// Who the layers above this one are talking to.
    ///
    /// Everything passes the host through except a tunnel: above a CONNECT the
    /// peer is the target, not the proxy the transport dialled. A certificate
    /// checked against the proxy would pass and prove nothing.
    pub(in crate::endpoint) fn host_above(&self, below: String) -> String {
        match self {
            LayerSpec::Proxy(proxy) => target_host(&proxy.target),
            LayerSpec::Socks(socks) => target_host(&socks.target),
            LayerSpec::Noise(_) | LayerSpec::Tls(_) | LayerSpec::Ws(_) => below,
        }
    }

    /// Refuse a stack that cannot work, before anything is opened.
    pub(in crate::endpoint) fn check(
        &self,
        below_is_datagram: bool,
        listening: bool,
    ) -> anyhow::Result<()> {
        match self {
            LayerSpec::Noise(noise) => noise.check(below_is_datagram, listening),
            LayerSpec::Proxy(proxy) => proxy.check(below_is_datagram, listening),
            LayerSpec::Socks(socks) => socks.check(below_is_datagram, listening),
            LayerSpec::Tls(tls) => tls.check(below_is_datagram, listening),
            LayerSpec::Ws(ws) => ws.check(below_is_datagram),
        }
    }

    /// Wrap a connection this relay dialled. `host` is what the transport was
    /// pointed at, for the layers that need a name to check against.
    pub(in crate::endpoint) async fn wrap_client(
        &self,
        stream: EndpointStream,
        host: &str,
    ) -> anyhow::Result<EndpointStream> {
        match self {
            LayerSpec::Noise(noise) => noise.wrap_client(stream, host).await,
            LayerSpec::Proxy(proxy) => proxy.wrap_client(stream).await,
            LayerSpec::Socks(socks) => socks.wrap_client(stream).await,
            LayerSpec::Tls(tls) => tls.wrap_client(stream, host).await,
            LayerSpec::Ws(ws) => ws.wrap_client(stream, host).await,
        }
    }

    /// Wrap a connection this relay accepted.
    pub(in crate::endpoint) async fn wrap_server(
        &self,
        stream: EndpointStream,
    ) -> anyhow::Result<EndpointStream> {
        match self {
            LayerSpec::Proxy(_) | LayerSpec::Socks(_) => {
                bail!("a proxy layer cannot accept a connection")
            }
            LayerSpec::Noise(noise) => noise.wrap_server(stream).await,
            LayerSpec::Tls(tls) => tls.wrap_server(stream).await,
            LayerSpec::Ws(ws) => ws.wrap_server(stream).await,
        }
    }
}

/// The host half of a `host:port` target, for the layers that reroute.
///
/// A bare name with no port comes back unchanged rather than being rejected
/// here: `check` has already refused an empty target, and a malformed one is
/// the connect's error to report with the context it has.
fn target_host(target: &str) -> String {
    target
        .rsplit_once(':')
        .map_or_else(|| target.to_owned(), |(host, _)| host.to_owned())
}
