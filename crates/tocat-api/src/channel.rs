//! Side channels: the only way a plugin reaches the outside world.
//!
//! A plugin never opens a file. At build time it describes the sink it wants
//! and receives an opaque [`ChannelId`]. At run time it queues writes against
//! that id. The host owns the file descriptor, dedupes identical targets so two
//! plugins pointing at the same path share one buffered writer, and can refuse
//! a target outright (see stdout, below).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Opaque handle to a host-owned side channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChannelId(pub u32);

impl ChannelId {
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Where a side channel points.
///
/// Note the absence of stdout: on a `-` / stdio endpoint that stream carries
/// relay payload, and interleaving dump output into it would corrupt the
/// transfer. Hosts are expected to reject it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ChannelTarget {
    Stderr,
    File {
        path: PathBuf,
        #[serde(default)]
        append: bool,
    },
}

impl ChannelTarget {
    /// Create a File ChannelTarget from something path-like
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File {
            path: path.into(),
            append: true,
        }
    }
}

/// Implemented by the host. Handed to plugins during construction only.
pub trait HostBuilder {
    /// Reserve a side channel, returning a handle to write to later.
    ///
    /// Implementations should return the same [`ChannelId`] for equal targets.
    fn open_channel(&mut self, target: ChannelTarget) -> Result<ChannelId>;
}
