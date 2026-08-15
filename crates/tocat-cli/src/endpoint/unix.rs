//! unix.rs: the `AF_UNIX` family, and everything the six schemes share.
//!
//! Three socket types, each in a dialled and a listening form. `unix:` and
//! `unix-listen:` are byte streams and live here. `seqpacket` holds the
//! connected message sockets and `dgram` the connectionless ones; both borrow
//! [`SocketPath`], [`unlink_stale`] and [`apply_mode`] from this file, which is
//! the only reason the address rules are stated once rather than three times.
//!
//! # Addresses
//!
//! A leading `@` names the abstract namespace: a Linux extension whose address
//! is the bytes after a leading NUL and which has no filesystem entry at all.
//! [`SocketPath`] is where that translation happens, once, for the compact CLI
//! grammar and the config file alike, so nothing downstream has to know which
//! spelling it was handed. Three things hang off
//! [`is_abstract`](SocketPath::is_abstract), and a fourth is a trap:
//!
//! * there is no stale path to unlink,
//! * there is no [`PathGuard`] to hold, since the kernel frees the name when
//!   the last socket holding it closes,
//! * there is no file to chmod, and
//! * there is **no permission check either**, so anything in the network
//!   namespace can connect. [`apply_mode`] refuses rather than ignoring a
//!   `mode` that would not be applied, because an operator who asked for `600`
//!   and silently got none is worse off than one who got an error.
//!
//! # unlink
//!
//! `unlink` is about the *stale* path rather than the fresh one. Bind fails on
//! an existing path whether or not anything is listening on it, so with
//! `unlink` set the path is probed first: a refused connection means the owner
//! is gone and the path can be removed, while a successful one means a live
//! server and is an error.

pub(super) mod dgram;
pub(super) mod seqpacket;

use std::{
    ffi::OsStr,
    future::Future,
    num::NonZeroUsize,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tocat_api::normalize;
use tokio::net::{UnixListener, UnixStream};
use tracing::info;

use crate::endpoint::{
    Connection, EndpointStream,
    parse::{Opt, ParseEndpointError},
    sys::{Mode, PathGuard},
};

/// The address of a unix socket, as written.
///
/// Held the way the kernel wants it: an abstract name is a path whose first
/// byte is NUL. That is also the form tokio's `UnixListener::bind` and
/// `UnixStream::connect` translate for themselves on Linux, which is why the
/// stream schemes pass [`as_path`](Self::as_path) straight to them. The
/// datagram schemes cannot, and go through [`addr`](Self::addr) instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPath(PathBuf);

impl SocketPath {
    /// The address as a spec or a config file wrote it, where a leading `@`
    /// asks for the abstract namespace.
    ///
    /// A file really called `@name` is still reachable, as `./@name` or by
    /// absolute path.
    pub(super) fn from_spec(body: &str) -> Self {
        match body.strip_prefix('@') {
            Some(name) => {
                let mut bytes = Vec::with_capacity(name.len() + 1);
                bytes.push(0);
                bytes.extend_from_slice(name.as_bytes());

                Self(PathBuf::from(OsStr::from_bytes(&bytes)))
            }
            None => Self(PathBuf::from(body)),
        }
    }

    /// A path this process chose rather than one a user wrote, taken
    /// literally: a leading `@` here is part of the filename.
    pub(super) fn from_path(path: PathBuf) -> Self {
        Self(path)
    }

    pub(super) fn as_path(&self) -> &Path {
        &self.0
    }

    /// True for an address in the abstract namespace, which has no filesystem
    /// entry. Every filesystem operation in this module asks first.
    pub fn is_abstract(&self) -> bool {
        self.abstract_name().is_some()
    }

    fn abstract_name(&self) -> Option<&[u8]> {
        self.0.as_os_str().as_bytes().strip_prefix(b"\0")
    }

    /// The guard that removes this path when the connection ends, if there is
    /// a path to remove.
    ///
    /// `None` for an abstract name: the kernel reclaims it when the last
    /// socket holding it closes, and there is no file whose removal would mean
    /// anything.
    pub fn guard(&self) -> Option<PathGuard> {
        (!self.is_abstract()).then(|| PathGuard(self.0.clone()))
    }

    /// The address in the form the datagram schemes need.
    ///
    /// The stream schemes never call this. tokio's `UnixListener::bind` and
    /// `UnixStream::connect` map a leading NUL to an abstract address on their
    /// own, while its datagram entry points take a path and reject the NUL
    /// byte an abstract name starts with, so those go through std's
    /// address-taking constructors instead.
    pub(super) fn addr(&self) -> std::io::Result<std::os::unix::net::SocketAddr> {
        #[cfg(target_os = "linux")]
        if let Some(name) = self.abstract_name() {
            use std::os::linux::net::SocketAddrExt as _;

            return std::os::unix::net::SocketAddr::from_abstract_name(name);
        }

        std::os::unix::net::SocketAddr::from_pathname(&self.0)
    }

    /// Reject an address this platform cannot express, here rather than at
    /// bind, where it would arrive as an error about NUL bytes in a path.
    pub(super) fn supported(&self) -> anyhow::Result<()> {
        #[cfg(not(target_os = "linux"))]
        if self.is_abstract() {
            anyhow::bail!("{self} names the abstract namespace, which only Linux has");
        }

        Ok(())
    }
}

impl std::fmt::Display for SocketPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.abstract_name() {
            Some(name) => write!(f, "@{}", String::from_utf8_lossy(name)),
            None => write!(f, "{}", self.0.display()),
        }
    }
}

impl<'de> Deserialize<'de> for SocketPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_spec(&String::deserialize(deserializer)?))
    }
}

impl Serialize for SocketPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.is_abstract() {
            // Round trips, and has to: a NUL byte is not expressible in TOML,
            // so `@name` is the only way `--dump-config` can write this back.
            serializer.serialize_str(&self.to_string())
        } else {
            self.0.serialize(serializer)
        }
    }
}

/// Clear a path left behind by a dead server, and refuse to touch a live one.
///
/// `probe` is the connect that tells them apart, supplied by the caller
/// because each socket type has its own. It is not polled unless there is
/// something there to probe.
pub(super) async fn unlink_stale(
    path: &SocketPath,
    probe: impl Future<Output = std::io::Result<()>>,
) -> anyhow::Result<()> {
    // An abstract name is freed when its owner closes, so there is never a
    // stale one, and nothing on the filesystem to remove if there were.
    if path.is_abstract() || !path.as_path().exists() {
        return Ok(());
    }

    match probe.await {
        Ok(()) => anyhow::bail!("{path} is already in use"),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path.as_path())
                .with_context(|| format!("removing stale socket {path}"))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("probing {path}")),
    }

    Ok(())
}

/// Apply the permissions a spec asked for, refusing where they would be a lie.
///
/// After bind rather than before: bind masks its mode with the umask, so a
/// chmod afterwards is the only way to land on exactly the requested bits.
pub(super) fn apply_mode(path: &SocketPath, mode: Option<Mode>) -> anyhow::Result<()> {
    let Some(mode) = mode else {
        return Ok(());
    };

    if path.is_abstract() {
        anyhow::bail!(
            "mode cannot be applied to {path}: an abstract name has no filesystem entry and no \
             permission check, so anything in the network namespace can reach it",
        );
    }

    mode.apply(path.as_path())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Unix {
    pub path: SocketPath,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UnixListen {
    pub path: SocketPath,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub fork: bool,
    #[serde(default, rename = "max-connections")]
    pub max_connections: Option<NonZeroUsize>,
    #[serde(default)]
    pub unlink: bool,
    #[serde(default)]
    pub mode: Option<Mode>,
}

impl Unix {
    const SCHEME: &'static str = "unix";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut name = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            path: SocketPath::from_spec(body),
            name,
        })
    }

    pub(super) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("unix://{}", self.path))
    }

    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        self.path.supported()?;

        let stream = UnixStream::connect(self.path.as_path())
            .await
            .with_context(|| format!("connecting to {}", self.path))?;

        Ok(EndpointStream::unix(stream).into_connection())
    }
}

impl UnixListen {
    const SCHEME: &'static str = "unix-listen";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut name = None;
        let mut fork = false;
        let mut max_connections = None;
        let mut unlink = false;
        let mut mode = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "fork" => fork = opt.flag()?,
                "maxconnections" | "maxconn" => {
                    max_connections = Some(opt.count()?);
                }
                "mode" => mode = Some(opt.mode()?),
                "name" => name = Some(opt.string()?),
                "unlink" => unlink = opt.flag()?,
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            path: SocketPath::from_spec(body),
            name,
            fork,
            max_connections,
            unlink,
            mode,
        })
    }

    pub(super) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("unix://{}", self.path))
    }

    /// Bind without accepting, for callers that own the accept loop.
    ///
    /// The caller is responsible for the [`PathGuard`]: this only creates the
    /// socket.
    pub async fn bind(&self) -> anyhow::Result<UnixListener> {
        self.path.supported()?;

        if self.unlink {
            unlink_stale(&self.path, async {
                UnixStream::connect(self.path.as_path()).await.map(drop)
            })
            .await?;
        }

        let listener = UnixListener::bind(self.path.as_path())
            .with_context(|| format!("binding {}", self.path))?;

        apply_mode(&self.path, self.mode)?;

        Ok(listener)
    }

    /// Bind and take a single peer.
    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let listener = self.bind().await?;
        let guard = self.path.guard();
        info!(path = %self.path, "listening");
        let (stream, _) = listener.accept().await?;

        Ok(EndpointStream::unix(stream).into_connection_with_guard(guard))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointSpec;

    fn dial(s: &str) -> Unix {
        match s.parse::<EndpointSpec>().expect("parses") {
            EndpointSpec::Unix(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn listen(s: &str) -> UnixListen {
        match s.parse::<EndpointSpec>().expect("parses") {
            EndpointSpec::UnixListen(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_plain_path_is_a_path() {
        let path = SocketPath::from_spec("/run/app/app.sock");

        assert!(!path.is_abstract());
        assert_eq!(path.as_path(), Path::new("/run/app/app.sock"));
        assert_eq!(path.to_string(), "/run/app/app.sock");
    }

    /// The kernel wants a leading NUL, and everything above wants the `@` back.
    #[test]
    fn an_at_sign_becomes_the_abstract_namespace() {
        let path = SocketPath::from_spec("@tocat");

        assert!(path.is_abstract());
        assert_eq!(path.as_path().as_os_str().as_bytes(), b"\0tocat");
        assert_eq!(path.to_string(), "@tocat");
    }

    /// A file really called `@name` has to stay reachable.
    #[test]
    fn a_relative_at_sign_is_still_a_file() {
        assert!(!SocketPath::from_spec("./@tocat").is_abstract());
        assert!(!SocketPath::from_path(PathBuf::from("@tocat")).is_abstract());
    }

    /// The guard unlinks the path when the relay ends, and an abstract name
    /// has none: holding one would remove an unrelated file.
    #[test]
    fn only_a_real_path_has_a_guard() {
        assert!(SocketPath::from_spec("/tmp/tocat.sock").guard().is_some());
        assert!(SocketPath::from_spec("@tocat").guard().is_none());
    }

    /// Both spellings reach the same place, so the config file is not the one
    /// form that cannot name an abstract socket.
    #[test]
    fn the_table_form_reads_addresses_the_same_way() {
        let spec: EndpointSpec =
            toml::from_str("type = \"unix-listen\"\npath = \"@tocat\"").expect("deserialises");

        match spec {
            EndpointSpec::UnixListen(e) => assert!(e.path.is_abstract()),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `--dump-config` has to write back something that parses again, and a
    /// NUL byte is not expressible in TOML.
    #[test]
    fn an_abstract_address_round_trips_through_toml() {
        let encoded = toml::to_string(&Unix {
            path: SocketPath::from_spec("@tocat"),
            name: Some("control".to_owned()),
        })
        .expect("serialises");

        assert!(encoded.contains("\"@tocat\""), "{encoded}");
    }

    #[test]
    fn the_listening_options_are_all_accepted() {
        let e = listen("unix-listen:/tmp/tocat.sock,fork,unlink,mode=660,max-conn=4");

        assert!(e.fork);
        assert!(e.unlink);
        assert_eq!(e.max_connections, NonZeroUsize::new(4));
        assert_eq!(e.mode, Some("660".parse().expect("valid mode")));
    }

    /// Dialling has nothing to bind, so a binding option is a mistake rather
    /// than a no-op.
    #[test]
    fn dialling_rejects_the_listening_options() {
        assert!(matches!(
            "unix:/tmp/tocat.sock,fork"
                .parse::<EndpointSpec>()
                .expect_err("rejected"),
            ParseEndpointError::UnsupportedOption { .. }
        ));

        assert_eq!(dial("unix:/tmp/tocat.sock,name=control").label(), "control");
    }
}
