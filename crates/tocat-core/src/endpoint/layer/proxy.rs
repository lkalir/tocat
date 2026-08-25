//! proxy.rs: an HTTP CONNECT tunnel, as a layer.
//!
//! The first layer whose peer is not the one the transport dialled. TLS and
//! WebSocket wrap a connection to the host underneath them; a proxy dials the
//! *proxy* and then asks it for a tunnel to somewhere else, so everything
//! stacked above this one is talking to the target.
//!
//! That is why [`LayerSpec::host_above`] exists. Without it a `tls:` layer over
//! a proxy would check the certificate against the proxy's name, which would
//! pass and prove nothing.
//!
//! # Boundaries
//!
//! Transparent. Once the tunnel is open the bytes are the peer's, so this
//! passes through whatever was underneath it, unchanged.
//!
//! # Things that are easy to lose in a refactor
//!
//! The response is read **one byte at a time** until the header ends. A
//! buffered read would swallow the first bytes the target sent, which arrive on
//! the same stream immediately after the proxy's reply and belong to the
//! pipeline rather than to this handshake.
//!
//! Client only. A listening transport cannot ask anybody for a tunnel, so this
//! is refused at build time rather than reaching a `wrap_server` that has
//! nothing sensible to do.

use anyhow::{Context as _, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracing::debug;

use crate::endpoint::{
    EndpointStream,
    parse::{Opt, ParseEndpointError},
};

/// A header longer than this is a proxy misbehaving, and a reason to stop
/// rather than to keep reading.
const MAX_HEADER: usize = 8 * 1024;

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Proxy {
    /// Where the tunnel goes, as `host:port`. The transport's address is the
    /// proxy; this is the peer.
    pub target: String,

    /// `user:password` for basic authentication, inline.
    ///
    /// `--dump-config` prints what it is given, so a password written here
    /// appears in a dump. The file and environment forms exist for that reason,
    /// and match what the `encrypt` plugin already does with keys.
    pub proxy_auth: Option<String>,
    pub proxy_auth_file: Option<String>,
    pub proxy_auth_env: Option<String>,
}

impl Proxy {
    pub(in crate::endpoint) fn option(
        &mut self,
        opt: &Opt<'_>,
    ) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "target" => self.target = opt.string()?,
            "proxyauth" => self.proxy_auth = Some(opt.string()?),
            "proxyauthfile" => self.proxy_auth_file = Some(opt.string()?),
            "proxyauthenv" => self.proxy_auth_env = Some(opt.string()?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    pub(in crate::endpoint) fn check(
        &self,
        below_is_datagram: bool,
        listening: bool,
    ) -> anyhow::Result<()> {
        if below_is_datagram {
            bail!("proxy needs a byte stream underneath it");
        }

        if listening {
            bail!("proxy is a client layer: a listening endpoint has nobody to ask for a tunnel");
        }

        if self.target.is_empty() {
            bail!("proxy needs a target, as proxy:proxyhost:port:targethost:port or target=");
        }

        let sources = [
            self.proxy_auth.is_some(),
            self.proxy_auth_file.is_some(),
            self.proxy_auth_env.is_some(),
        ];

        if sources.iter().filter(|set| **set).count() > 1 {
            bail!(
                "proxy-auth, proxy-auth-file and proxy-auth-env are three ways to say the same \
                   thing; pick one"
            );
        }

        Ok(())
    }

    /// The credentials, from whichever source was named.
    fn credentials(&self) -> anyhow::Result<Option<String>> {
        if let Some(inline) = &self.proxy_auth {
            return Ok(Some(inline.clone()));
        }

        if let Some(path) = &self.proxy_auth_file {
            let read = std::fs::read_to_string(path)
                .with_context(|| format!("reading proxy credentials from {path}"))?;

            return Ok(Some(read.trim_end().to_owned()));
        }

        if let Some(name) = &self.proxy_auth_env {
            let read = std::env::var(name)
                .with_context(|| format!("reading proxy credentials from ${name}"))?;

            return Ok(Some(read));
        }

        Ok(None)
    }

    /// Ask the proxy for a tunnel, and hand back the same stream once it is
    /// open.
    ///
    /// The stream is returned unwrapped: after a 200 the proxy is transparent,
    /// so there is nothing to keep in the way of the bytes.
    pub(in crate::endpoint) async fn wrap_client(
        &self,
        stream: EndpointStream,
    ) -> anyhow::Result<EndpointStream> {
        let EndpointStream::Duplex(mut inner) = stream else {
            bail!("proxy needs a two-way stream underneath it");
        };

        let mut request = format!(
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n",
            target = self.target,
        );

        if let Some(credentials) = self.credentials()? {
            request.push_str(&format!(
                "Proxy-Authorization: Basic {}\r\n",
                STANDARD.encode(credentials),
            ));
        }

        request.push_str("\r\n");

        inner
            .write_all(request.as_bytes())
            .await
            .context("sending CONNECT to the proxy")?;

        inner
            .flush()
            .await
            .context("sending CONNECT to the proxy")?;

        let status = read_status(&mut inner)
            .await
            .context("reading the proxy's answer")?;

        debug!(target = %self.target, status, "proxy tunnel open");

        Ok(EndpointStream::Duplex(inner))
    }
}

/// Read the response header and return its status code.
///
/// One byte at a time, on purpose: the target's first bytes follow the proxy's
/// reply on the same stream, and anything read past the blank line would be
/// taken from the pipeline.
async fn read_status(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> anyhow::Result<u16> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        if stream.read_exact(&mut byte).await.is_err() {
            bail!("the proxy closed the connection without answering");
        }

        header.push(byte[0]);

        if header.ends_with(b"\r\n\r\n") {
            break;
        }

        if header.len() > MAX_HEADER {
            bail!("the proxy's answer exceeded {MAX_HEADER} bytes without ending");
        }
    }

    let text = String::from_utf8_lossy(&header);
    let first = text.lines().next().unwrap_or_default();

    let status: u16 = first
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| {
            format!("the proxy answered with {first:?}, which is not a status line")
        })?;

    // 2xx is the tunnel. Everything else is the proxy declining, and its reason
    // line is the most useful thing anyone will get, so it goes in the error.
    if !(200..300).contains(&status) {
        bail!("the proxy refused the tunnel: {}", first.trim());
    }

    Ok(status)
}

/// Split `proxyhost:port:targethost:port` into the transport's address and the
/// layer's target.
///
/// The proxy is the first two components because the transport is what the
/// scheme's address has always meant; the target is everything after, which
/// leaves an IPv6 target's own colons alone.
pub(in crate::endpoint) fn split_target(body: &str) -> Option<(&str, &str)> {
    let (host, rest) = body.split_once(':')?;
    let (port, target) = rest.split_once(':')?;

    if host.is_empty() || port.is_empty() || target.is_empty() {
        return None;
    }

    Some((&body[..host.len() + 1 + port.len()], target))
}
