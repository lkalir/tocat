//! child.rs: spawning subprocesses.
//!
//! Shared by the `exec:`/`system:` endpoints and the `process` plugin, which
//! want the same child with different stderr handling.

use std::process::Stdio;

use anyhow::Context;
use tocat_api::StderrMode;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tracing::{debug, warn};

use crate::endpoint::size_if_pipe;

pub struct ChildParts {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    /// Present only when `stderr` was [`StderrMode::Log`].
    pub stderr: Option<ChildStderr>,
}

/// Spawn with stdin and stdout piped.
pub fn spawn(
    program: &str,
    args: &[String],
    shell: bool,
    stderr: StderrMode,
    pipe_size: usize,
) -> anyhow::Result<ChildParts> {
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

    // `kill_on_drop` matters here: if the relay tears down while the child is
    // blocked writing to a stdout nobody is draining, nothing else will end it.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(match stderr {
            StderrMode::Inherit => Stdio::inherit(),
            StderrMode::Log => Stdio::piped(),
            StderrMode::Null => Stdio::null(),
        })
        .kill_on_drop(true);

    let mut child = cmd.spawn().with_context(|| format!("spawning {program}"))?;

    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");

    // A child's stdin and stdout are always pipes, and their 64 KiB default
    // would cap the relay however large the copy buffer is.
    size_if_pipe(&stdin, "child stdin", pipe_size);
    size_if_pipe(&stdout, "child stdout", pipe_size);

    Ok(ChildParts {
        stdin,
        stdout,
        stderr: child.stderr.take(),
        child,
    })
}

/// Reap in the background, reporting a non-zero exit.
///
/// For endpoints only. A `process` stage waits on its child directly, because
/// there a bad exit means the bytes it produced were wrong and the direction
/// should fail rather than log.
pub fn reap_in_background(mut child: Child) {
    let pid = child.id();

    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) if status.success() => debug!(?pid, "child exited"),
            Ok(status) => warn!(?pid, %status, "child exited non-zero"),
            Err(e) => warn!(?pid, error = %e, "waiting on child failed"),
        }
    });
}
