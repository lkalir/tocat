//! unix.rs: `unix:` and `unix-listen:`.
//!
//! Binding leaves a path behind, so [`UnixListen::bind`] pairs with a
//! [`PathGuard`] that removes it on drop; the guard has to outlive the
//! connection or the socket is unlinked while it is still in use.
//!
//! `unlink` is about the *stale* path rather than the fresh one. Bind fails on
//! an existing path whether or not anything is listening on it, so with
//! `unlink` set the path is probed first: a refused connection means the owner
//! is gone and the path can be removed, while a successful one means a live
//! server and is an error.

use std::num::NonZeroUsize;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::net::{UnixListener, UnixStream};
use tracing::info;

use crate::endpoint::{
    Connection, EndpointStream,
    parse::{Opt, ParseEndpointError},
    sys::{Mode, PathGuard},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct Unix {
    pub path: std::path::PathBuf,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UnixListen {
    pub path: std::path::PathBuf,
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
            path: std::path::PathBuf::from(body),
            name,
        })
    }

    pub(super) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("unix://{}", self.path.display()))
    }

    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let stream = UnixStream::connect(&self.path)
            .await
            .with_context(|| format!("connecting to {}", self.path.display()))?;
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
            path: std::path::PathBuf::from(body),
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
            .unwrap_or_else(|| format!("unix://{}", self.path.display()))
    }

    /// Bind without accepting, for callers that own the accept loop.
    ///
    /// The caller is responsible for the [`PathGuard`]: this only creates the
    /// socket.
    pub async fn bind(&self) -> anyhow::Result<UnixListener> {
        if self.unlink && self.path.exists() {
            match UnixStream::connect(&self.path).await {
                Ok(_) => anyhow::bail!("{} is already in use", self.path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(&self.path).with_context(|| {
                        format!("removing stale socket {}", self.path.display())
                    })?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("probing {}", self.path.display()));
                }
            }
        }

        let listener = UnixListener::bind(&self.path)
            .with_context(|| format!("binding {}", self.path.display()))?;

        if let Some(mode) = self.mode {
            mode.apply(&self.path)?;
        }

        Ok(listener)
    }

    /// Bind and take a single peer.
    pub(super) async fn connect(&self) -> anyhow::Result<Connection> {
        let listener = self.bind().await?;
        let guard = PathGuard(self.path.clone());
        info!(path = %self.path.display(), "listening");
        let (stream, _) = listener.accept().await?;
        Ok(EndpointStream::unix(stream).into_connection_with_guard(guard))
    }
}
