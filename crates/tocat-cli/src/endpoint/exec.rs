//! exec.rs: `exec:` and `system:`.
//!
//! Both relay through a child's stdin and stdout. They differ only in how the
//! command line is interpreted: `exec:` splits on whitespace and runs the
//! program directly, `system:` hands the whole string to `$SHELL -c` and gets
//! quoting, globbing and pipelines with it.
//!
//! The child's stderr is inherited rather than piped, so its diagnostics reach
//! the terminal instead of being relayed as payload. The `process` plugin is
//! the one that captures stderr, because a plugin sits mid-pipeline where
//! stray output would corrupt the stream.

use serde::{Deserialize, Serialize};
use tocat_api::StderrMode;

use crate::{
    child,
    endpoint::{
        Connection, EndpointStream,
        parse::{Opt, ParseEndpointError},
    },
};

#[derive(Debug, Deserialize, Serialize)]
pub struct Exec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct System {
    pub command: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Spawn and hand back the child's pipes as the endpoint's stream.
///
/// The child is reaped in the background: nothing here waits on it, so the
/// relay ends when the pipes close rather than when the process does.
async fn spawn(
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

impl Exec {
    const SCHEME: &'static str = "exec";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let argv: Vec<String> = body.split_whitespace().map(String::from).collect();
        if argv.is_empty() {
            return Err(ParseEndpointError::Empty);
        }

        let mut name = None;

        for opt in opts {
            match opt.key {
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self { argv, name })
    }

    /// Unlike the socket endpoints, an explicit `name` does not replace the
    /// label: the command line is what identifies the endpoint in a log.
    pub(super) fn label(&self) -> String {
        format!("EXEC({})", self.argv.join(" "))
    }

    pub(super) async fn connect(&self, buffer: usize) -> anyhow::Result<Connection> {
        let Some((program, args)) = self.argv.split_first() else {
            anyhow::bail!("exec: empty argv");
        };

        spawn(program, args, false, buffer).await
    }
}

impl System {
    const SCHEME: &'static str = "system";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        let mut name = None;

        for opt in opts {
            match opt.key {
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            command: body.to_string(),
            name,
        })
    }

    /// See [`Exec::label`]: the command is the identity.
    pub(super) fn label(&self) -> String {
        format!("SYSTEM({})", self.command)
    }

    pub(super) async fn connect(&self, buffer: usize) -> anyhow::Result<Connection> {
        spawn(&self.command, &[], true, buffer).await
    }
}
