//! Public plugin API for tocat.
//!
//! A plugin is a synchronous byte transformer. It is handed a chunk of bytes
//! that arrived from upstream and decides what to forward downstream. Anything
//! that touches the outside world (writing a dump file, emitting a log line)
//! is *not* performed by the plugin. It is queued as an [`Effect`] and applied
//! by the host after the call returns.
//!
//! That split is deliberate. It keeps plugins pure and trivially testable, it
//! keeps all I/O on the host's async runtime, and it is the shape a WASM guest
//! has to take anyway (guest calls a host import, host performs the syscall).
//! A future `WasmPlugin` implements [`Plugin`] like any other; nothing in the
//! relay needs to change.
//!
//! The same split covers time. A stage cannot await and cannot read a clock (a
//! guest has no way to reach one) so a stage that needs time rather than
//! traffic to drive it declares a period with [`Plugin::tick_interval`] and is
//! called back through [`Plugin::on_tick`]. The host holds the timer and
//! decides when anyone is due.
//!
//! # Composition
//!
//! Plugins are declared once and instantiated per direction. A declaration list
//! `[a, b]` with `direction = "both"` produces:
//!
//! ```text
//! source --> a --> b --> sink        (Direction::SourceToSink)
//! source <-- a <-- b <-- sink        (Direction::SinkToSource)
//! ```
//!
//! The reverse pipeline is the *mirror* of the declaration order, so wrapping
//! plugins (framing, compression, encryption) nest correctly without the user
//! having to write the pipeline out twice. Each direction gets its own
//! instance, so per-direction state (byte offsets, codec state) never leaks
//! across paths.

pub mod channel;
pub mod error;
pub mod pipeline;
pub mod plugin;

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use crate::{
    channel::{ChannelId, ChannelTarget, HostBuilder},
    error::{PluginError, Result},
    pipeline::{Chain, Pipeline, Registry, Segment},
    plugin::{
        BuildCtx, Ctx, EffectSink, Emit, Execution, ExternalStage, LogLevel, PipelineMeta, Plugin,
        PluginFactory, Stage, StageInfo, StderrMode,
    },
};

/// One of the two byte paths through the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Bytes read from the source, written to the sink.
    #[serde(alias = "forward", alias = "src-to-sink", alias = "source-to-sink")]
    SourceToSink,
    /// Bytes read from the sink, written to the source.
    #[serde(alias = "reverse", alias = "sink-to-src", alias = "sink-to-source")]
    SinkToSource,
}

impl Direction {
    pub const ALL: [Direction; 2] = [Direction::SourceToSink, Direction::SinkToSource];

    #[must_use]
    pub fn flip(self) -> Self {
        match self {
            Direction::SourceToSink => Direction::SinkToSource,
            Direction::SinkToSource => Direction::SourceToSink,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::SourceToSink => "source-to-sink",
            Direction::SinkToSource => "sink-to-source",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which path(s) a declared plugin applies to.
///
/// [`DirectionSpec::Both`] instantiates the plugin twice — once per direction —
/// rather than sharing one instance between them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DirectionSpec {
    #[serde(alias = "forward", alias = "source", alias = "src-to-sink")]
    SourceToSink,
    #[serde(alias = "reverse", alias = "sink", alias = "sink-to-src")]
    SinkToSource,
    #[default]
    #[serde(
        alias = "bidi",
        alias = "bidirectional",
        alias = "duplex",
        alias = "all"
    )]
    Both,
}

impl DirectionSpec {
    #[must_use]
    pub fn contains(self, direction: Direction) -> bool {
        matches!(
            (self, direction),
            (DirectionSpec::Both, _)
                | (DirectionSpec::SourceToSink, Direction::SourceToSink)
                | (DirectionSpec::SinkToSource, Direction::SinkToSource)
        )
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DirectionSpec::SourceToSink => "source-to-sink",
            DirectionSpec::SinkToSource => "sink-to-source",
            DirectionSpec::Both => "both",
        }
    }
}

impl fmt::Display for DirectionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDirectionError(pub String);

impl fmt::Display for ParseDirectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown direction {:?}; expected one of: source-to-sink, sink-to-source, both",
            self.0
        )
    }
}

impl std::error::Error for ParseDirectionError {}

impl FromStr for DirectionSpec {
    type Err = ParseDirectionError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "source-to-sink" | "src-to-sink" | "forward" | "source" | "src" | "out" => {
                Ok(DirectionSpec::SourceToSink)
            }
            "sink-to-source" | "sink-to-src" | "reverse" | "sink" | "in" => {
                Ok(DirectionSpec::SinkToSource)
            }
            "both" | "bidi" | "bidirectional" | "duplex" | "all" => Ok(DirectionSpec::Both),
            _ => Err(ParseDirectionError(s.to_string())),
        }
    }
}

/// A declared pipeline entry: which plugin, on which path, with what config.
///
/// In TOML the plugin's own options are flattened alongside `name` and
/// `direction`:
///
/// ```toml
/// [[plugin]]
/// name = "tee"
/// direction = "both"
/// file = "dump.log"
/// format = "hex"
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PluginSpec {
    #[serde(alias = "plugin", alias = "use")]
    pub name: String,
    #[serde(default)]
    pub direction: DirectionSpec,
    /// A name for this instance, used in logs and as the default label for
    /// stages that print one. Without it a stage is called after its plugin,
    /// with `#n` appended when the same plugin appears twice on one path.
    #[serde(default, rename = "as")]
    pub alias: Option<String>,
    /// Override the plugin's default placement. `true` runs this stage on its
    /// own task behind a bounded channel.
    #[serde(default)]
    pub detach: Option<bool>,
    /// Plugin-defined options, opaque to the host.
    #[serde(flatten, default)]
    pub config: Map<String, Value>,
}

impl PluginSpec {
    pub fn new(name: impl Into<String>, direction: DirectionSpec) -> Self {
        Self {
            name: name.into(),
            direction,
            alias: None,
            detach: None,
            config: Map::new(),
        }
    }

    #[must_use]
    pub fn named(mut self, alias: impl Into<String>) -> Self {
        self.alias = Some(alias.into());
        self
    }

    #[must_use]
    pub fn detached(mut self, detach: bool) -> Self {
        self.detach = Some(detach);
        self
    }

    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.config.insert(key.into(), value.into());
        self
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Value>) -> &mut Self {
        self.config.insert(key.into(), value.into());
        self
    }
}

impl fmt::Display for PluginSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.alias {
            Some(alias) => write!(f, "{} ({}:{})", alias, self.name, self.direction),
            None => write!(f, "{}:{}", self.name, self.direction),
        }
    }
}
