//! sockopt.rs: the socket options every socket scheme shares.
//!
//! One grammar and one application, so that `tcp:`, `unix:` and `udp:` cannot
//! drift apart in spelling or in behaviour. A scheme calls [`SocketOptions::
//! option`] from the catch-all arm of its own option match, which answers
//! whether the key belonged here; anything left over is still that scheme's
//! error to report.
//!
//! Names describe what is being controlled rather than the constant being set,
//! and values carry units, because these appear in a TOML file as often as on
//! a command line and `recv-buffer = "256KiB"` is configuration where
//! `rcvbuf = 262144` is a register write. socat's spellings are aliases: the
//! key goes through [`normalize`], so case, hyphens and underscores are already
//! one key and an alias costs a pattern.
//!
//! # When each option can be set
//!
//! Two moments, and mixing them up is silent rather than loud. `reuseaddr` and
//! the buffer sizes have to be set on the socket **before** it binds or
//! connects: after the fact the first is meaningless and the second no longer
//! affects the window that was already negotiated. Everything else is set on
//! the open connection. [`apply_before`] and [`apply_after`] are that split,
//! and a scheme that only calls one of them will find half its options quietly
//! doing nothing.
//!
//! # Things that are easy to lose in a refactor
//!
//! A forked listener accepts many connections and each one is a new socket.
//! The per-connection options belong to every accepted stream, not just to the
//! one the unforked path takes.

use std::{os::fd::AsFd, time::Duration};

use serde::{Deserialize, Serialize};
use tocat_api::{ByteSize, Interval, normalize};

use crate::endpoint::parse::{Opt, ParseEndpointError};

/// Connections the kernel queues before a listener has accepted them, when
/// `backlog=` says nothing. Matches the usual `somaxconn`.
///
/// Not `max-connections`, which bounds how many this relay serves at once: a
/// short backlog turns a burst of clients into refused connections whether or
/// not the relay would have got to them.
pub(in crate::endpoint) const DEFAULT_BACKLOG: u32 = 1024;

/// What a scheme's socket can actually carry.
///
/// A key that is real but wrong for the family falls through to the scheme's
/// own error, so `unix:...,nagle=false` reads as an unsupported option for
/// `unix` rather than as an option that parsed and then did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Tcp,
    UnixStream,
    Datagram,
}

impl Family {
    fn is_tcp(self) -> bool {
        self == Family::Tcp
    }

    fn is_stream(self) -> bool {
        matches!(self, Family::Tcp | Family::UnixStream)
    }

    /// A unix socket has no address to be in `TIME_WAIT`.
    fn allows_reuseaddr(self) -> bool {
        !matches!(self, Family::UnixStream)
    }
}

/// Keepalive, which is a flag and a duration in one option: bare turns it on
/// with the system's idle time, a duration turns it on and sets that idle time.
///
/// Untagged so that `keepalive = true` and `keepalive = "30s"` both
/// deserialize, matching what the command line accepts.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Keepalive {
    Enabled(bool),
    Idle(Interval),
}

impl Keepalive {
    fn enabled(self) -> bool {
        match self {
            Keepalive::Enabled(on) => on,
            Keepalive::Idle(_) => true,
        }
    }

    fn idle(self) -> Option<Duration> {
        match self {
            Keepalive::Enabled(_) => None,
            Keepalive::Idle(interval) => Some(interval.duration()),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct SocketOptions {
    /// Reuse an address still in `TIME_WAIT`. Keeps socat's spelling despite
    /// naming the constant rather than the behaviour: every better name I
    /// tried described it less accurately, and the people who reach for it
    /// know it by this one.
    pub reuseaddr: bool,

    /// Nagle's algorithm, on by default in the kernel. `nagle=false` is
    /// socat's `nodelay`, said without the double negative.
    pub nagle: Option<bool>,

    pub keepalive: Option<Keepalive>,

    /// Between keepalive probes once the idle time has passed.
    pub keepalive_interval: Option<Interval>,

    /// Unanswered probes before the connection is considered dead.
    pub keepalive_probes: Option<u32>,

    /// Kernel socket buffers. Not to be confused with `buffer-size`, which is
    /// the relay's own copy buffer and a different thing entirely.
    pub recv_buffer: Option<ByteSize>,
    pub send_buffer: Option<ByteSize>,

    /// How long `close` waits for unsent data. `linger=0` discards it and
    /// closes with an RST, which is a way to test a peer's error handling and
    /// a way to lose the tail of a stream, depending on which you meant.
    pub linger: Option<Interval>,
}

impl SocketOptions {
    /// Take `opt` if it names one of these, and say whether it did.
    ///
    /// `Ok(false)` means the key is not a socket option and the caller should
    /// fall through to its own error, so that an unsupported option still
    /// names the scheme that rejected it.
    pub(in crate::endpoint) fn option(
        &mut self,
        opt: &Opt<'_>,
        family: Family,
    ) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "reuseaddr" | "soreuseaddr" if family.allows_reuseaddr() => {
                self.reuseaddr = opt.flag()?
            }
            "nagle" if family.is_tcp() => self.nagle = Some(opt.flag()?),
            "nodelay" | "tcpnodelay" if family.is_tcp() => self.nagle = Some(!opt.flag()?),
            "keepalive" | "sokeepalive" if family.is_tcp() => {
                self.keepalive = Some(keepalive(opt)?)
            }
            "keepaliveinterval" | "keepintvl" | "tcpkeepintvl" if family.is_tcp() => {
                self.keepalive_interval = Some(interval(opt)?);
            }
            "keepaliveprobes" | "keepcnt" | "tcpkeepcnt" if family.is_tcp() => {
                self.keepalive_probes = Some(opt.count()?.get() as u32);
            }
            "linger" | "solinger" if family.is_stream() => self.linger = Some(interval(opt)?),
            "recvbuffer" | "rcvbuf" | "sorcvbuf" => self.recv_buffer = Some(opt.size()?),
            "sendbuffer" | "sndbuf" | "sosndbuf" => self.send_buffer = Some(opt.size()?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// Whether anything here has to be set before the socket is bound or
    /// connected, which is what decides whether a scheme needs a [`TcpSocket`]
    /// rather than a plain connect.
    pub(in crate::endpoint) fn needs_socket(&self) -> bool {
        self.reuseaddr || self.recv_buffer.is_some() || self.send_buffer.is_some()
    }

    /// The options that only mean something before bind or connect.
    ///
    /// Over a descriptor rather than a socket so that the schemes which build
    /// their socket by hand, because their tokio constructor creates and
    /// binds in one step, can call the same thing.
    pub(in crate::endpoint) fn apply_before(&self, fd: impl AsFd) -> std::io::Result<()> {
        use rustix::net::sockopt;

        let fd = fd.as_fd();

        if self.reuseaddr {
            sockopt::set_socket_reuseaddr(fd, true)?;
        }

        if let Some(size) = self.recv_buffer {
            sockopt::set_socket_recv_buffer_size(fd, size.bytes())?;
        }

        if let Some(size) = self.send_buffer {
            sockopt::set_socket_send_buffer_size(fd, size.bytes())?;
        }

        Ok(())
    }

    /// The options that belong to an open socket.
    pub fn apply(&self, fd: impl AsFd) -> std::io::Result<()> {
        use rustix::net::sockopt;

        let fd = fd.as_fd();

        if let Some(nagle) = self.nagle {
            sockopt::set_tcp_nodelay(fd, !nagle)?;
        }

        if let Some(linger) = self.linger {
            sockopt::set_socket_linger(fd, Some(linger.duration()))?;
        }

        if let Some(keepalive) = self.keepalive {
            sockopt::set_socket_keepalive(fd, keepalive.enabled())?;

            if let Some(idle) = keepalive.idle() {
                sockopt::set_tcp_keepidle(fd, idle)?;
            }
        }

        if let Some(interval) = self.keepalive_interval {
            sockopt::set_tcp_keepintvl(fd, interval.duration())?;
        }

        if let Some(probes) = self.keepalive_probes {
            sockopt::set_tcp_keepcnt(fd, probes)?;
        }

        if let Some(size) = self.recv_buffer {
            sockopt::set_socket_recv_buffer_size(fd, size.bytes())?;
        }

        if let Some(size) = self.send_buffer {
            sockopt::set_socket_send_buffer_size(fd, size.bytes())?;
        }

        Ok(())
    }
}

/// Bare is on; a value is either a flag or the idle time.
fn keepalive(opt: &Opt<'_>) -> Result<Keepalive, ParseEndpointError> {
    let Ok(flag) = opt.flag() else {
        return Ok(Keepalive::Idle(interval(opt)?));
    };

    Ok(Keepalive::Enabled(flag))
}

fn interval(opt: &Opt<'_>) -> Result<Interval, ParseEndpointError> {
    opt.text()?
        .parse()
        .map_err(|e| ParseEndpointError::InvalidInterval(format!("{e}")))
}
