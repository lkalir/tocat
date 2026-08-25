//! stdio.rs: `stdio:` (and the bare `-`).
//!
//! The synchronous path does not go through `std::io::stdin`/`stdout`.
//! `std::io::stdout()` is a `LineWriter`: it scans every byte for newlines and
//! flushes on each one, which is ruinous for binary payload. `std::io::stdin()`
//! carries an 8 KiB `BufReader`, which is one more copy of everything. Going to
//! the descriptor directly through [`RawStd`] avoids both.
//!
//! Both paths resize fd 0 and fd 1 if they turn out to be pipes. Nobody
//! declares those as pipes, but `tocat … | pv` makes fd 1 one, and its 64 KiB
//! default would otherwise cap every write.

use serde::{Deserialize, Serialize};
use tocat_api::normalize;

use crate::endpoint::{
    Connection, EndpointStream, SyncHalves,
    parse::{Opt, ParseEndpointError},
    sys::size_if_pipe,
};

#[derive(Debug, Deserialize, Serialize)]
pub struct Stdio {
    #[serde(default)]
    pub name: Option<String>,
}

/// stdin/stdout as raw descriptors.
struct RawStd(std::mem::ManuallyDrop<std::fs::File>);

impl RawStd {
    /// # Safety
    ///
    /// `fd` must be open for the life of the process. `ManuallyDrop` keeps the
    /// descriptor from being closed when this value is dropped, which matters:
    /// closing fd 1 out from under the process would be hard to debug.
    unsafe fn new(fd: std::os::fd::RawFd) -> Self {
        use std::os::fd::FromRawFd;
        Self(std::mem::ManuallyDrop::new(unsafe {
            std::fs::File::from_raw_fd(fd)
        }))
    }
}

impl std::io::Read for RawStd {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(buf)
    }
}

impl std::io::Write for RawStd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

impl Stdio {
    const SCHEME: &'static str = "stdio";

    pub(super) fn parse<'a>(
        _body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut name = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self { name })
    }

    pub(super) fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| "STDIO".to_string())
    }

    /// Give both standard descriptors the buffer size the relay was asked for,
    /// where they are pipes and the kernel allows it.
    fn resize(buffer: usize) {
        size_if_pipe(&std::io::stdin(), "stdin", buffer);
        size_if_pipe(&std::io::stdout(), "stdout", buffer);
    }

    pub(super) fn connect(&self, buffer: usize) -> anyhow::Result<Connection> {
        Self::resize(buffer);

        Ok(EndpointStream::stdio().into_connection())
    }

    pub(super) fn connect_sync(&self, buffer: usize) -> SyncHalves {
        Self::resize(buffer);

        SyncHalves {
            // SAFETY: fds 0 and 1 are open for the life of the process,
            // and `RawStd` will not close them.
            reader: Some(Box::new(unsafe { RawStd::new(0) })),
            writer: Some(Box::new(unsafe { RawStd::new(1) })),
            guard: None,
        }
    }
}
