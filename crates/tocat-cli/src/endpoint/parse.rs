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

use tocat_api::normalize;

use crate::{
    config::ByteSize,
    endpoint::{
        EndpointSpec,
        exec::{Exec, System},
        file::File,
        pipe::Pipe,
        pty::{Pty, PtyExec},
        stdio::Stdio,
        sys::Mode,
        tcp::{Tcp, TcpListen},
        tty::Tty,
        udp::{Udp, UdpListen},
        unix::{Unix, UnixListen},
    },
};

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
            return Ok(Self::Stdio(Stdio { name: None }));
        }

        let mut parts = s.split(',');
        let target = parts.next().unwrap_or("");
        let opts = options(parts);

        let (scheme, body) = target.split_once(':').unwrap_or((target, ""));

        match normalize(scheme).as_str() {
            "exec" => Exec::parse(body, opts).map(Self::Exec),
            "file" | "open" => File::parse(body, opts).map(Self::File),
            "pipe" | "fifo" => Pipe::parse(body, opts).map(Self::Pipe),
            "pty" => Pty::parse(body, opts).map(Self::Pty),
            "ptyexec" => PtyExec::parse(body, opts).map(Self::PtyExec),
            "stdio" => Stdio::parse(body, opts).map(Self::Stdio),
            "system" => System::parse(body, opts).map(Self::System),
            "tcp" | "tcpconnect" | "connect" => Tcp::parse(body, opts).map(Self::Tcp),
            "tcplisten" | "listen" => TcpListen::parse(body, opts).map(Self::TcpListen),
            "tty" | "serial" => Tty::parse(body, opts).map(Self::Tty),
            "udp" | "udpconnect" => Udp::parse(body, opts).map(Self::Udp),
            "udplisten" => UdpListen::parse(body, opts).map(Self::UdpListen),
            "unix" | "unixconnect" | "uds" | "udsconnect" => {
                Unix::parse(body, opts).map(Self::Unix)
            }
            "unixlisten" | "udslisten" => UnixListen::parse(body, opts).map(Self::UnixListen),
            other => Err(Self::Err::UnknownScheme(other.to_owned())),
        }
    }
}
