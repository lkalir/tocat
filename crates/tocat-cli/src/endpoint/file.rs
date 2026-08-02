//! file.rs: `file:` (alias `open:`).
//!
//! Half-duplex by role: the same spec opens for reading as a source and for
//! writing as a sink, which is why every open takes a [`Direction`]. The
//! writing options (`append`, `create`, `truncate`) are therefore ignored on
//! the source side, where the file is opened read-only.
//!
//! A `file:` pointed at a FIFO is the one-shot version of `pipe:`: the open
//! blocks until a peer appears and the stream ends when the last writer
//! leaves. That is a legitimate thing to want, so it warns rather than
//! refusing. `pipe:` is the form that outlives its producers.

use std::os::unix::fs::FileTypeExt;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::endpoint::{
    Connection, Direction, EndpointStream, SyncHalves,
    parse::{Opt, ParseEndpointError},
    sys::default_true,
};

#[derive(Debug, Deserialize, Serialize)]
pub struct File {
    pub path: std::path::PathBuf,
    #[serde(default)]
    pub append: bool,
    #[serde(default = "default_true")]
    pub create: bool,
    #[serde(default)]
    pub truncate: bool,
    #[serde(default)]
    pub name: Option<String>,
}

impl File {
    const SCHEME: &'static str = "file";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut append = false;
        // On unless turned off, as in the serde attribute above.
        let mut create = true;
        let mut truncate = false;
        let mut name = None;

        for opt in opts {
            match opt.key {
                "append" => append = opt.flag()?,
                "create" => create = opt.flag()?,
                "name" => name = Some(opt.string()?),
                "truncate" | "trunc" => truncate = opt.flag()?,
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            path: std::path::PathBuf::from(body),
            append,
            create,
            truncate,
            name,
        })
    }

    /// Unlike the other endpoints, an explicit `name` does not replace the
    /// label here: a file is identified by its path.
    pub(super) fn label(&self) -> String {
        format!("file://{}", self.path.display())
    }

    /// A source reads; a sink writes and honours `append`, `create` and
    /// `truncate`. `truncate` is dropped under `append`, where the two would
    /// contradict each other.
    ///
    /// Order is `read`, `create`, `append`, `truncate`.
    fn options(&self, dir: Direction) -> (bool, bool, bool, bool) {
        match dir {
            Direction::Source => (true, false, false, false),
            Direction::Sink => (
                false,
                self.create,
                self.append,
                self.truncate && !self.append,
            ),
        }
    }

    /// Warn if the path turned out to be a FIFO. See the module docs.
    fn warn_if_fifo(&self, kind: std::fs::FileType) {
        if kind.is_fifo() {
            warn!(path = %self.path.display(), "FIFO endpoint: open blocks until a peer connects");
        }
    }

    pub(super) async fn connect(&self, dir: Direction) -> anyhow::Result<Connection> {
        let (read, create, append, truncate) = self.options(dir);

        let file = tokio::fs::OpenOptions::new()
            .read(read)
            .write(!read)
            .create(create)
            .append(append)
            .truncate(truncate)
            .open(&self.path)
            .await
            .with_context(|| format!("opening {}", self.path.display()))?;

        self.warn_if_fifo(file.metadata().await?.file_type());

        Ok(match dir {
            Direction::Source => EndpointStream::read_only(file),
            Direction::Sink => EndpointStream::write_only(file),
        }
        .into_connection())
    }

    pub(super) fn connect_sync(&self, dir: Direction) -> anyhow::Result<SyncHalves> {
        let (read, create, append, truncate) = self.options(dir);

        let file = std::fs::OpenOptions::new()
            .read(read)
            .write(!read)
            .create(create)
            .append(append)
            .truncate(truncate)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;

        self.warn_if_fifo(file.metadata()?.file_type());

        Ok(match dir {
            Direction::Source => SyncHalves {
                reader: Some(Box::new(file)),
                writer: None,
                guard: None,
            },
            Direction::Sink => SyncHalves {
                reader: None,
                writer: Some(Box::new(file)),
                guard: None,
            },
        })
    }
}
