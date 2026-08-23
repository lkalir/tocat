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

mod tls;

use serde::{Deserialize, Serialize};

pub use self::tls::{ClientAuth, Tls, Verify};
use crate::endpoint::EndpointStream;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LayerSpec {
    Tls(Tls),
}

impl LayerSpec {
    /// What this layer carries, given what is under it.
    ///
    /// TLS is `Fuse`: a record is not a message, so whatever was underneath,
    /// what comes out is a byte stream.
    pub(in crate::endpoint) fn is_datagram(&self, _below: bool) -> bool {
        match self {
            LayerSpec::Tls(_) => false,
        }
    }

    /// Refuse a stack that cannot work, before anything is opened.
    pub(in crate::endpoint) fn check(
        &self,
        below_is_datagram: bool,
        listening: bool,
    ) -> anyhow::Result<()> {
        match self {
            LayerSpec::Tls(tls) => tls.check(below_is_datagram, listening),
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
            LayerSpec::Tls(tls) => tls.wrap_client(stream, host).await,
        }
    }

    /// Wrap a connection this relay accepted.
    pub(in crate::endpoint) async fn wrap_server(
        &self,
        stream: EndpointStream,
    ) -> anyhow::Result<EndpointStream> {
        match self {
            LayerSpec::Tls(tls) => tls.wrap_server(stream).await,
        }
    }
}
