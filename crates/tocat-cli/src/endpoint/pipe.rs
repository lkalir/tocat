//! pipe.rs: `pipe:` (alias `fifo:`).
//!
//! Two things here are load-bearing. `hold`, on by default, opens the FIFO
//! read-write even on the writing side, so tocat is its own peer: the open
//! never blocks and the stream never ends, which is what a log or event pipe
//! wants when its producers come and go. POSIX leaves read-write on a FIFO
//! undefined, but every platform tocat targets implements it, and there is no
//! other way to hold one open. Without `hold` the open blocks until a peer
//! appears, hence the warning: a hang that was asked for should not look like
//! a hang that was not.
//!
//! And `unlink` hands back a [`PathGuard`], which removes the FIFO when it
//! drops. It has to outlive the connection.
//!
//! Both halves of the endpoint are opened from the same descriptor but exposed
//! half-duplex on purpose. A held FIFO is readable *and* writable, so treating
//! it as duplex would feed our own writes straight back into our reader.

use std::os::unix::fs::FileTypeExt;

use anyhow::Context;
use rustix::fs::Mode as FileMode;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tracing::warn;

use crate::{
    config::ByteSize,
    endpoint::{
        Connection, Direction, EndpointStream, SyncHalves,
        parse::{Opt, ParseEndpointError},
        sys::{Mode, PathGuard, default_true, size_if_pipe},
    },
};

#[derive(Debug, Deserialize, Serialize)]
pub struct Pipe {
    pub path: std::path::PathBuf,
    /// `mkfifo` the path if it is missing.
    #[serde(default = "default_true")]
    pub create: bool,
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Remove the FIFO when the relay finishes.
    #[serde(default)]
    pub unlink: bool,
    /// Hold the FIFO open across producers.
    ///
    /// With `hold` (the default) tocat opens read-write, so it is its own
    /// writer: opening never blocks and the stream never ends, which is
    /// what you want for a log or event pipe whose producers come and go.
    /// Without it, a source blocks until a writer appears and sees EOF when
    /// the last one leaves: one producer, then done.
    #[serde(default = "default_true")]
    pub hold: bool,
    /// Kernel FIFO capacity. Linux only, best-effort, and unrelated to the
    /// global `buffer-size`: this one decides when the producer blocks.
    #[serde(default)]
    pub size: Option<ByteSize>,
    #[serde(default)]
    pub name: Option<String>,
}

impl Pipe {
    const SCHEME: &'static str = "pipe";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        // `create` and `hold` are the two that are on unless turned off, which
        // is why they are the defaults here and in the serde attributes above.
        let mut create = true;
        let mut hold = true;
        let mut mode = None;
        let mut unlink = false;
        let mut size = None;
        let mut name = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "create" => create = opt.flag()?,
                "hold" => hold = opt.flag()?,
                "mode" => mode = Some(opt.mode()?),
                "name" => name = Some(opt.string()?),
                "size" | "pipesize" => size = Some(opt.size()?),
                "unlink" => unlink = opt.flag()?,
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            path: std::path::PathBuf::from(body),
            create,
            mode,
            unlink,
            hold,
            size,
            name,
        })
    }

    pub(super) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("pipe://{}", self.path.display()))
    }

    /// Which access the FIFO is opened with. See the module docs for why `hold`
    /// opens read-write even on the writing side.
    fn access(&self, dir: Direction) -> (bool, bool) {
        match (self.hold, dir) {
            (true, _) => (true, true),
            (false, Direction::Source) => (true, false),
            (false, Direction::Sink) => (false, true),
        }
    }

    /// Create the FIFO if it is missing, and refuse anything that is not one.
    fn ensure(&self) -> anyhow::Result<()> {
        let path: &std::path::Path = &self.path;

        match std::fs::metadata(path) {
            Ok(meta) => {
                anyhow::ensure!(
                    meta.file_type().is_fifo(),
                    "{} exists and is not a FIFO",
                    path.display()
                );
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(
                    self.create,
                    "{} does not exist; pass `create` to make it",
                    path.display()
                );
            }
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
        }

        match rustix::fs::mkfifoat(rustix::fs::CWD, path, FileMode::from_bits_truncate(0o666)) {
            Ok(()) => {}
            // A racing producer may have won; anything else is fatal.
            Err(e) if e == rustix::io::Errno::EXIST => {}
            Err(e) => return Err(e).with_context(|| format!("mkfifo {}", path.display())),
        }

        if let Some(mode) = self.mode {
            mode.apply(path)?;
        }

        Ok(())
    }

    /// The guard that removes the FIFO on drop, when `unlink` asked for one.
    fn guard(&self) -> Option<PathGuard> {
        self.unlink.then(|| PathGuard(self.path.clone()))
    }

    /// Best-effort resize of the kernel buffer, if `size` was given.
    fn resize<F: std::os::fd::AsFd>(&self, fd: &F) {
        if let Some(size) = self.size {
            size_if_pipe(fd, &self.path.display().to_string(), size.bytes());
        }
    }

    pub(super) async fn connect(&self, dir: Direction) -> anyhow::Result<Connection> {
        self.ensure()?;

        let (read, write) = self.access(dir);

        if !self.hold {
            warn!(path = %self.path.display(), "FIFO without `hold`: open blocks until a peer connects");
        }

        let file = tokio::fs::OpenOptions::new()
            .read(read)
            .write(write)
            .open(&self.path)
            .await
            .with_context(|| format!("opening {}", self.path.display()))?;

        self.resize(&file);

        let stream = match dir {
            Direction::Source => EndpointStream::read_only(file),
            Direction::Sink => EndpointStream::write_only(file),
        };

        Ok(stream.into_connection_with_guard(self.guard()))
    }

    pub(super) fn connect_sync(&self, dir: Direction) -> anyhow::Result<SyncHalves> {
        self.ensure()?;

        let (read, write) = self.access(dir);
        let file = std::fs::OpenOptions::new()
            .read(read)
            .write(write)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;

        self.resize(&file);

        let guard = self.guard();

        Ok(match dir {
            Direction::Source => SyncHalves {
                reader: Some(Box::new(file)),
                writer: None,
                guard,
            },
            Direction::Sink => SyncHalves {
                reader: None,
                writer: Some(Box::new(file)),
                guard,
            },
        })
    }
}
