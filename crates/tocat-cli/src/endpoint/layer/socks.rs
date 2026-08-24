//! socks.rs: a SOCKS5 tunnel, as a layer.
//!
//! The same shape as [`proxy`](super::proxy): the transport dials the proxy,
//! this asks for a route to somewhere else, and everything above it is talking
//! to the target. It reports that through `host_above` for the same reason.
//!
//! What differs is the handshake, which is binary rather than HTTP, and the
//! address form, which lets the proxy do the name lookup. That last part is the
//! reason to prefer SOCKS over CONNECT when both are available: a hostname sent
//! as a name is resolved on the far side, so a target only the proxy's network
//! can resolve still works, and no lookup for it happens here.
//!
//! Only version 5. SOCKS4a exists and nothing has deployed it this decade;
//! adding it would be a second protocol for the same job.
//!
//! # Boundaries
//!
//! Transparent, as CONNECT is. After the reply the bytes are the target's.
//!
//! # Things that are easy to lose in a refactor
//!
//! Every read here is exact. The greeting, the authentication reply and the
//! connect reply all have known lengths, and the target's first bytes follow
//! the last of them on the same stream, so a read that took more than it needed
//! would take them from the pipeline.
//!
//! The bound address in the reply is read and discarded, but it has to be read:
//! it is variable length, and skipping it by assumption rather than by its own
//! length byte leaves the stream misaligned in a way that shows up as garbage
//! much later.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracing::debug;

use crate::endpoint::{
    EndpointStream,
    parse::{Opt, ParseEndpointError},
};

const VERSION: u8 = 5;
const NO_AUTH: u8 = 0;
const USER_PASS: u8 = 2;
const NO_ACCEPTABLE: u8 = 0xff;

const CONNECT: u8 = 1;
const ADDR_IPV4: u8 = 1;
const ADDR_NAME: u8 = 3;
const ADDR_IPV6: u8 = 4;

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Socks {
    /// Where the route goes, as `host:port`. The transport's address is the
    /// proxy; this is the peer.
    pub target: String,

    /// `user:password`, inline. As on the CONNECT layer, the file and
    /// environment forms exist because a dump prints what it is given.
    pub proxy_auth: Option<String>,
    pub proxy_auth_file: Option<String>,
    pub proxy_auth_env: Option<String>,

    /// Resolve the target here rather than sending the name to the proxy.
    ///
    /// Off by default, which is the point of SOCKS: a name the proxy's network
    /// can resolve and yours cannot still works, and nothing leaks which host
    /// is being asked for to a local resolver.
    pub resolve_locally: bool,
}

impl Socks {
    pub(in crate::endpoint) fn option(
        &mut self,
        opt: &Opt<'_>,
    ) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "target" => self.target = opt.string()?,
            "proxyauth" => self.proxy_auth = Some(opt.string()?),
            "proxyauthfile" => self.proxy_auth_file = Some(opt.string()?),
            "proxyauthenv" => self.proxy_auth_env = Some(opt.string()?),
            "resolvelocally" | "resolve" => self.resolve_locally = opt.flag()?,
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
            bail!("socks5 needs a byte stream underneath it");
        }

        if listening {
            bail!("socks5 is a client layer: a listening endpoint has nobody to ask for a route");
        }

        if self.target.is_empty() {
            bail!("socks5 needs a target, as socks5:proxyhost:port:targethost:port or target=");
        }

        let sources = [
            self.proxy_auth.is_some(),
            self.proxy_auth_file.is_some(),
            self.proxy_auth_env.is_some(),
        ];

        if sources.iter().filter(|set| **set).count() > 1 {
            bail!(
                "proxy-auth, proxy-auth-file and proxy-auth-env are three ways to say the same \
                 thing; pick one",
            );
        }

        Ok(())
    }

    fn credentials(&self) -> anyhow::Result<Option<(String, String)>> {
        let raw = if let Some(inline) = &self.proxy_auth {
            inline.clone()
        } else if let Some(path) = &self.proxy_auth_file {
            std::fs::read_to_string(path)
                .with_context(|| format!("reading proxy credentials from {path}"))?
                .trim_end()
                .to_owned()
        } else if let Some(name) = &self.proxy_auth_env {
            std::env::var(name)
                .with_context(|| format!("reading proxy credentials from ${name}"))?
        } else {
            return Ok(None);
        };

        // The password may contain colons; the username may not, which is what
        // splitting once from the left means.
        let (user, password) = raw
            .split_once(':')
            .context("proxy credentials are user:password")?;

        if user.len() > 255 || password.len() > 255 {
            bail!("a SOCKS5 username and password are at most 255 bytes each");
        }

        Ok(Some((user.to_owned(), password.to_owned())))
    }

    pub(in crate::endpoint) async fn wrap_client(
        &self,
        stream: EndpointStream,
    ) -> anyhow::Result<EndpointStream> {
        let EndpointStream::Duplex(mut inner) = stream else {
            bail!("socks5 needs a two-way stream underneath it");
        };

        let credentials = self.credentials()?;

        // Offer what we can actually do. A proxy that wants a password from a
        // relay that has none is a configuration error worth naming.
        let methods: &[u8] = if credentials.is_some() {
            &[USER_PASS, NO_AUTH]
        } else {
            &[NO_AUTH]
        };

        let mut greeting = vec![VERSION, methods.len() as u8];
        greeting.extend_from_slice(methods);

        inner
            .write_all(&greeting)
            .await
            .context("greeting the socks5 proxy")?;

        let mut answer = [0u8; 2];
        inner
            .read_exact(&mut answer)
            .await
            .context("reading the socks5 greeting reply")?;

        if answer[0] != VERSION {
            bail!("the proxy answered with SOCKS version {}, not 5", answer[0]);
        }

        match answer[1] {
            NO_AUTH => {}
            USER_PASS => {
                let Some((user, password)) = &credentials else {
                    bail!("the proxy wants a username and password; set proxy-auth");
                };

                authenticate(&mut inner, user, password).await?;
            }
            NO_ACCEPTABLE => bail!(
                "the proxy accepted none of the authentication methods offered; it may want a \
                 username and password, which proxy-auth supplies",
            ),
            other => bail!("the proxy chose authentication method {other}, which is not supported"),
        }

        self.connect(&mut inner).await?;

        debug!(target = %self.target, "socks5 route open");

        Ok(EndpointStream::Duplex(inner))
    }

    /// Ask for the route, and read the reply.
    async fn connect(
        &self,
        stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    ) -> anyhow::Result<()> {
        let (host, port) = self
            .target
            .rsplit_once(':')
            .with_context(|| format!("{} is not host:port", self.target))?;

        let port: u16 = port
            .parse()
            .with_context(|| format!("{port} is not a port"))?;

        let host = host.trim_start_matches('[').trim_end_matches(']');

        let mut request = vec![VERSION, CONNECT, 0];

        match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(addr)) => {
                request.push(ADDR_IPV4);
                request.extend_from_slice(&addr.octets());
            }
            Ok(std::net::IpAddr::V6(addr)) => {
                request.push(ADDR_IPV6);
                request.extend_from_slice(&addr.octets());
            }
            // A name goes as a name unless asked otherwise, so the proxy does
            // the lookup. See [`Socks::resolve_locally`].
            Err(_) if !self.resolve_locally => {
                if host.len() > 255 {
                    bail!("a SOCKS5 hostname is at most 255 bytes");
                }

                request.push(ADDR_NAME);
                request.push(host.len() as u8);
                request.extend_from_slice(host.as_bytes());
            }
            Err(_) => {
                let resolved = tokio::net::lookup_host((host, port))
                    .await
                    .with_context(|| format!("resolving {host}"))?
                    .next()
                    .with_context(|| format!("{host} resolved to no address"))?;

                match resolved.ip() {
                    std::net::IpAddr::V4(addr) => {
                        request.push(ADDR_IPV4);
                        request.extend_from_slice(&addr.octets());
                    }
                    std::net::IpAddr::V6(addr) => {
                        request.push(ADDR_IPV6);
                        request.extend_from_slice(&addr.octets());
                    }
                }
            }
        }

        request.extend_from_slice(&port.to_be_bytes());

        stream
            .write_all(&request)
            .await
            .context("sending the socks5 connect request")?;

        let mut reply = [0u8; 4];
        stream
            .read_exact(&mut reply)
            .await
            .context("reading the socks5 connect reply")?;

        if reply[1] != 0 {
            bail!("the proxy refused the route: {}", refusal(reply[1]));
        }

        // The bound address is not useful here, but it is variable length and
        // has to be consumed exactly or everything after it is misaligned.
        match reply[3] {
            ADDR_IPV4 => drain(stream, 4 + 2).await?,
            ADDR_IPV6 => drain(stream, 16 + 2).await?,
            ADDR_NAME => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len).await?;
                drain(stream, len[0] as usize + 2).await?;
            }
            other => bail!("the proxy replied with address type {other}, which is not known"),
        }

        Ok(())
    }
}

async fn authenticate(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    user: &str,
    password: &str,
) -> anyhow::Result<()> {
    // Version 1 here is the username and password subnegotiation's own version,
    // not SOCKS5's.
    let mut message = vec![1, user.len() as u8];
    message.extend_from_slice(user.as_bytes());
    message.push(password.len() as u8);
    message.extend_from_slice(password.as_bytes());

    stream
        .write_all(&message)
        .await
        .context("sending socks5 credentials")?;

    let mut reply = [0u8; 2];
    stream
        .read_exact(&mut reply)
        .await
        .context("reading the socks5 authentication reply")?;

    if reply[1] != 0 {
        bail!("the proxy rejected the username and password");
    }

    Ok(())
}

async fn drain(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    count: usize,
) -> anyhow::Result<()> {
    let mut discard = vec![0u8; count];
    stream.read_exact(&mut discard).await?;

    Ok(())
}

/// The reply codes worth saying in words. Anything else is reported as itself.
fn refusal(code: u8) -> String {
    match code {
        1 => "general failure".to_owned(),
        2 => "not allowed by ruleset".to_owned(),
        3 => "network unreachable".to_owned(),
        4 => "host unreachable".to_owned(),
        5 => "connection refused".to_owned(),
        6 => "time to live expired".to_owned(),
        7 => "command not supported".to_owned(),
        8 => "address type not supported".to_owned(),
        other => format!("reply code {other}"),
    }
}
