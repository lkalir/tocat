//! sys.rs: the system plumbing no single endpoint owns.
//!
//! Three things live here because more than one endpoint needs them:
//! [`Mode`], the octal permission bits `pipe:` and `unix-listen:` can apply to
//! the path they create; [`PathGuard`], which removes a path on drop; and
//! [`size_if_pipe`], the best-effort pipe resize used wherever a descriptor
//! might turn out to be a pipe.

use std::{os::unix::fs::PermissionsExt, path::PathBuf, str::FromStr};

use anyhow::Context;
use serde::{Deserialize, Serialize, Serializer, de::Error as _};

use crate::endpoint::parse::ParseEndpointError;

/// Permission bits for a path this process creates.
///
/// Held as raw octal so it round-trips through the config file the way it was
/// written: `mode = "660"`, not `mode = 432`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Mode(u32);

impl Mode {
    /// Apply the bits to `path`.
    ///
    /// Both `mkfifo` and `bind` mask their mode argument with the umask, so
    /// the only way to land on exactly the requested bits is to chmod after
    /// the fact.
    pub(super) fn apply(self, path: &std::path::Path) -> anyhow::Result<()> {
        std::fs::set_permissions(path, PermissionsExt::from_mode(self.0))
            .with_context(|| format!("chmod {:o} on {}", self.0, path.display()))
    }
}

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

/// Removes a path on drop: a bound unix socket, or a FIFO opened with
/// `unlink`. Must outlive the connection using it.
pub struct PathGuard(pub PathBuf);

impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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
    use tracing::{debug, warn};

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

/// `#[serde(default = "...")]` for the options that are on unless disabled.
pub(super) fn default_true() -> bool {
    true
}
