//! ws.rs: WebSocket over a byte stream, as a layer.
//!
//! The first layer that changes what an endpoint carries. TLS takes a stream
//! and returns a stream; this takes a stream and returns a message endpoint, so
//! `ws:` is a datagram endpoint sitting on a TCP transport and the boundary
//! rules apply to it exactly as they do to `udp:`.
//!
//! That is also what makes it the first useful message-oriented *network*
//! transport: `frame`, `unframe` and the datagram-safe stages have until now
//! only been reachable over unix sockets.
//!
//! # Boundaries
//!
//! `Preserve`. One WebSocket message in is one message out, which is the whole
//! point of the layer and the reason it declares the opposite of TLS.
//!
//! # Things that are easy to lose in a refactor
//!
//! The stream is **split** rather than held behind one lock. `recv` and `send`
//! are called concurrently by the two directions of the pump, so a single mutex
//! would let a blocked read hold it against a write until the peer said
//! something, which for a relay is a deadlock rather than a slowdown.
//!
//! A message longer than the copy buffer is an **error**, not a truncation.
//! Every other message endpoint here truncates, because a datagram sender does
//! not know what the receiver will accept; a WebSocket peer does believe it
//! sent a whole message, and quietly delivering half of one is worse than
//! failing.
//!
//! Text frames are accepted and their bytes passed through. What is *sent* is
//! binary unless `text=true`, because a text frame promises valid UTF-8 and
//! only the person running the relay knows whether the bytes crossing it are
//! text. Under `text=true` a message that is not valid UTF-8 is an error.

use anyhow::{Context as _, bail};
use futures::{
    SinkExt as _, StreamExt as _,
    future::BoxFuture,
    stream::{SplitSink, SplitStream},
};
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async, client_async,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
        protocol::{Message, frame::coding::CloseCode},
    },
};
use tracing::warn;

use crate::endpoint::{
    EndpointStream, MessageSocket,
    parse::{Opt, ParseEndpointError},
    stream::AsyncStream,
};

type Socket = WebSocketStream<Box<dyn AsyncStream>>;

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Ws {
    /// The resource. `ws:host:9000/socket` is sugar for this.
    ///
    /// The same value on both sides, asked for when dialling and required when
    /// accepting: unset is `/` either way, so a listener with no path serves
    /// `/` and answers anything else with a 404 rather than upgrading it.
    pub path: Option<String>,

    /// Send text frames rather than binary ones, for a peer that accepts
    /// nothing else.
    ///
    /// Whether the bytes crossing the relay are text is not something tocat
    /// knows, so this is a promise the person running it makes. A message that
    /// is not valid UTF-8 is an error rather than a mangled frame: the
    /// alternative is sending something a peer is entitled to reject, or
    /// replacing bytes, and neither is a relay's business.
    ///
    /// Received frames are unaffected. Text arrives as its bytes either way.
    pub text: bool,
}

impl Ws {
    pub(in crate::endpoint) fn option(
        &mut self,
        opt: &Opt<'_>,
    ) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "path" => self.path = Some(opt.string()?),
            "text" => self.text = opt.flag()?,
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// The path as it goes on the wire, always absolute.
    fn path(&self) -> String {
        match self.path.as_deref() {
            None | Some("") => "/".to_owned(),
            Some(path) if path.starts_with('/') => path.to_owned(),
            Some(path) => format!("/{path}"),
        }
    }

    pub(in crate::endpoint) fn check(&self, below_is_datagram: bool) -> anyhow::Result<()> {
        if below_is_datagram {
            bail!("ws needs a byte stream underneath it");
        }

        Ok(())
    }

    /// Send the HTTP upgrade and take the message stream that results.
    ///
    /// `host` is what the transport dialled, which is what the `Host` header
    /// has to say whether or not a TLS layer is also using it for a name check.
    pub(in crate::endpoint) async fn wrap_client(
        &self,
        stream: EndpointStream,
        host: &str,
    ) -> anyhow::Result<EndpointStream> {
        let EndpointStream::Duplex(inner) = stream else {
            bail!("ws needs a two-way stream underneath it");
        };

        // The scheme in this URL only tells tungstenite which default port to
        // assume for the Host header; the transport is already connected, and
        // TLS, if any, is the layer below this one.
        let url = format!("ws://{host}{}", self.path());

        let (socket, _response) = client_async(&url, inner)
            .await
            .with_context(|| format!("websocket handshake with {url}"))?;

        Ok(EndpointStream::message(Frames::new(socket, self.text)))
    }

    pub(in crate::endpoint) async fn wrap_server(
        &self,
        stream: EndpointStream,
    ) -> anyhow::Result<EndpointStream> {
        let EndpointStream::Duplex(inner) = stream else {
            bail!("ws needs a two-way stream underneath it");
        };

        let wanted = self.path();

        #[expect(
            clippy::result_large_err,
            reason = "tungstenite owns the signature, and we only pay this once per connection"
        )]
        let check = |request: &Request, response: Response| {
            let asked = request.uri().path();

            if asked == wanted {
                return Ok(response);
            }

            warn!(asked, serving = %wanted, "refusing a websocket upgrade for another path");

            let mut refusal =
                ErrorResponse::new(Some(format!("this endpoint does not serve {asked}")));

            *refusal.status_mut() = StatusCode::NOT_FOUND;

            Err(refusal)
        };

        let socket = accept_hdr_async(inner, check)
            .await
            .context("websocket handshake with client")?;

        Ok(EndpointStream::message(Frames::new(socket, self.text)))
    }
}

/// A WebSocket as a message endpoint.
struct Frames {
    /// See [`Ws::text`].
    text: bool,

    /// Split so that a read waiting on the peer cannot block a write. The two
    /// halves coordinate through the sink's own lock, which is what makes
    /// tungstenite's automatic pong replies leave on the next write rather than
    /// immediately: a relay that never sends can let a peer's ping time out.
    reader: AsyncMutex<SplitStream<Socket>>,
    writer: AsyncMutex<SplitSink<Socket, Message>>,
}

impl Frames {
    fn new(socket: Socket, text: bool) -> Self {
        let (writer, reader) = socket.split();

        Self {
            text,
            reader: AsyncMutex::new(reader),
            writer: AsyncMutex::new(writer),
        }
    }
}

impl MessageSocket for Frames {
    fn recv<'a>(&'a self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<Option<usize>>> {
        Box::pin(async move {
            let mut reader = self.reader.lock().await;

            loop {
                let Some(message) = reader.next().await else {
                    return Ok(None);
                };

                let payload = match message.map_err(std::io::Error::other)? {
                    Message::Binary(bytes) => bytes,
                    // Accepted, and its bytes passed on as they are. What the
                    // peer promised about them is the peer's business.
                    Message::Text(text) => text.into(),
                    // A close is end of stream, not an empty message.
                    Message::Close(_) => return Ok(None),
                    // Answered by tungstenite, and none of the pipeline's
                    // business either way.
                    Message::Ping(_) => {
                        let _ = self.writer.lock().await.flush().await;
                        continue;
                    }
                    Message::Pong(_) | Message::Frame(_) => continue,
                };

                if payload.len() > buf.len() {
                    return Err(std::io::Error::other(format!(
                        "a {} byte message does not fit the {} byte copy buffer; raise -b",
                        payload.len(),
                        buf.len(),
                    )));
                }

                buf[..payload.len()].copy_from_slice(&payload);

                return Ok(Some(payload.len()));
            }
        })
    }

    fn send<'a>(&'a self, buf: &'a [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(async move {
            let message = if self.text {
                let text = std::str::from_utf8(buf).map_err(|e| {
                    std::io::Error::other(format!(
                        "text=true was asked for and this message is not valid UTF-8: {e}",
                    ))
                })?;

                Message::text(text)
            } else {
                Message::binary(buf.to_vec())
            };

            self.writer
                .lock()
                .await
                .send(message)
                .await
                .map_err(std::io::Error::other)?;

            Ok(buf.len())
        })
    }

    /// End of stream, which for WebSocket is a frame rather than a shutdown.
    ///
    /// This is why the trait's `finish` is async: a close has to be written and
    /// flushed, and a peer that never receives one sees the connection as
    /// abnormally terminated.
    fn finish<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut writer = self.writer.lock().await;

            let _ = writer
                .send(Message::Close(Some(
                    tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: CloseCode::Normal,
                        reason: Default::default(),
                    },
                )))
                .await;

            let _ = writer.close().await;
        })
    }
}

/// Split `host:port/path` into the address and the path, so that the sugared
/// form and `path=` mean the same thing.
///
/// The path is taken from the first `/` after the address, which a host or a
/// port cannot contain, so there is nothing to disambiguate.
pub(in crate::endpoint) fn split_path(body: &str) -> (&str, Option<&str>) {
    match body.split_once('/') {
        Some((addr, path)) => (addr, Some(path)),
        None => (body, None),
    }
}
