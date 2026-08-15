//! pty.rs: `pty:` and `pty-exec:`.
//!
//! Both allocate a pseudo-terminal and relay its master side, which is a
//! duplex byte stream and needs no adapter. They differ in what ends up on the
//! slave side: `pty-exec:` spawns a child there, `pty:` publishes it as a
//! device path and waits for something else to open it.
//!
//! # Why a pty rather than a pipe
//!
//! [`Exec`](super::Exec) already relays a child's stdin and stdout, so the pty
//! is only worth its cost when a program behaves differently on a terminal.
//! Three things change, and all three are the reason someone reaches for this:
//! libc line-buffers stdout instead of block-buffering it, so output arrives a
//! line at a time rather than 4KiB at a time; the kernel provides a line
//! discipline, so line editing and job control work; and programs that refuse
//! to run without a terminal run.
//!
//! # The line discipline eats bytes
//!
//! A pty in canonical mode is not a transparent pipe. It translates carriage
//! returns, turns `^C` into a signal rather than a byte, and refuses a line
//! longer than 4096 bytes. Relaying anything but text through it is corruption,
//! which is why `raw` is the default here and cooked mode is the thing you ask
//! for. Echo is off for the same reason: a relay that echoes writes back at the
//! reader is a loop, not a feature.
//!
//! # Layout
//!
//! [`open`] is the shared half: allocate, apply the window size, apply the
//! termios flags. What each scheme does with the slave is the only difference,
//! and it lives in the two `connect` methods.

use std::{os::fd::AsFd, path::PathBuf, str::FromStr};

use anyhow::Context;
use rustix::termios::{
    LocalModes, OptionalActions, Termios, Winsize, tcgetattr, tcsetattr, tcsetwinsize,
};
use serde::{Deserialize, Serialize, Serializer, de::Error as _};
use tocat_api::normalize;
use tracing::{info, warn};

use crate::{
    child,
    endpoint::{
        Connection, EndpointStream,
        parse::{Opt, ParseEndpointError},
        sys::PathGuard,
    },
};

/// Rows and columns, as the config file writes them: `size = "24x80"`.
///
/// A pty starts at 0x0, which most full-screen programs read as "no idea" and
/// some read as a terminal one character wide. Nothing here is relayed, so
/// there is no correct value to infer: an unset size is left at whatever the
/// kernel gave, and the program is left to its own default.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
}

impl FromStr for WinSize {
    type Err = ParseEndpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || ParseEndpointError::InvalidSize(s.to_string());
        let (rows, cols) = s.split_once(['x', 'X']).ok_or_else(invalid)?;

        Ok(Self {
            rows: rows.trim().parse().map_err(|_| invalid())?,
            cols: cols.trim().parse().map_err(|_| invalid())?,
        })
    }
}

impl<'de> Deserialize<'de> for WinSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse()
            .map_err(|_| D::Error::custom(format!("invalid terminal size {s:?}, want ROWSxCOLS")))
    }
}

impl Serialize for WinSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{}x{}", self.rows, self.cols))
    }
}

/// The terminal settings both schemes share.
///
/// Held as its own type for the parsing and the applying, but spelled out
/// field by field in each spec rather than flattened into it: `flatten` and
/// the internally tagged `EndpointSpec` both buffer, and combining them is a
/// known serde trap. Six duplicated lines are cheaper than that.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Terminal {
    /// Pass bytes through untouched. On by default: see the module docs.
    pub raw: bool,
    /// Echo input back at the writer. Off by default, since on a relay that is
    /// a loop.
    pub echo: bool,
    pub size: Option<WinSize>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            raw: true,
            echo: false,
            size: None,
        }
    }
}

impl Terminal {
    /// The option keys every terminal scheme accepts. Returns whether the key
    /// was one of them, so a scheme can fall through to its own.
    pub(super) fn parse_opt(
        &mut self,
        opt: &Opt<'_>,
        key: &str,
    ) -> Result<bool, ParseEndpointError> {
        match key {
            "raw" => self.raw = opt.flag()?,
            "echo" => self.echo = opt.flag()?,
            "size" => self.size = Some(opt.text()?.parse()?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// Fold these settings into a `Termios` the caller has already read.
    ///
    /// Separate from writing it back so that a scheme with more to say can add
    /// its own before the single `tcsetattr`, and so that a scheme that has to
    /// keep the original can take a copy first.
    pub(super) fn fill(self, termios: &mut Termios) {
        if self.raw {
            termios.make_raw();
        }

        // After `make_raw`, which clears it: the configured value wins.
        termios.local_modes.set(LocalModes::ECHO, self.echo);
    }

    /// Set the window size on an open terminal, if one was asked for.
    pub(super) fn resize<F: AsFd>(self, fd: F) -> anyhow::Result<()> {
        let Some(size) = self.size else {
            return Ok(());
        };

        tcsetwinsize(
            fd,
            Winsize {
                ws_row: size.rows,
                ws_col: size.cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .context("setting the terminal size")
    }

    /// Read, fold, write. What a scheme with nothing else to configure wants.
    ///
    /// Both calls go to the master fd and land on the pair: a pty's termios
    /// and window size belong to the line discipline between the two ends
    /// rather than to either side of it.
    fn apply(self, pty: &pty_process::Pty) -> anyhow::Result<()> {
        self.resize(pty.as_fd())?;

        let mut termios = tcgetattr(pty.as_fd()).context("reading the terminal settings")?;
        self.fill(&mut termios);

        // `Now` rather than `Flush`: nothing has been written yet, so there is
        // no pending output to drain and no pending input worth discarding.
        tcsetattr(pty.as_fd(), OptionalActions::Now, &termios)
            .context("applying the terminal settings")
    }
}

/// Allocate a pty and apply `terminal` to it.
fn open(terminal: Terminal) -> anyhow::Result<(pty_process::Pty, pty_process::Pts)> {
    let (pty, pts) = pty_process::open().context("allocating a pty")?;
    terminal.apply(&pty)?;
    Ok((pty, pts))
}

/// The slave device path, for logging and for `link`.
fn pts_path(pty: &pty_process::Pty) -> anyhow::Result<PathBuf> {
    let name = rustix::pty::ptsname(pty.as_fd(), Vec::new()).context("reading the pts name")?;
    let name = String::from_utf8(name.into_bytes()).context("pts name is not utf-8")?;

    Ok(PathBuf::from(name))
}

/// A pty whose slave nothing is spawned on.
#[derive(Debug, Deserialize, Serialize)]
pub struct Pty {
    /// A symlink to create pointing at the slave device, removed on drop.
    /// Without one the path is only logged, which is fine interactively and
    /// useless from a script.
    #[serde(default)]
    pub link: Option<PathBuf>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "crate::endpoint::sys::default_true")]
    pub raw: bool,
    #[serde(default)]
    pub echo: bool,
    #[serde(default)]
    pub size: Option<WinSize>,
}

/// A child spawned on the slave, with the pty as its controlling terminal.
#[derive(Debug, Deserialize, Serialize)]
pub struct PtyExec {
    pub argv: Vec<String>,
    /// Hand the whole command line to `$SHELL -c` instead of running it
    /// directly. The `system:` half of the `exec:`/`system:` pair, as a flag,
    /// because it is the only thing that separated them.
    #[serde(default)]
    pub shell: bool,
    /// What to set `TERM` to for the child. Unset leaves the relay's own
    /// value, which is usually what a program should see.
    #[serde(default)]
    pub term: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "crate::endpoint::sys::default_true")]
    pub raw: bool,
    #[serde(default)]
    pub echo: bool,
    #[serde(default)]
    pub size: Option<WinSize>,
}

impl Pty {
    const SCHEME: &'static str = "pty";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut name = None;
        let mut terminal = Terminal::default();

        // The body is the link path, so `pty:/tmp/tty0` needs no option at
        // all. `link=` is accepted too, for the table form's benefit.
        let mut link = (!body.is_empty()).then(|| PathBuf::from(body));

        for opt in opts {
            let key = normalize(opt.key);

            if terminal.parse_opt(&opt, key.as_str())? {
                continue;
            }

            match key.as_str() {
                "link" => link = Some(PathBuf::from(opt.text()?)),
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            link,
            name,
            raw: terminal.raw,
            echo: terminal.echo,
            size: terminal.size,
        })
    }

    fn terminal(&self) -> Terminal {
        Terminal {
            raw: self.raw,
            echo: self.echo,
            size: self.size,
        }
    }

    pub(super) fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| match &self.link {
            Some(link) => format!("pty://{}", link.display()),
            None => "pty://".to_string(),
        })
    }

    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let (pty, pts) = open(self.terminal())?;
        let path = pts_path(&pty)?;

        // Nothing is spawned here, so the slave has no other holder. Dropping
        // it would leave the master readable but hung up, and the relay would
        // see end of stream before a peer ever arrived, so it rides along on
        // the connection's `keepalive` and closes with it.
        let stream = EndpointStream::Duplex(Box::new(pty));

        let Some(link) = &self.link else {
            info!(pts = %path.display(), "pty allocated");
            return Ok(stream.into_connection().with_keepalive(pts));
        };

        // Replacing a live link would steal a path something else is using, so
        // only a dangling one is cleared: a symlink whose target is gone.
        if link.is_symlink() && !link.exists() {
            warn!(link = %link.display(), "removing a dangling link");
            let _ = std::fs::remove_file(link);
        }

        std::os::unix::fs::symlink(&path, link)
            .with_context(|| format!("linking {} to {}", link.display(), path.display()))?;

        info!(link = %link.display(), pts = %path.display(), "pty allocated");

        Ok(stream
            .into_connection_with_guard(Some(PathGuard(link.clone())))
            .with_keepalive(pts))
    }
}

impl PtyExec {
    const SCHEME: &'static str = "pty-exec";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut shell = false;
        let mut term = None;
        let mut name = None;
        let mut terminal = Terminal::default();

        for opt in opts {
            let key = normalize(opt.key);

            if terminal.parse_opt(&opt, key.as_str())? {
                continue;
            }

            match key.as_str() {
                "shell" => shell = opt.flag()?,
                "term" => term = Some(opt.string()?),
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        // Under `shell` the body is one command line for `sh -c`, so splitting
        // it would break every quote in it.
        let argv: Vec<String> = if shell {
            vec![body.to_string()]
        } else {
            body.split_whitespace().map(String::from).collect()
        };

        if argv.is_empty() || argv[0].is_empty() {
            return Err(ParseEndpointError::Empty);
        }

        Ok(Self {
            argv,
            shell,
            term,
            name,
            raw: terminal.raw,
            echo: terminal.echo,
            size: terminal.size,
        })
    }

    fn terminal(&self) -> Terminal {
        Terminal {
            raw: self.raw,
            echo: self.echo,
            size: self.size,
        }
    }

    /// As with `exec:`, an explicit `name` does not replace the label: the
    /// command line is what identifies the endpoint in a log.
    pub(super) fn label(&self) -> String {
        format!("PTY-EXEC({})", self.argv.join(" "))
    }

    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let (pty, pts) = open(self.terminal())?;

        let mut command = if self.shell {
            let sh = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
            pty_process::Command::new(sh).arg("-c").arg(&self.argv[0])
        } else {
            pty_process::Command::new(&self.argv[0]).args(&self.argv[1..])
        };

        if let Some(term) = &self.term {
            command = command.env("TERM", term);
        }

        // The pty is the child's stdin, stdout *and* stderr, which is the one
        // way this differs from `exec:`. There is no second descriptor to
        // inherit: on a terminal, diagnostics are part of what the terminal
        // shows, so they are relayed with everything else.
        let child = command
            .kill_on_drop(true)
            .spawn(pts)
            .with_context(|| format!("spawning {} on a pty", self.argv.join(" ")))?;

        child::reap_in_background(child);

        Ok(EndpointStream::Duplex(Box::new(pty)).into_connection())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointSpec;

    fn spec(s: &str) -> EndpointSpec {
        s.parse().expect("parses")
    }

    #[test]
    fn a_size_is_rows_by_columns() {
        assert_eq!(
            "24x80".parse::<WinSize>(),
            Ok(WinSize { rows: 24, cols: 80 })
        );
        assert!("24".parse::<WinSize>().is_err());
        assert!("24x".parse::<WinSize>().is_err());
        assert!("axb".parse::<WinSize>().is_err());
    }

    /// Raw on and echo off, because a relay is not a terminal emulator.
    #[test]
    fn the_defaults_are_transparent() {
        let EndpointSpec::PtyExec(e) = spec("pty-exec:cat") else {
            panic!("wrong variant");
        };

        assert!(e.raw);
        assert!(!e.echo);
        assert_eq!(e.size, None);
    }

    #[test]
    fn the_body_is_the_link_path() {
        let EndpointSpec::Pty(e) = spec("pty:/tmp/ttyfake") else {
            panic!("wrong variant");
        };

        assert_eq!(e.link, Some(PathBuf::from("/tmp/ttyfake")));
    }

    /// Splitting a shell command line would break every quote in it.
    #[test]
    fn shell_keeps_the_command_line_whole() {
        let EndpointSpec::PtyExec(e) = spec("pty-exec:echo 'a b',shell") else {
            panic!("wrong variant");
        };

        assert_eq!(e.argv, vec!["echo 'a b'".to_string()]);

        let EndpointSpec::PtyExec(e) = spec("pty-exec:echo a b") else {
            panic!("wrong variant");
        };

        assert_eq!(e.argv, vec!["echo", "a", "b"]);
    }

    #[test]
    fn an_option_the_scheme_does_not_take_is_an_error() {
        assert!("pty:,fork".parse::<EndpointSpec>().is_err());
        assert!("pty-exec:cat,link=/tmp/x".parse::<EndpointSpec>().is_err());
    }

    #[test]
    fn an_empty_command_is_rejected() {
        assert!(matches!(
            "pty-exec:".parse::<EndpointSpec>(),
            Err(ParseEndpointError::Empty)
        ));
    }
}
