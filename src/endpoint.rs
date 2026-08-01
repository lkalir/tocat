use std::{
    num::NonZeroUsize,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::PathBuf,
    process::Stdio,
    str::FromStr,
};

use anyhow::Context;
use serde::{Deserialize, Serialize, Serializer, de::Error as _};
use tokio::{
    io::{AsyncRead, AsyncWrite, empty, sink},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
    process::Command,
};
use tracing::{debug, info, warn};

pub struct Connection {
    pub stream: EndpointStream,
    pub guard: Option<UnixSocketGuard>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum DumpFormat {
    Hex,
    RawBinary,
}

#[derive(Debug, Clone, Deserialize, Default, Serialize)]
pub struct DumpConfig {
    pub file: Option<PathBuf>,
    pub format: Option<DumpFormat>,
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

    pub fn into_connection_with_guard(self, guard: UnixSocketGuard) -> Connection {
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

    pub fn is_duplex(&self) -> bool {
        matches!(self, EndpointStream::Duplex(_))
    }

    pub fn into_split(self) -> (BoxRead, BoxWrite) {
        match self {
            Self::Duplex(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            Self::Split(r, w) => (r, w),
        }
    }

    pub fn into_duplex(self) -> Box<dyn AsyncStream> {
        match self {
            Self::Duplex(s) => s,
            Self::Split(..) => unreachable!("checked by is_duplex"),
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

fn parse_dump_target(val: &str) -> Result<Option<PathBuf>, ParseEndpointError> {
    match val {
        "-" | "stderr" | "/dev/stderr" => Ok(None),
        "stdout" | "/dev/stdout" | "/dev/fd/1" => Err(ParseEndpointError::DumpToStdout),
        other => Ok(Some(PathBuf::from(other))),
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
        dump: Option<DumpConfig>,
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
        dump: Option<DumpConfig>,
        #[serde(default)]
        fork: bool,
        #[serde(default, rename = "max-connections")]
        max_connections: Option<NonZeroUsize>,
    },
    Stdio {
        #[serde(default)]
        dump: Option<DumpConfig>,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(alias = "UNIX", alias = "unix-connect", alias = "UNIX-CONNECT")]
    Unix {
        path: PathBuf,
        #[serde(default)]
        dump: Option<DumpConfig>,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(alias = "UNIX-LISTEN", alias = "unixlisten", alias = "UNIXLISTEN")]
    UnixListen {
        path: PathBuf,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        dump: Option<DumpConfig>,
        #[serde(default)]
        fork: bool,
        #[serde(default, rename = "max-connections")]
        max_connections: Option<NonZeroUsize>,
        #[serde(default)]
        unlink: bool,
        #[serde(default)]
        mode: Option<Mode>,
    },
    File {
        path: PathBuf,
        #[serde(default)]
        append: bool,
        #[serde(default = "default_true")]
        create: bool,
        #[serde(default)]
        truncate: bool,
        #[serde(default)]
        dump: Option<DumpConfig>,
        #[serde(default)]
        name: Option<String>,
    },
    Exec {
        argv: Vec<String>,
        #[serde(default)]
        dump: Option<DumpConfig>,
        #[serde(default)]
        name: Option<String>,
    },
    System {
        command: String,
        #[serde(default)]
        dump: Option<DumpConfig>,
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

async fn spawn_child(program: &str, args: &[String], shell: bool) -> anyhow::Result<Connection> {
    let mut cmd = if shell {
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        let mut c = Command::new(sh);
        c.arg("-c").arg(program);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = cmd.spawn().with_context(|| format!("spawning {program}"))?;
    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let pid = child.id();

    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) if status.success() => debug!(?pid, "child exited"),
            Ok(status) => warn!(?pid, %status, "child exited non-zero"),
            Err(e) => warn!(?pid, error = %e, "waiting on child failed"),
        }
    });

    Ok(EndpointStream::Split(Box::new(stdout), Box::new(stdin)).into_connection())
}

impl EndpointSpec {
    pub fn is_listen(&self) -> bool {
        matches!(
            self,
            EndpointSpec::TcpListen { .. } | EndpointSpec::UnixListen { .. }
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
            } => format!("tcp://({addr}"),
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
                host.clone().unwrap_or_else(|| "localhost".to_string()),
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
            EndpointSpec::File { path, .. } => {
                format!("file://{}", path.display())
            }
            EndpointSpec::Exec { argv, .. } => {
                format!("EXEC({})", argv.join(" "))
            }
            EndpointSpec::System { command, .. } => {
                format!("SYSTEM({command})")
            }
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

    pub fn dump_config(&self) -> Option<&DumpConfig> {
        match self {
            EndpointSpec::Tcp { dump, .. }
            | EndpointSpec::Stdio { dump, .. }
            | EndpointSpec::Unix { dump, .. }
            | EndpointSpec::UnixListen { dump, .. }
            | EndpointSpec::File { dump, .. }
            | EndpointSpec::Exec { dump, .. }
            | EndpointSpec::System { dump, .. }
            | EndpointSpec::TcpListen { dump, .. } => dump.as_ref(),
        }
    }

    pub async fn connect(&self, dir: Direction) -> anyhow::Result<Connection> {
        match self {
            EndpointSpec::Tcp { addr, .. } => {
                let stream = TcpStream::connect(addr).await?;
                Ok(EndpointStream::tcp(stream).into_connection())
            }
            EndpointSpec::Stdio { .. } => Ok(EndpointStream::stdio().into_connection()),
            EndpointSpec::TcpListen { host, port, .. } => {
                let host = host.as_deref().unwrap_or("localhost");
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
                let guard = UnixSocketGuard(path.clone());
                info!(path = %path.display(), "listening");
                let (stream, _) = listener.accept().await?;
                Ok(EndpointStream::unix(stream).into_connection_with_guard(guard))
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
                spawn_child(program, args, false).await
            }
            EndpointSpec::System { command, .. } => spawn_child(command, &[], true).await,
        }
    }
}

pub struct UnixSocketGuard(pub PathBuf);
impl Drop for UnixSocketGuard {
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
    DumpToStdout,
    InvalidMode(String),
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
            ParseEndpointError::DumpToStdout => write!(
                f,
                "cannot dump to stdout, it may carry relay payload. Use `-` for stderr."
            ),
            ParseEndpointError::InvalidMode(body) => write!(f, "invalid permissions: {body}"),
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
    dump: Option<DumpConfig>,
    fork: bool,
    max_connections: Option<NonZeroUsize>,
    unlink: bool,
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
            ..Default::default()
        }
    }

    fn parse(parts: std::str::Split<'_, char>) -> Result<Self, ParseEndpointError> {
        use ParseEndpointError as E;

        let mut o = Self::new();
        let mut dump = DumpConfig::default();
        let mut has_dump = false;

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
                "dump" | "dump_file" => {
                    dump.file = parse_dump_target(value(key, val)?)?;
                    has_dump = true;
                }
                "fork" => o.fork = flag(val)?,
                "format" | "dump_format" => {
                    dump.format = Some(match value(key, val)? {
                        "hex" => DumpFormat::Hex,
                        "raw" | "binary" | "raw-binary" => DumpFormat::RawBinary,
                        other => return Err(E::UnknownOption(other.to_string())),
                    });
                    has_dump = true;
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

        o.dump = has_dump.then_some(dump);
        Ok(o)
    }
}

fn tcp(body: &str, o: Options) -> Result<EndpointSpec, ParseEndpointError> {
    if body.is_empty() {
        return Err(ParseEndpointError::Empty);
    }
    Ok(EndpointSpec::Tcp {
        addr: body.to_owned(),
        dump: o.dump,
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

fn tcp_listen(body: &str, o: Options) -> Result<EndpointSpec, ParseEndpointError> {
    let (host, port) = parse_host_port(body)?;
    Ok(EndpointSpec::TcpListen {
        host,
        port,
        name: o.name,
        dump: o.dump,
        fork: o.fork,
        max_connections: o.max_connections,
    })
}

fn exec(body: &str, o: Options) -> Result<EndpointSpec, ParseEndpointError> {
    let argv: Vec<String> = body.split_whitespace().map(String::from).collect();
    if argv.is_empty() {
        return Err(ParseEndpointError::Empty);
    }

    Ok(EndpointSpec::Exec {
        argv,
        dump: o.dump,
        name: o.name,
    })
}

impl FromStr for EndpointSpec {
    type Err = ParseEndpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(Self::Err::Empty);
        }

        if s == "-" {
            return Ok(Self::Stdio {
                dump: None,
                name: None,
            });
        }

        let mut parts = s.split(',');
        let target = parts.next().unwrap_or("");
        let opts = Options::parse(parts)?;

        let (scheme, body) = target.split_once(':').unwrap_or((target, ""));

        match scheme.to_lowercase().as_str() {
            "exec" => exec(body, opts),
            "system" => Ok(Self::System {
                command: body.to_string(),
                dump: opts.dump,
                name: opts.name,
            }),
            "tcp" | "tcp-connect" | "connect" => tcp(body, opts),
            "tcplisten" | "tcp-listen" | "listen" => tcp_listen(body, opts),
            "stdio" => Ok(Self::Stdio {
                dump: opts.dump,
                name: opts.name,
            }),
            "unix" | "unix-connect" => Ok(Self::Unix {
                path: PathBuf::from(body),
                dump: opts.dump,
                name: opts.name,
            }),
            "file" => Ok(Self::File {
                path: PathBuf::from(body),
                append: opts.append,
                create: opts.create,
                truncate: opts.truncate,
                dump: opts.dump,
                name: opts.name,
            }),
            "unixlisten" | "unix-listen" => Ok(Self::UnixListen {
                path: PathBuf::from(body),
                name: opts.name,
                dump: opts.dump,
                fork: opts.fork,
                max_connections: opts.max_connections,
                unlink: opts.unlink,
                mode: opts.mode,
            }),
            other => Err(Self::Err::UnknownScheme(other.to_owned())),
        }
    }
}
