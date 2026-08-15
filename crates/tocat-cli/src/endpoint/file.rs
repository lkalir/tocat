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
//!
//! # Devices
//!
//! A block device or a plain character device is a file that happens to live
//! in `/dev`, so it belongs here rather than in a scheme of its own. `device`
//! is the assertion that it is one, which stops `create` turning a wrong path
//! into a regular file where a device should have been, and `seek` is what
//! makes the half of a device you meant reachable.
//!
//! A terminal is the exception, and it has [`tty`](super::tty): it is duplex
//! on one descriptor, which is a shape this module cannot produce, and its
//! settings have to be restored afterwards, which this module has nowhere to
//! keep.

use std::{
    io::Seek,
    os::unix::fs::{FileTypeExt, MetadataExt},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tocat_api::{ByteSize, normalize};
use tokio::io::AsyncSeekExt;
use tracing::warn;

use crate::endpoint::{
    Connection, Direction, EndpointStream, SyncHalves,
    parse::{Opt, ParseEndpointError},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct File {
    pub path: std::path::PathBuf,
    #[serde(default)]
    pub append: bool,
    /// Unset rather than `true` so that `device` can supply the other default
    /// without losing the difference between "left alone" and "asked for",
    /// which is what makes `device` plus `create` an error rather than a
    /// silent override. Read through [`File::creates`].
    #[serde(default)]
    pub create: Option<bool>,
    #[serde(default)]
    pub truncate: bool,
    /// Require the path to exist and be a block or character device.
    #[serde(default)]
    pub device: bool,
    /// Start at this offset rather than at the beginning.
    #[serde(default)]
    pub seek: Option<ByteSize>,
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
        let mut create = None;
        let mut truncate = false;
        let mut device = false;
        let mut seek = None;
        let mut name = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "append" => append = opt.flag()?,
                "create" => create = Some(opt.flag()?),
                "device" | "dev" => device = opt.flag()?,
                "name" => name = Some(opt.string()?),
                "seek" => seek = Some(opt.size()?),
                "truncate" | "trunc" => truncate = opt.flag()?,
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        let spec = Self {
            path: std::path::PathBuf::from(body),
            append,
            create,
            truncate,
            device,
            seek,
            name,
        };

        spec.validate()?;

        Ok(spec)
    }

    /// Reject the combinations where one option would quietly undo another.
    ///
    /// All three are cases where the pair has no coherent reading, so the
    /// alternative is an option that parses and then does nothing, which is
    /// the thing this grammar exists to avoid.
    fn validate(&self) -> Result<(), ParseEndpointError> {
        if self.seek.is_some() && self.append {
            return Err(ParseEndpointError::Conflict {
                scheme: Self::SCHEME,
                reason: "seek has nothing to do under append, where every write goes to the end",
            });
        }

        if self.device && self.create == Some(true) {
            return Err(ParseEndpointError::Conflict {
                scheme: Self::SCHEME,
                reason: "device asserts the path already exists, so there is nothing to create",
            });
        }

        if self.device && (self.truncate || self.append) {
            return Err(ParseEndpointError::Conflict {
                scheme: Self::SCHEME,
                reason: "a device has no length, so it cannot be truncated or appended to",
            });
        }

        Ok(())
    }

    /// Whether a missing path is created. Never, under `device`.
    fn creates(&self) -> bool {
        self.create.unwrap_or(!self.device)
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
                self.creates(),
                self.append,
                self.truncate && !self.append,
            ),
        }
    }

    /// Refuse a path that is not a device, before anything is opened.
    ///
    /// Before rather than after, because opening is what would create the
    /// regular file this exists to prevent. The error names what was found, so
    /// a wrong path and an unplugged device read differently.
    fn check_device(&self) -> anyhow::Result<()> {
        if !self.device {
            return Ok(());
        }

        let path = self.path.display();
        let meta = std::fs::metadata(&self.path)
            .with_context(|| format!("{path} was declared a device"))?;
        let kind = meta.file_type();

        if !kind.is_block_device() && !kind.is_char_device() {
            anyhow::bail!("{path} is not a device");
        }

        Ok(())
    }

    /// The offset, checked against the device's block size where there is one.
    ///
    /// An unaligned offset into a block device is not an error the kernel
    /// reports, it is a shifted image, so it is worth catching here.
    fn seek_to(&self, meta: &std::fs::Metadata) -> anyhow::Result<Option<u64>> {
        let Some(seek) = self.seek else {
            return Ok(None);
        };

        let offset = seek.bytes() as u64;

        if meta.file_type().is_block_device() {
            let block = meta.blksize();

            if block != 0 && !offset.is_multiple_of(block) {
                warn!(
                    offset,
                    block,
                    path = %self.path.display(),
                    "seek is not a multiple of the block size",
                );
            }
        }

        Ok(Some(offset))
    }

    /// Warn if the path turned out to be a FIFO. See the module docs.
    fn warn_if_fifo(&self, kind: std::fs::FileType) {
        if kind.is_fifo() {
            warn!(path = %self.path.display(), "FIFO endpoint: open blocks until a peer connects");
        }
    }

    pub(super) async fn connect(&self, dir: Direction) -> anyhow::Result<Connection> {
        let (read, create, append, truncate) = self.options(dir);

        self.check_device()?;

        let mut file = tokio::fs::OpenOptions::new()
            .read(read)
            .write(!read)
            .create(create)
            .append(append)
            .truncate(truncate)
            .open(&self.path)
            .await
            .with_context(|| format!("opening {}", self.path.display()))?;

        let meta = file.metadata().await?;
        self.warn_if_fifo(meta.file_type());

        if let Some(offset) = self.seek_to(&meta)? {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .with_context(|| format!("seeking to {offset} in {}", self.path.display()))?;
        }

        Ok(match dir {
            Direction::Source => EndpointStream::read_only(file),
            Direction::Sink => EndpointStream::write_only(file),
        }
        .into_connection())
    }

    pub(super) fn connect_sync(&self, dir: Direction) -> anyhow::Result<SyncHalves> {
        let (read, create, append, truncate) = self.options(dir);

        self.check_device()?;

        let mut file = std::fs::OpenOptions::new()
            .read(read)
            .write(!read)
            .create(create)
            .append(append)
            .truncate(truncate)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;

        let meta = file.metadata()?;
        self.warn_if_fifo(meta.file_type());

        if let Some(offset) = self.seek_to(&meta)? {
            file.seek(std::io::SeekFrom::Start(offset))
                .with_context(|| format!("seeking to {offset} in {}", self.path.display()))?;
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointSpec;

    fn file(s: &str) -> File {
        match s.parse::<EndpointSpec>().expect("parses") {
            EndpointSpec::File(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn err(s: &str) -> ParseEndpointError {
        s.parse::<EndpointSpec>().expect_err("rejected")
    }

    /// Unchanged for everything that does not mention a device.
    #[test]
    fn a_plain_file_still_creates_by_default() {
        assert!(file("file:/tmp/out.bin").creates());
        assert!(!file("file:/tmp/out.bin,create=false").creates());
    }

    /// The whole point: a wrong device path must not become a regular file.
    #[test]
    fn device_turns_creation_off() {
        assert!(!file("file:/dev/sda,device").creates());
    }

    #[test]
    fn asking_for_both_is_an_error_rather_than_an_override() {
        assert!(matches!(
            err("file:/dev/sda,device,create"),
            ParseEndpointError::Conflict { .. }
        ));

        // Saying it out loud is fine; it is only the contradiction that fails.
        assert!(!file("file:/dev/sda,device,create=false").creates());
    }

    #[test]
    fn a_device_has_no_length_to_change() {
        assert!(matches!(
            err("file:/dev/sda,device,truncate"),
            ParseEndpointError::Conflict { .. }
        ));
        assert!(matches!(
            err("file:/dev/sda,device,append"),
            ParseEndpointError::Conflict { .. }
        ));
    }

    /// `O_APPEND` sends every write to the end whatever the offset says, so
    /// the pair has no reading in which both do something.
    #[test]
    fn seek_and_append_contradict() {
        assert!(matches!(
            err("file:/tmp/out.bin,seek=1MiB,append"),
            ParseEndpointError::Conflict { .. }
        ));
    }

    #[test]
    fn seek_takes_the_usual_suffixes() {
        assert_eq!(
            file("file:/dev/sda,device,seek=1MiB").seek,
            Some(ByteSize(1024 * 1024)),
        );
        assert_eq!(file("file:/tmp/x,seek=512").seek, Some(ByteSize(512)));
    }
}
