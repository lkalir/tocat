//! endpoint.rs — what the relay connects to.
//!
//! An endpoint is one end of the byte path: a socket to dial or listen on, a
//! file, a child process, or stdio. [`EndpointSpec`] is the parsed form and can
//! come either from the compact CLI grammar (`tcp-listen:8080,fork`) or from a
//! TOML table, which is why it carries both `FromStr` and `Deserialize`.
//!
//! [`Direction`] here means *role* — which side of the relay this endpoint is —
//! not which way bytes are moving. It matters for `file:` and the other
//! half-duplex endpoints, where the same spec opens for reading as a source and
//! for writing as a sink.
//!
//! Two things are load-bearing and easy to lose in a refactor: a `UnixListen`
//! endpoint, or a `pipe:` opened with `unlink`, hands back a [`PathGuard`] that
//! removes the path on drop, so the guard has to outlive the connection; and a
//! FIFO opened without `hold` blocks until a peer appears, which is worth the
//! warning it emits rather than looking like a hang.
//!
//! `pipe:` (alias `fifo:`) defaults to holding the FIFO open read-write, so it
//! outlives its producers. `file:` pointed at a FIFO is the one-shot version
//! and keeps that behaviour.
//!
//! Payload dumping used to be an endpoint option (`dump=`, `format=`). It is
//! now the `tee` plugin, which can sit anywhere in the pipeline rather than
//! only at the ends.

use std::{
    num::NonZeroUsize,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::PathBuf,
    str::FromStr,
};

use anyhow::Context;
use rustix::fs::Mode as FileMode;
use serde::{Deserialize, Serialize, Serializer, de::Error as _};
use tocat_api::StderrMode;
use tokio::{
    io::{AsyncRead, AsyncWrite, empty, sink},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
};
use tracing::{debug, info, warn};

use crate::{child, config::ByteSize};

/// A synchronous endpoint half. See [`EndpointSpec::connect_sync`].
pub type SyncRead = Box<dyn std::io::Read + Send>;
pub type SyncWrite = Box<dyn std::io::Write + Send>;

/// The blocking halves of an endpoint. Either may be absent — a `file:` source
/// has nothing to write to, and pairing a reader with a missing writer means
/// that direction does not exist and should not be run at all.
#[derive(Default)]
pub struct SyncHalves {
    pub reader: Option<SyncRead>,
    pub writer: Option<SyncWrite>,
    /// Carried for the same reason [`Connection`] carries one: a `pipe:` opened
    /// with `unlink` removes its path when this is dropped, so the caller has
    /// to hold it for as long as the transfer runs.
    pub guard: Option<PathGuard>,
}

/// stdin/stdout as raw descriptors.
///
/// `std::io::stdout()` is a `LineWriter`: it scans every byte for newlines and
/// flushes on each one, which is ruinous for binary payload. `std::io::stdin()`
/// carries an 8 KiB `BufReader`, which is one more copy of everything. Going to
/// the descriptor directly avoids both.
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

pub struct Connection {
    pub stream: EndpointStream,
    pub guard: Option<PathGuard>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Endpoint {
    Raw(String),
    Spec(EndpointSpec),
}

pub enum EndpointStream {
    Duplex(Box<dyn AsyncStream>),
    Split(BoxRead, BoxWrite),
    /// A datagram socket, not `AsyncRead`/`AsyncWrite` since those traits have
    /// no way to preserve message boundaries
    Datagram(DatagramSocket),
}

/// A connected datagram socket
#[derive(Clone)]
pub enum DatagramSocket {
    Udp(std::sync::Arc<tokio::net::UdpSocket>),
}

impl DatagramSocket {
    /// One datagram. A message longer than `buf` is **truncated**.
    pub async fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            DatagramSocket::Udp(socket) => socket.recv(buf).await,
        }
    }

    pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            DatagramSocket::Udp(socket) => socket.send(buf).await,
        }
    }
}

/// The reading end of an endpoint, stream or datagram
pub enum ReadHalf {
    Stream(BoxRead),
    Datagram(DatagramSocket),
}

/// The writing end of an endpoint, stream or datagram
pub enum WriteHalf {
    Stream(BoxWrite),
    Datagram(DatagramSocket),
}

pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

impl EndpointStream {
    pub fn into_connection(self) -> Connection {
        Connection {
            stream: self,
            guard: None,
        }
    }

    pub fn into_connection_with_guard(self, guard: PathGuard) -> Connection {
        Connection {
            stream: self,
            guard: Some(guard),
        }
    }

    pub fn tcp(s: TcpStream) -> Self {
        Self::Duplex(Box::new(s))
    }

    pub fn unix(s: UnixStream) -> Self {
        Self::Duplex(Box::new(s))
    }

    pub fn stdio() -> Self {
        Self::Split(Box::new(tokio::io::stdin()), Box::new(tokio::io::stdout()))
    }

    pub fn read_only(r: impl AsyncRead + Unpin + Send + 'static) -> Self {
        Self::Split(Box::new(r), Box::new(sink()))
    }

    pub fn write_only(w: impl AsyncWrite + Unpin + Send + 'static) -> Self {
        Self::Split(Box::new(empty()), Box::new(w))
    }

    pub fn datagram(socket: tokio::net::UdpSocket) -> Self {
        Self::Datagram(DatagramSocket::Udp(std::sync::Arc::new(socket)))
    }

    pub fn into_halves(self) -> (ReadHalf, WriteHalf) {
        match self {
            Self::Duplex(s) => {
                let (r, w) = tokio::io::split(s);
                (
                    ReadHalf::Stream(Box::new(r)),
                    WriteHalf::Stream(Box::new(w)),
                )
            }
            Self::Split(r, w) => (ReadHalf::Stream(r), WriteHalf::Stream(w)),
            Self::Datagram(socket) => (
                ReadHalf::Datagram(socket.clone()),
                WriteHalf::Datagram(socket),
            ),
        }
    }
}

impl Endpoint {
    pub fn into_spec(self) -> Result<EndpointSpec, ParseEndpointError> {
        match self {
            Endpoint::Raw(raw) => raw.parse(),
            Endpoint::Spec(spec) => Ok(spec),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Mode(u32);

impl FromStr for Mode {
    type Err = ParseEndpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bits = u32::from_str_radix(s, 8)
            .map_err(|_| ParseEndpointError::InvalidMode(s.to_string()))?;

        if bits > 0o777 {
            return Err(ParseEndpointError::InvalidMode(s.to_string()));
        }

        Ok(Mode(bits))
    }
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bits = u32::from_str_radix(&s, 8)
            .map_err(|_| D::Error::custom(format!("invalid octal mode {s:?}")))?;
        if bits > 0o777 {
            return Err(D::Error::custom(format!("mode {s} out of range")));
        }

        Ok(Mode(bits))
    }
}

impl Serialize for Mode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{:03o}", self.0))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum EndpointSpec {
    #[serde(
        alias = "TCP",
        alias = "tcp-connect",
        alias = "connect",
        alias = "TCP-CONNECT",
        alias = "CONNECT"
    )]
    Tcp {
        addr: String,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(
        alias = "TCP-LISTEN",
        alias = "tcplisten",
        alias = "TCPLISTEN",
        alias = "listen",
        alias = "LISTEN"
    )]
    TcpListen {
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        fork: bool,
        #[serde(default, rename = "max-connections")]
        max_connections: Option<NonZeroUsize>,
    },
    Stdio {
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(alias = "UNIX", alias = "unix-connect", alias = "UNIX-CONNECT")]
    Unix {
        path: PathBuf,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(alias = "UNIX-LISTEN", alias = "unixlisten", alias = "UNIXLISTEN")]
    UnixListen {
        path: PathBuf,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        fork: bool,
        #[serde(default, rename = "max-connections")]
        max_connections: Option<NonZeroUsize>,
        #[serde(default)]
        unlink: bool,
        #[serde(default)]
        mode: Option<Mode>,
    },
    #[serde(alias = "fifo", alias = "FIFO", alias = "PIPE")]
    Pipe {
        path: PathBuf,
        /// `mkfifo` the path if it is missing.
        #[serde(default = "default_true")]
        create: bool,
        #[serde(default)]
        mode: Option<Mode>,
        /// Remove the FIFO when the relay finishes.
        #[serde(default)]
        unlink: bool,
        /// Hold the FIFO open across producers.
        ///
        /// With `hold` (the default) tocat opens read-write, so it is its own
        /// writer: opening never blocks and the stream never ends, which is
        /// what you want for a log or event pipe whose producers come and go.
        /// Without it, a source blocks until a writer appears and sees EOF when
        /// the last one leaves — one producer, then done.
        #[serde(default = "default_true")]
        hold: bool,
        /// Kernel FIFO capacity. Linux only, best-effort, and unrelated to the
        /// global `buffer-size`: this one decides when the producer blocks.
        #[serde(default)]
        size: Option<ByteSize>,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(alias = "open", alias = "FILE", alias = "OPEN")]
    File {
        path: PathBuf,
        #[serde(default)]
        append: bool,
        #[serde(default = "default_true")]
        create: bool,
        #[serde(default)]
        truncate: bool,
        #[serde(default)]
        name: Option<String>,
    },
    Exec {
        argv: Vec<String>,
        #[serde(default)]
        name: Option<String>,
    },
    System {
        command: String,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(alias = "UDP", alias = "udp-connect", alias = "UDP-CONNECT")]
    Udp {
        addr: String,
        /// Local address to bind before connecting. Defaults to an ephemeral
        /// port on all interfaces
        #[serde(default)]
        bind: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(alias = "UDP-LISTEN", alias = "udplisten", alias = "UDPLISTEN")]
    UdpListen {
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        name: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Source,
    Sink,
}

async fn open_file(
    path: &std::path::Path,
    dir: Direction,
    append: bool,
    create: bool,
    truncate: bool,
) -> anyhow::Result<tokio::fs::File> {
    let mut opts = tokio::fs::OpenOptions::new();

    match dir {
        Direction::Source => {
            opts.read(true);
        }
        Direction::Sink => {
            opts.write(true)
                .create(create)
                .append(append)
                .truncate(truncate && !append);
        }
    }

    let file = opts
        .open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;

    #[cfg(unix)]
    if file.metadata().await?.file_type().is_fifo() {
        warn!(path = %path.display(), "FIFO endpoint: open blocks until a peer connects");
    }

    Ok(file)
}

/// Create the FIFO if it is missing, and refuse anything that is not one.
fn ensure_fifo(path: &std::path::Path, create: bool, mode: Option<Mode>) -> anyhow::Result<()> {
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
                create,
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

    // mkfifo's mode is masked by umask, so apply it explicitly, as with a
    // bound unix socket.
    if let Some(mode) = mode {
        std::fs::set_permissions(path, PermissionsExt::from_mode(mode.0))
            .with_context(|| format!("chmod {:o} on {}", mode.0, path.display()))?;
    }

    Ok(())
}

/// Enlarge a pipe's kernel buffer, if the descriptor is a pipe at all.
///
/// Separate from `-b`, which sizes tocat's own copy buffer. This one is kernel
/// memory and decides when the *writer* blocks: at the 64 KiB default, a
/// producer stalls as soon as that much is unread, however large tocat's buffer
/// is. That includes descriptors nobody declared as pipes e.g. `tocat … | pv`
/// makes fd 1 a pipe, and a child's stdin and stdout always are.
///
/// Entirely best-effort. Rather than stat the descriptor first, this asks and
/// interprets the refusal: a non-pipe answers `EINVAL`, which is not a problem
/// worth reporting above debug.
#[cfg(target_os = "linux")]
pub fn size_if_pipe<F: std::os::fd::AsFd>(fd: &F, label: &str, want: usize) {
    use rustix::{io::Errno, pipe::fcntl_setpipe_size};

    match fcntl_setpipe_size(fd, want) {
        Ok(got) if got == want => debug!(pipe = label, size = got, "pipe resized"),
        Ok(got) => debug!(
            pipe = label,
            want, got, "pipe resized to the next power of two"
        ),

        // Not a pipe. The common case for a file or a socket, and fine.
        Err(e) if e == Errno::INVAL => debug!(pipe = label, "not a pipe; leaving it alone"),

        // More data is buffered than the requested size would hold.
        Err(e) if e == Errno::BUSY => debug!(pipe = label, want, "pipe busy; leaving it alone"),

        Err(e) if e == Errno::PERM => warn!(
            pipe = label,
            want, "cannot enlarge pipe past /proc/sys/fs/pipe-max-size without CAP_SYS_RESOURCE",
        ),

        Err(e) => debug!(pipe = label, error = %e, "could not resize pipe"),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn size_if_pipe<F: std::os::fd::AsFd>(_fd: &F, _label: &str, _want: usize) {
    // F_SETPIPE_SZ is Linux-only. Everywhere else keeps the default capacity.
}

/// Which access a FIFO is opened with.
///
/// `hold` opens read-write even on the writing side. POSIX leaves that
/// undefined, but every platform tocat targets implements it, and it is the
/// only way to be your own writer — which is what keeps the FIFO from
/// reporting EOF each time a producer exits.
fn pipe_access(dir: Direction, hold: bool) -> (bool, bool) {
    match (hold, dir) {
        (true, _) => (true, true),
        (false, Direction::Source) => (true, false),
        (false, Direction::Sink) => (false, true),
    }
}

fn open_file_sync(
    path: &std::path::Path,
    dir: Direction,
    append: bool,
    create: bool,
    truncate: bool,
) -> anyhow::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();

    match dir {
        Direction::Source => {
            opts.read(true);
        }
        Direction::Sink => {
            opts.write(true)
                .create(create)
                .append(append)
                .truncate(truncate && !append);
        }
    }

    let file = opts
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;

    #[cfg(unix)]
    if file.metadata()?.file_type().is_fifo() {
        warn!(path = %path.display(), "FIFO endpoint: open blocks until a peer connects");
    }

    Ok(file)
}

async fn spawn_child(
    program: &str,
    args: &[String],
    shell: bool,
    buffer: usize,
) -> anyhow::Result<Connection> {
    // Endpoints inherit stderr so a child's diagnostics reach the terminal
    // rather than the relayed data.
    let parts = child::spawn(program, args, shell, StderrMode::Inherit, buffer)?;
    child::reap_in_background(parts.child);

    Ok(EndpointStream::Split(Box::new(parts.stdout), Box::new(parts.stdin)).into_connection())
}

impl EndpointSpec {
    pub fn is_listen(&self) -> bool {
        matches!(
            self,
            EndpointSpec::TcpListen { .. }
                | EndpointSpec::UnixListen { .. }
                | EndpointSpec::UdpListen { .. }
        )
    }

    pub fn is_datagram(&self) -> bool {
        matches!(
            self,
            EndpointSpec::Udp { .. } | EndpointSpec::UdpListen { .. }
        )
    }

    pub fn is_fork(&self) -> bool {
        matches!(
            self,
            EndpointSpec::TcpListen { fork: true, .. }
                | EndpointSpec::UnixListen { fork: true, .. }
        )
    }

    pub fn name(&self) -> String {
        match self {
            EndpointSpec::Tcp { name: Some(n), .. } => n.clone(),
            EndpointSpec::Tcp {
                addr, name: None, ..
            } => format!("tcp://{addr}"),
            EndpointSpec::Stdio { name: Some(n), .. } => n.clone(),
            EndpointSpec::Stdio { name: None, .. } => "STDIO".to_string(),
            EndpointSpec::TcpListen { name: Some(n), .. } => n.clone(),
            EndpointSpec::TcpListen {
                host,
                port,
                name: None,
                ..
            } => format!(
                "tcp://{}:{}",
                host.clone().unwrap_or_else(|| "127.0.0.1".to_string()),
                port.unwrap_or(8000)
            ),
            EndpointSpec::Unix { name: Some(n), .. } => n.clone(),
            EndpointSpec::Unix {
                path, name: None, ..
            } => format!("unix://{}", path.display()),
            EndpointSpec::UnixListen { name: Some(n), .. } => n.clone(),
            EndpointSpec::UnixListen {
                path, name: None, ..
            } => format!("unix://{}", path.display()),
            EndpointSpec::Pipe { name: Some(n), .. } => n.clone(),
            EndpointSpec::Pipe {
                path, name: None, ..
            } => format!("pipe://{}", path.display()),
            EndpointSpec::File { path, .. } => {
                format!("file://{}", path.display())
            }
            EndpointSpec::Exec { argv, .. } => {
                format!("EXEC({})", argv.join(" "))
            }
            EndpointSpec::System { command, .. } => {
                format!("SYSTEM({command})")
            }
            EndpointSpec::Udp { name: Some(n), .. } => n.clone(),
            EndpointSpec::Udp {
                addr, name: None, ..
            } => format!("udp://{addr}"),
            EndpointSpec::UdpListen { name: Some(n), .. } => n.clone(),
            EndpointSpec::UdpListen {
                host,
                port,
                name: None,
                ..
            } => format!(
                "udp://{}:{}",
                host.clone().unwrap_or_else(|| "127.0.0.1".to_string()),
                port.unwrap_or(8000)
            ),
        }
    }

    /// True when tokio has no real async implementation for this endpoint and
    /// serves it from the blocking pool.
    ///
    /// Those wrappers read into their own buffer and then copy into yours, so a
    /// relay between two of them pays two userspace copies of the payload that
    /// a plain `read`/`write` loop does not. When nothing else needs the async
    /// machinery, [`connect_sync`](Self::connect_sync) skips it.
    pub fn is_blocking_backed(&self) -> bool {
        matches!(
            self,
            EndpointSpec::File { .. } | EndpointSpec::Stdio { .. } | EndpointSpec::Pipe { .. }
        )
    }

    /// Open this endpoint as plain blocking handles.
    ///
    /// Only valid for endpoints where
    /// [`is_blocking_backed`](Self::is_blocking_backed) holds; sockets are
    /// genuinely async and gain nothing here.
    pub fn connect_sync(&self, dir: Direction, buffer: usize) -> anyhow::Result<SyncHalves> {
        match self {
            EndpointSpec::Stdio { .. } => {
                size_if_pipe(&std::io::stdin(), "stdin", buffer);
                size_if_pipe(&std::io::stdout(), "stdout", buffer);

                Ok(SyncHalves {
                    // SAFETY: fds 0 and 1 are open for the life of the process,
                    // and `RawStd` will not close them.
                    reader: Some(Box::new(unsafe { RawStd::new(0) })),
                    writer: Some(Box::new(unsafe { RawStd::new(1) })),
                    guard: None,
                })
            }
            EndpointSpec::Pipe {
                path,
                create,
                mode,
                unlink,
                hold,
                size,
                ..
            } => {
                ensure_fifo(path, *create, *mode)?;

                let (read, write) = pipe_access(dir, *hold);
                let file = std::fs::OpenOptions::new()
                    .read(read)
                    .write(write)
                    .open(path)
                    .with_context(|| format!("opening {}", path.display()))?;

                if let Some(size) = size {
                    size_if_pipe(&file, &path.display().to_string(), size.bytes());
                }

                let guard = unlink.then(|| PathGuard(path.clone()));

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
            EndpointSpec::File {
                path,
                append,
                create,
                truncate,
                ..
            } => {
                let file = open_file_sync(path, dir, *append, *create, *truncate)?;

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
            other => anyhow::bail!("{} has no synchronous form", other.name()),
        }
    }

    pub fn max_connections(&self) -> NonZeroUsize {
        const DEFAULT_MAX_CONNECTIONS: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

        match self {
            EndpointSpec::TcpListen {
                max_connections, ..
            }
            | EndpointSpec::UnixListen {
                max_connections, ..
            } => max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS),
            _ => DEFAULT_MAX_CONNECTIONS,
        }
    }

    pub async fn connect(&self, dir: Direction, buffer: usize) -> anyhow::Result<Connection> {
        match self {
            EndpointSpec::Tcp { addr, .. } => {
                let stream = TcpStream::connect(addr).await?;
                Ok(EndpointStream::tcp(stream).into_connection())
            }
            EndpointSpec::Stdio { .. } => {
                // `tocat … | pv` makes fd 1 a pipe without anyone declaring
                // one, and its 64 KiB default would cap every write.
                size_if_pipe(&std::io::stdin(), "stdin", buffer);
                size_if_pipe(&std::io::stdout(), "stdout", buffer);

                Ok(EndpointStream::stdio().into_connection())
            }
            EndpointSpec::TcpListen { host, port, .. } => {
                let host = host.as_deref().unwrap_or("127.0.0.1");
                let port = port.unwrap_or(8000);
                let listener = TcpListener::bind((host, port)).await?;
                info!("Listening for connection on {host}:{port}");
                let (stream, peer) = listener.accept().await?;
                info!("Accepted connection from {peer}");
                Ok(EndpointStream::tcp(stream).into_connection())
            }
            EndpointSpec::Unix { path, .. } => {
                let stream = UnixStream::connect(path)
                    .await
                    .with_context(|| format!("connecting to {}", path.display()))?;
                Ok(EndpointStream::unix(stream).into_connection())
            }
            EndpointSpec::UnixListen {
                path, unlink, mode, ..
            } => {
                let listener = bind_unix(path, *unlink, *mode).await?;
                let guard = PathGuard(path.clone());
                info!(path = %path.display(), "listening");
                let (stream, _) = listener.accept().await?;
                Ok(EndpointStream::unix(stream).into_connection_with_guard(guard))
            }
            EndpointSpec::Pipe {
                path,
                create,
                mode,
                unlink,
                hold,
                size,
                ..
            } => {
                ensure_fifo(path, *create, *mode)?;

                let (read, write) = pipe_access(dir, *hold);

                if !hold {
                    warn!(path = %path.display(), "FIFO without `hold`: open blocks until a peer connects");
                }

                let file = tokio::fs::OpenOptions::new()
                    .read(read)
                    .write(write)
                    .open(path)
                    .await
                    .with_context(|| format!("opening {}", path.display()))?;

                if let Some(size) = size {
                    size_if_pipe(&file, &path.display().to_string(), size.bytes());
                }

                // Half-duplex regardless of how the descriptor was opened: a
                // `hold` FIFO is readable *and* writable, and treating it as
                // duplex would feed our own writes straight back to our reader.
                let stream = match dir {
                    Direction::Source => EndpointStream::read_only(file),
                    Direction::Sink => EndpointStream::write_only(file),
                };

                Ok(if *unlink {
                    stream.into_connection_with_guard(PathGuard(path.clone()))
                } else {
                    stream.into_connection()
                })
            }
            EndpointSpec::File {
                path,
                append,
                create,
                truncate,
                ..
            } => {
                let f = open_file(path, dir, *append, *create, *truncate).await?;
                Ok(match dir {
                    Direction::Source => EndpointStream::read_only(f),
                    Direction::Sink => EndpointStream::write_only(f),
                }
                .into_connection())
            }
            EndpointSpec::Exec { argv, .. } => {
                let Some((program, args)) = argv.split_first() else {
                    anyhow::bail!("exec: empty argv");
                };
                spawn_child(program, args, false, buffer).await
            }
            EndpointSpec::System { command, .. } => spawn_child(command, &[], true, buffer).await,
            EndpointSpec::Udp { addr, bind, .. } => {
                // Resolve the peer first: the local socket has to be in the same address
                // family, so a v6 peer needs a v6 wildcard. This is the same mismatch that
                // makes a `localhost` listener unreachable, one layer down.
                let peer = tokio::net::lookup_host(addr)
                    .await
                    .with_context(|| format!("resolving {addr}"))?
                    .next()
                    .with_context(|| format!("{addr} resolved to no address"))?;

                let socket = match bind {
                    Some(local) => tokio::net::UdpSocket::bind(local.as_str())
                        .await
                        .with_context(|| format!("binding {local}"))?,
                    None => {
                        let wildcard: std::net::SocketAddr = if peer.is_ipv4() {
                            (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
                        } else {
                            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
                        };

                        tokio::net::UdpSocket::bind(wildcard)
                            .await
                            .with_context(|| format!("binding {wildcard}"))?
                    }
                };

                socket
                    .connect(peer)
                    .await
                    .with_context(|| format!("connecting to {peer}"))?;

                Ok(EndpointStream::datagram(socket).into_connection())
            }
            EndpointSpec::UdpListen { host, port, .. } => {
                let host = host.as_deref().unwrap_or("127.0.0.1");
                let port = port.unwrap_or(8000);
                let socket = tokio::net::UdpSocket::bind((host, port)).await?;
                info!(local = %socket.local_addr()?, "listening for datagrams");

                // Peek rather than receive: the first datagram tells us who the
                // peer is, and it has to stay queued so
                // the relay does not eat it. There is nothing to accept: the
                // frist sender simply becomes the peer
                // for the rest of the run.
                let mut probe = [0u8; 1];
                let (_, peer) = socket.peek_from(&mut probe).await?;
                socket.connect(peer).await?;
                info!("Peered with {peer}");

                Ok(EndpointStream::datagram(socket).into_connection())
            }
        }
    }
}

/// Removes a path on drop: a bound unix socket, or a FIFO opened with
/// `unlink`. Must outlive the connection using it.
pub struct PathGuard(pub PathBuf);

impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn bind_unix(
    path: &std::path::Path,
    unlink: bool,
    mode: Option<Mode>,
) -> anyhow::Result<UnixListener> {
    if unlink && path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => anyhow::bail!("{} is already in use", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)
                    .with_context(|| format!("removing stale socket {}", path.display()))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("probing {}", path.display())),
        }
    }

    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;

    if let Some(mode) = mode {
        std::fs::set_permissions(path, PermissionsExt::from_mode(mode.0))
            .with_context(|| format!("chmod {:o} on {}", mode.0, path.display()))?;
    }

    Ok(listener)
}

#[derive(Debug, PartialEq)]
pub enum ParseEndpointError {
    Empty,
    UnknownScheme(String),
    UnknownOption(String),
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
            ParseEndpointError::UnknownOption(body) => write!(f, "unknown option: {body}"),
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

#[derive(Default, Debug)]
struct Options {
    name: Option<String>,
    fork: bool,
    max_connections: Option<NonZeroUsize>,
    unlink: bool,
    hold: bool,
    bind: Option<String>,
    size: Option<ByteSize>,
    mode: Option<Mode>,
    append: bool,
    create: bool,
    truncate: bool,
}

fn value<'a>(key: &str, v: Option<&'a str>) -> Result<&'a str, ParseEndpointError> {
    v.ok_or_else(|| ParseEndpointError::MissingValue(key.to_string()))
}

impl Options {
    fn new() -> Self {
        Self {
            create: true,
            hold: true,
            ..Default::default()
        }
    }

    fn parse(parts: std::str::Split<'_, char>) -> Result<Self, ParseEndpointError> {
        use ParseEndpointError as E;

        let mut o = Self::new();

        for opt in parts {
            let (key, val) = match opt.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (opt, None),
            };

            let flag = |v: Option<&str>| match v {
                None => Ok(true),
                Some(v) => v.parse::<bool>().map_err(|_| E::InvalidFlag(v.to_string())),
            };

            match key {
                "append" => o.append = flag(val)?,
                "create" => o.create = flag(val)?,
                "fork" => o.fork = flag(val)?,
                "bind" => o.bind = Some(value(key, val)?.to_string()),
                "hold" => o.hold = flag(val)?,
                "size" | "pipe-size" => {
                    o.size = Some(
                        value(key, val)?
                            .parse()
                            .map_err(|e| E::InvalidSize(format!("{e}")))?,
                    );
                }
                "max-connections" | "max_connections" | "max_conn" | "max-conn" => {
                    o.max_connections = Some(
                        value(key, val)?
                            .parse()
                            .map_err(|_| E::InvalidNumber(key.to_string()))?,
                    )
                }
                "mode" => o.mode = Some(value(key, val)?.parse()?),
                "name" => o.name = Some(value(key, val)?.to_string()),
                "truncate" | "trunc" => o.truncate = flag(val)?,
                "unlink" => o.unlink = flag(val)?,
                _ => return Err(E::UnknownOption(key.to_string())),
            }
        }

        Ok(o)
    }
}

fn tcp(body: &str, o: Options) -> Result<EndpointSpec, ParseEndpointError> {
    if body.is_empty() {
        return Err(ParseEndpointError::Empty);
    }
    Ok(EndpointSpec::Tcp {
        addr: body.to_owned(),
        name: o.name,
    })
}

fn parse_host_port(body: &str) -> Result<(Option<String>, Option<u16>), ParseEndpointError> {
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

fn udp_listen(body: &str, o: Options) -> Result<EndpointSpec, ParseEndpointError> {
    let (host, port) = parse_host_port(body)?;
    Ok(EndpointSpec::UdpListen {
        host,
        port,
        name: o.name,
    })
}

fn tcp_listen(body: &str, o: Options) -> Result<EndpointSpec, ParseEndpointError> {
    let (host, port) = parse_host_port(body)?;
    Ok(EndpointSpec::TcpListen {
        host,
        port,
        name: o.name,
        fork: o.fork,
        max_connections: o.max_connections,
    })
}

fn exec(body: &str, o: Options) -> Result<EndpointSpec, ParseEndpointError> {
    let argv: Vec<String> = body.split_whitespace().map(String::from).collect();
    if argv.is_empty() {
        return Err(ParseEndpointError::Empty);
    }

    Ok(EndpointSpec::Exec { argv, name: o.name })
}

impl FromStr for EndpointSpec {
    type Err = ParseEndpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(Self::Err::Empty);
        }

        if s == "-" {
            return Ok(Self::Stdio { name: None });
        }

        let mut parts = s.split(',');
        let target = parts.next().unwrap_or("");
        let opts = Options::parse(parts)?;

        let (scheme, body) = target.split_once(':').unwrap_or((target, ""));

        match scheme.to_lowercase().as_str() {
            "exec" => exec(body, opts),
            "system" => Ok(Self::System {
                command: body.to_string(),
                name: opts.name,
            }),
            "tcp" | "tcp-connect" | "connect" => tcp(body, opts),
            "tcplisten" | "tcp-listen" | "listen" => tcp_listen(body, opts),
            "udp" | "udp-connect" => {
                if body.is_empty() {
                    return Err(Self::Err::Empty);
                }
                Ok(Self::Udp {
                    addr: body.to_owned(),
                    bind: opts.bind,
                    name: opts.name,
                })
            }
            "udp-listen" | "udplisten" => udp_listen(body, opts),
            "stdio" => Ok(Self::Stdio { name: opts.name }),
            "unix" | "unix-connect" => Ok(Self::Unix {
                path: PathBuf::from(body),
                name: opts.name,
            }),
            "pipe" | "fifo" => Ok(Self::Pipe {
                path: PathBuf::from(body),
                create: opts.create,
                mode: opts.mode,
                unlink: opts.unlink,
                hold: opts.hold,
                size: opts.size,
                name: opts.name,
            }),
            "file" | "open" => Ok(Self::File {
                path: PathBuf::from(body),
                append: opts.append,
                create: opts.create,
                truncate: opts.truncate,
                name: opts.name,
            }),
            "unixlisten" | "unix-listen" => Ok(Self::UnixListen {
                path: PathBuf::from(body),
                name: opts.name,
                fork: opts.fork,
                max_connections: opts.max_connections,
                unlink: opts.unlink,
                mode: opts.mode,
            }),
            other => Err(Self::Err::UnknownScheme(other.to_owned())),
        }
    }
}
