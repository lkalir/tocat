//! parse.rs: the compact CLI endpoint grammar.
//!
//! `scheme:body,opt,opt=value`. This file owns only what is common to every
//! scheme: splitting the tail into [`Opt`]s, turning one into a flag, a size or
//! a mode, and the error type. Which keys are legal, what they default to and
//! what they mean is each scheme's own business, next to the fields they set.
//!
//! An option the scheme does not accept is an error. `tcp:80,append` used to
//! parse and then silently do nothing, because options were collected before
//! the scheme was known and every scheme read only the fields it cared about.
//! Now each scheme matches on its own keys and rejects the rest, so a
//! misplaced option is reported instead of ignored. Aliases (`trunc` for
//! `truncate`, `pipe-size` for `size`) live with the scheme that accepts them.

use std::num::NonZeroUsize;

use tocat_api::{ByteSize, normalize};

use crate::endpoint::{
    EndpointSpec, LayerSpec, Noise, Proxy, Socks, Tls, Transport, Ws,
    chan::Chan,
    exec::{Exec, System},
    file::File,
    layer::{proxy::split_target, ws::split_path},
    pipe::Pipe,
    pty::{Pty, PtyExec},
    stdio::Stdio,
    sys::Mode,
    tcp::{Tcp, TcpListen},
    tty::Tty,
    udp::{Udp, UdpListen},
    unix::{
        Unix, UnixListen,
        dgram::{UnixDgram, UnixDgramListen},
        seqpacket::{UnixSeqpacket, UnixSeqpacketListen},
    },
};

/// Split one option list between a layer and the transport under it.
///
/// The layer is asked first. The only key both could want is `name`, which a
/// layer refuses so that an endpoint has one name rather than one per lefel, so
/// asking in this order cannot take an option from the transport.
fn split_tls<'a>(
    opts: impl Iterator<Item = Opt<'a>>,
) -> Result<(Vec<Opt<'a>>, Tls), ParseEndpointError> {
    let mut tls = Tls::default();
    let mut rest = Vec::new();

    for opt in opts {
        if !tls.option(&opt)? {
            rest.push(opt);
        }
    }

    Ok((rest, tls))
}

/// The same as [split_tls], for the Noise layer
fn split_noise<'a>(
    opts: impl Iterator<Item = Opt<'a>>,
) -> Result<(Vec<Opt<'a>>, Noise), ParseEndpointError> {
    let mut noise = Noise::default();
    let mut rest = Vec::new();

    for opt in opts {
        if !noise.option(&opt)? {
            rest.push(opt);
        }
    }

    Ok((rest, noise))
}

/// The same as [split_tls], for the WebSocket layer
fn split_ws<'a>(
    opts: impl Iterator<Item = Opt<'a>>,
) -> Result<(Vec<Opt<'a>>, Ws), ParseEndpointError> {
    let mut ws = Ws::default();
    let mut rest = Vec::new();

    for opt in opts {
        if !ws.option(&opt)? {
            rest.push(opt);
        }
    }

    Ok((rest, ws))
}

/// The same as [split_tls], for the PROXY layer
fn split_proxy<'a>(
    opts: impl Iterator<Item = Opt<'a>>,
) -> Result<(Vec<Opt<'a>>, Proxy), ParseEndpointError> {
    let mut proxy = Proxy::default();
    let mut rest = Vec::new();

    for opt in opts {
        if !proxy.option(&opt)? {
            rest.push(opt);
        }
    }

    Ok((rest, proxy))
}

/// The same as [split_tls], for the SOCKS5 layer
fn split_socks<'a>(
    opts: impl Iterator<Item = Opt<'a>>,
) -> Result<(Vec<Opt<'a>>, Socks), ParseEndpointError> {
    let mut socks = Socks::default();
    let mut rest = Vec::new();

    for opt in opts {
        if !socks.option(&opt)? {
            rest.push(opt);
        }
    }

    Ok((rest, socks))
}

#[derive(Debug, PartialEq)]
pub enum ParseEndpointError {
    Empty,
    UnknownScheme(String),
    UnsupportedOption {
        scheme: &'static str,
        option: String,
    },
    /// Two options that parse but cannot both mean anything.
    Conflict {
        scheme: &'static str,
        reason: &'static str,
    },
    InvalidPort(String),
    InvalidMode(String),
    InvalidSize(String),
    InvalidInterval(String),
    InvalidFlag(String),
    MissingValue(String),
    InvalidNumber(String),
}

impl std::fmt::Display for ParseEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseEndpointError::Empty => write!(f, "empty endpoint"),
            ParseEndpointError::UnknownScheme(body) => write!(f, "unknown scheme: {body}"),
            ParseEndpointError::UnsupportedOption { scheme, option } => {
                write!(f, "unsupported option for {scheme}: {option}")
            }
            ParseEndpointError::Conflict { scheme, reason } => {
                write!(f, "contradictory options for {scheme}: {reason}")
            }
            ParseEndpointError::InvalidPort(body) => write!(f, "invalid port: {body}"),
            ParseEndpointError::InvalidMode(body) => write!(f, "invalid permissions: {body}"),
            ParseEndpointError::InvalidSize(body) => write!(f, "invalid size: {body}"),
            ParseEndpointError::InvalidInterval(body) => write!(f, "invalid interval: {body}"),
            ParseEndpointError::InvalidFlag(body) => write!(f, "invalid flag: {body}"),
            ParseEndpointError::MissingValue(body) => write!(f, "missing value: {body}"),
            ParseEndpointError::InvalidNumber(body) => write!(f, "invalid number: {body}"),
        }
    }
}

impl std::error::Error for ParseEndpointError {}

/// One `key` or `key=value` from an endpoint's option list.
///
/// The key is public so a scheme can match on it; the value is reached through
/// the accessors below, which is where a missing or malformed one becomes an
/// error naming the key that carried it.
pub(super) struct Opt<'a> {
    pub(super) key: &'a str,
    value: Option<&'a str>,
}

impl<'a> Opt<'a> {
    /// A bare key means true, so `fork` and `fork=true` are the same thing.
    pub(super) fn flag(&self) -> Result<bool, ParseEndpointError> {
        match self.value {
            None => Ok(true),
            Some(v) => v
                .parse()
                .map_err(|_| ParseEndpointError::InvalidFlag(v.to_string())),
        }
    }

    /// The value of an option that requires one.
    pub(super) fn text(&self) -> Result<&'a str, ParseEndpointError> {
        self.value
            .ok_or_else(|| ParseEndpointError::MissingValue(self.key.to_string()))
    }

    pub(super) fn string(&self) -> Result<String, ParseEndpointError> {
        self.text().map(str::to_string)
    }

    pub(super) fn size(&self) -> Result<ByteSize, ParseEndpointError> {
        self.text()?
            .parse()
            .map_err(|e| ParseEndpointError::InvalidSize(format!("{e}")))
    }

    pub(super) fn mode(&self) -> Result<Mode, ParseEndpointError> {
        self.text()?.parse()
    }

    pub(super) fn count(&self) -> Result<NonZeroUsize, ParseEndpointError> {
        self.text()?
            .parse()
            .map_err(|_| ParseEndpointError::InvalidNumber(self.key.to_string()))
    }

    /// The catch-all arm of a scheme's option match.
    pub(super) fn unsupported(&self, scheme: &'static str) -> ParseEndpointError {
        ParseEndpointError::UnsupportedOption {
            scheme,
            option: self.key.to_string(),
        }
    }
}

/// Split the comma-separated tail of an endpoint into options.
pub(super) fn options<'a>(parts: std::str::Split<'a, char>) -> impl Iterator<Item = Opt<'a>> {
    parts.map(|opt| match opt.split_once('=') {
        Some((key, value)) => Opt {
            key,
            value: Some(value),
        },
        None => Opt {
            key: opt,
            value: None,
        },
    })
}

/// Split `host:port`, `host`, or `port` into its parts, leaving the defaults to
/// the endpoint. Shared by the two listening socket schemes.
pub(super) fn host_port(body: &str) -> Result<(Option<String>, Option<u16>), ParseEndpointError> {
    let (host, port) = if body.is_empty() {
        (None, None)
    } else if let Some((h, p)) = body.rsplit_once(':') {
        let parsed_port = p
            .parse::<u16>()
            .map_err(|_| ParseEndpointError::InvalidPort(p.to_string()))?;
        let host_opt = if h.is_empty() {
            None
        } else {
            Some(h.to_string())
        };
        (host_opt, Some(parsed_port))
    } else if let Ok(parsed_port) = body.parse::<u16>() {
        (None, Some(parsed_port))
    } else {
        (Some(body.to_string()), None)
    };

    Ok((host, port))
}

impl std::str::FromStr for EndpointSpec {
    type Err = ParseEndpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(Self::Err::Empty);
        }

        if s == "-" {
            return Ok(Transport::Stdio(Stdio { name: None }).into());
        }

        let mut parts = s.split(',');
        let target = parts.next().unwrap_or("");
        let opts = options(parts);

        let (scheme, body) = target.split_once(':').unwrap_or((target, ""));

        // A sugared scheme is a transport and a stack in one word, so the layer
        // is built here and the transport gets what is left.
        let mut layers = Vec::new();

        let transport = match normalize(scheme).as_str() {
            "chan" | "channel" | "queue" => Chan::parse(body, opts).map(Transport::Chan),
            "exec" => Exec::parse(body, opts).map(Transport::Exec),
            "file" | "open" => File::parse(body, opts).map(Transport::File),
            "pipe" | "fifo" => Pipe::parse(body, opts).map(Transport::Pipe),
            "proxy" | "proxyconnect" | "httpproxy" => {
                let Some((addr, target)) = split_target(body) else {
                    return Err(ParseEndpointError::Empty);
                };

                let (rest, mut proxy) = split_proxy(opts)?;

                if proxy.target.is_empty() {
                    proxy.target = target.to_owned();
                }

                layers.push(LayerSpec::Proxy(proxy));

                Tcp::parse(addr, rest.into_iter()).map(Transport::Tcp)
            }
            "noise" | "noiseconnect" => {
                let (rest, noise) = split_noise(opts)?;
                layers.push(LayerSpec::Noise(noise));

                Tcp::parse(body, rest.into_iter()).map(Transport::Tcp)
            }
            "noiselisten" => {
                let (rest, noise) = split_noise(opts)?;
                layers.push(LayerSpec::Noise(noise));

                TcpListen::parse(body, rest.into_iter()).map(Transport::TcpListen)
            }
            "pty" => Pty::parse(body, opts).map(Transport::Pty),
            "ptyexec" => PtyExec::parse(body, opts).map(Transport::PtyExec),
            "socks5" | "socks" => {
                let Some((addr, target)) = split_target(body) else {
                    return Err(ParseEndpointError::Empty);
                };

                let (rest, mut socks) = split_socks(opts)?;

                if socks.target.is_empty() {
                    socks.target = target.to_owned();
                }

                layers.push(LayerSpec::Socks(socks));

                Tcp::parse(addr, rest.into_iter()).map(Transport::Tcp)
            }
            "stdio" => Stdio::parse(body, opts).map(Transport::Stdio),
            "system" => System::parse(body, opts).map(Transport::System),
            "tcp" | "tcpconnect" | "connect" => Tcp::parse(body, opts).map(Transport::Tcp),
            "tcplisten" | "listen" => TcpListen::parse(body, opts).map(Transport::TcpListen),
            "tty" | "serial" => Tty::parse(body, opts).map(Transport::Tty),
            "tls" | "ssl" | "openssl" | "tlsconnect" | "sslconnect" | "opensslconnect" => {
                let (rest, tls) = split_tls(opts)?;
                layers.push(LayerSpec::Tls(tls));

                Tcp::parse(body, rest.into_iter()).map(Transport::Tcp)
            }
            "tlslisten" | "ssllisten" | "openssllisten" => {
                let (rest, tls) = split_tls(opts)?;
                layers.push(LayerSpec::Tls(tls));

                TcpListen::parse(body, rest.into_iter()).map(Transport::TcpListen)
            }
            "udp" | "udpconnect" => Udp::parse(body, opts).map(Transport::Udp),
            "udplisten" => UdpListen::parse(body, opts).map(Transport::UdpListen),
            "unix" | "unixconnect" | "uds" | "udsconnect" => {
                Unix::parse(body, opts).map(Transport::Unix)
            }
            "unixdgram" | "unixdatagram" | "udsdgram" | "udsdatagram" => {
                UnixDgram::parse(body, opts).map(Transport::UnixDgram)
            }
            "unixdgramlisten" | "unixdatagramlisten" | "udsdgramlisten" | "udsdatagramlisten" => {
                UnixDgramListen::parse(body, opts).map(Transport::UnixDgramListen)
            }
            "unixlisten" | "udslisten" => UnixListen::parse(body, opts).map(Transport::UnixListen),
            "unixseqpacket" | "unixseqpkt" | "udsseqpacket" | "udsseqpkt" | "seqpacket"
            | "seqpkt" => UnixSeqpacket::parse(body, opts).map(Transport::UnixSeqpacket),
            "unixseqpacketlisten"
            | "unixseqpktlisten"
            | "udsseqpacketlisten"
            | "udsseqpktlisten"
            | "seqpacketlisten"
            | "seqpktlisten" => {
                UnixSeqpacketListen::parse(body, opts).map(Transport::UnixSeqpacketListen)
            }
            "ws" | "websocket" => {
                let (addr, path) = split_path(body);
                let (rest, mut ws) = split_ws(opts)?;
                ws.path = ws.path.or_else(|| path.map(|p| p.to_owned()));
                layers.push(LayerSpec::Ws(ws));

                Tcp::parse(addr, rest.into_iter()).map(Transport::Tcp)
            }
            "wslisten" | "websocketlisten" => {
                let (addr, path) = split_path(body);
                let (rest, mut ws) = split_ws(opts)?;
                ws.path = ws.path.or_else(|| path.map(|p| p.to_owned()));
                layers.push(LayerSpec::Ws(ws));

                TcpListen::parse(addr, rest.into_iter()).map(Transport::TcpListen)
            }
            "wss" | "websockets" => {
                let (addr, path) = split_path(body);
                let (rest, tls) = split_tls(opts.collect::<Vec<_>>().into_iter())?;
                let (rest, mut ws) = split_ws(rest.into_iter())?;
                ws.path = ws.path.or_else(|| path.map(|p| p.to_owned()));

                layers.push(LayerSpec::Tls(tls));
                layers.push(LayerSpec::Ws(ws));

                Tcp::parse(addr, rest.into_iter()).map(Transport::Tcp)
            }
            "wsslisten" | "websocketslisten" => {
                let (addr, path) = split_path(body);
                let (rest, tls) = split_tls(opts.collect::<Vec<_>>().into_iter())?;
                let (rest, mut ws) = split_ws(rest.into_iter())?;
                ws.path = ws.path.or_else(|| path.map(|p| p.to_owned()));

                layers.push(LayerSpec::Tls(tls));
                layers.push(LayerSpec::Ws(ws));

                TcpListen::parse(addr, rest.into_iter()).map(Transport::TcpListen)
            }
            other => Err(Self::Err::UnknownScheme(other.to_owned())),
        }?;

        Ok(EndpointSpec { transport, layers })
    }
}
