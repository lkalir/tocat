//! The plugin traits and the contexts they are driven through.
//!
//! The lifecycle is three calls and two contexts. [`PluginFactory::build`] runs
//! once per direction per connection with a [`BuildCtx`]: this is where config
//! is deserialized, side channels are reserved, and anything derived from the
//! stage's fixed position is cached. [`Plugin::on_bytes`] then runs per chunk
//! with a [`Ctx`], and [`Plugin::on_eof`] once at the end: the last chance for
//! a stage holding buffered bytes to emit them, and where a codec writes its
//! epilogue.
//!
//! The split between the two contexts is the point: everything expensive or
//! fallible belongs to build time, so the per-chunk path is a synchronous call
//! that either forwards a slice or writes into a buffer.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use crate::{
    Direction,
    channel::{ChannelId, ChannelTarget, HostBuilder},
    error::{PluginError, Result},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Where the host runs a stage.
///
/// `Inline` stages run on the reading task: one synchronous call per chunk, no
/// channel, no wakeup, and (see [`Ctx::pass_through`]) no copy. `Detached`
/// buys concurrency with the reader at the cost of one copy and one task hop
/// per chunk, so it pays off only for stages doing real work per byte:
/// compression, encryption, parsing, etc.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Execution {
    #[default]
    Inline,
    Detached,
}

/// What a stage decided to do with the chunk it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    /// Nothing emitted; the chunk stops here.
    Pending,
    /// Input forwarded verbatim. The host reuses the input slice, copying
    /// nothing.
    Passthrough,
    /// The stage wrote its own bytes into the output buffer.
    Buffered,
}

/// Collects the side effects a plugin asks for during one call.
///
/// The host implements this over per-channel staging buffers, so a side write
/// is an `extend_from_slice` and nothing else. Staged bytes are flushed to
/// their sinks concurrently with the downstream write.
pub trait EffectSink {
    fn write(&mut self, channel: ChannelId, bytes: &[u8]);
    /// `stage` is the emitting stage's display name, so the host can tag the
    /// line without every plugin having to repeat itself.
    fn log(&mut self, level: LogLevel, stage: &str, message: &str);
}

/// Static description of the path a pipeline instance sits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineMeta {
    pub direction: Direction,
    pub source: String,
    pub sink: String,
    pub peer: Option<String>,
}

impl PipelineMeta {
    pub fn new(direction: Direction, source: impl Into<String>, sink: impl Into<String>) -> Self {
        Self {
            direction,
            source: source.into(),
            sink: sink.into(),
            peer: None,
        }
    }

    #[must_use]
    pub fn with_peer(mut self, peer: Option<impl Into<String>>) -> Self {
        self.peer = peer.map(Into::into);
        self
    }

    /// The endpoint bytes are read from on this path.
    #[must_use]
    pub fn upstream(&self) -> &str {
        match self.direction {
            Direction::SourceToSink => &self.source,
            Direction::SinkToSource => &self.sink,
        }
    }

    /// The endpoint bytes are written to on this path.
    #[must_use]
    pub fn downstream(&self) -> &str {
        match self.direction {
            Direction::SourceToSink => &self.sink,
            Direction::SinkToSource => &self.source,
        }
    }

    /// `"source -> sink"`, oriented for this path.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} -> {}", self.upstream(), self.downstream())
    }
}

/// Where a stage sits in its pipeline, and what it is called.
///
/// `upstream` and `downstream` are the stage's actual neighbours on this path:
/// the adjacent stages' display names, or an endpoint name at either end. A
/// `tee` wedged between two other stages therefore describes the hop it is
/// really watching rather than the endpoints it is nowhere near.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageInfo<'a> {
    /// Position on this path, after direction filtering and mirroring.
    pub index: usize,
    pub total: usize,
    /// The `as = "..."` alias if one was given, otherwise the plugin name,
    /// suffixed with `#n` when the same name appears more than once.
    pub name: &'a str,
    pub upstream: &'a str,
    pub downstream: &'a str,
}

impl StageInfo<'_> {
    /// `"upstream -> downstream"`, oriented for this path.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} -> {}", self.upstream, self.downstream)
    }

    #[must_use]
    pub fn is_first(&self) -> bool {
        self.index == 0
    }

    #[must_use]
    pub fn is_last(&self) -> bool {
        self.index + 1 == self.total
    }
}

/// Handed to a plugin for each chunk.
///
/// A stage must say what happens to the chunk:
/// [`pass_through`](Self::pass_through) forwards it untouched,
/// [`forward`](Self::forward) emits different bytes, and doing neither drops
/// it. Passthrough is the fast path and costs nothing (the next stage receives
/// the same slice).
pub struct Ctx<'a> {
    meta: &'a PipelineMeta,
    stage: &'a str,
    input: &'a [u8],
    out: &'a mut Vec<u8>,
    emit: &'a mut Emit,
    sink: &'a mut dyn EffectSink,
}

impl<'a> Ctx<'a> {
    pub fn new(
        meta: &'a PipelineMeta,
        stage: &'a str,
        input: &'a [u8],
        out: &'a mut Vec<u8>,
        emit: &'a mut Emit,
        sink: &'a mut dyn EffectSink,
    ) -> Self {
        Self {
            meta,
            stage,
            input,
            out,
            emit,
            sink,
        }
    }

    /// This stage's display name, as it appears in logs.
    #[must_use]
    pub fn stage(&self) -> &str {
        self.stage
    }

    #[must_use]
    pub fn meta(&self) -> &PipelineMeta {
        self.meta
    }

    #[must_use]
    pub fn direction(&self) -> Direction {
        self.meta.direction
    }

    /// The chunk this call was given. Empty during [`Plugin::on_eof`].
    #[must_use]
    pub fn input(&self) -> &[u8] {
        self.input
    }

    /// Forward the input unchanged, without copying it.
    pub fn pass_through(&mut self) {
        match *self.emit {
            Emit::Pending => *self.emit = Emit::Passthrough,
            Emit::Passthrough => {}
            // Something was emitted already, so passthrough has to materialise.
            Emit::Buffered => self.out.extend_from_slice(self.input),
        }
    }

    /// Emit `bytes` downstream. Performs a copy. Use
    /// [`pass_through`](Self::pass_through) when the bytes are the input.
    pub fn forward(&mut self, bytes: &[u8]) {
        if *self.emit == Emit::Passthrough {
            self.out.extend_from_slice(self.input);
        }

        *self.emit = Emit::Buffered;
        self.out.extend_from_slice(bytes);
    }

    /// Explicitly swallow the chunk. Emitting nothing does the same thing; this
    /// exists so a filter can state the intent.
    pub fn drop_chunk(&mut self) {
        if *self.emit == Emit::Passthrough {
            *self.emit = Emit::Pending;
        }
    }

    /// Stage bytes for a side channel obtained from [`BuildCtx::open_channel`].
    pub fn side_write(&mut self, channel: ChannelId, bytes: &[u8]) {
        self.sink.write(channel, bytes);
    }

    pub fn log(&mut self, level: LogLevel, message: &str) {
        self.sink.log(level, self.stage, message);
    }
}

/// Handed to a [`PluginFactory`] while constructing one instance.
pub struct BuildCtx<'a> {
    name: &'a str,
    config: &'a Map<String, Value>,
    meta: &'a PipelineMeta,
    stage: StageInfo<'a>,
    host: &'a mut dyn HostBuilder,
}

impl<'a> BuildCtx<'a> {
    pub fn new(
        name: &'a str,
        config: &'a Map<String, Value>,
        meta: &'a PipelineMeta,
        stage: StageInfo<'a>,
        host: &'a mut dyn HostBuilder,
    ) -> Self {
        Self {
            name,
            config,
            meta,
            stage,
            host,
        }
    }

    /// Where this instance sits in the pipeline. Cache anything derived from
    /// it as the position cannot change after construction.
    #[must_use]
    pub fn stage(&self) -> StageInfo<'a> {
        self.stage
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.name
    }

    #[must_use]
    pub fn meta(&self) -> &PipelineMeta {
        self.meta
    }

    #[must_use]
    pub fn direction(&self) -> Direction {
        self.meta.direction
    }

    #[must_use]
    pub fn raw_config(&self) -> &Map<String, Value> {
        self.config
    }

    /// Deserialize the entry's options into the plugin's own config type.
    pub fn config<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(Value::Object(self.config.clone()))
            .map_err(|e| PluginError::config(self.name, e))
    }

    /// Reserve a side channel. Equal targets share one handle process-wide.
    pub fn open_channel(&mut self, target: ChannelTarget) -> Result<ChannelId> {
        self.host.open_channel(target)
    }
}

/// What a factory produced.
///
/// Most stages are [`Plugin`]s the host calls per chunk. A few cannot be: a
/// subprocess decides nothing synchronously, may emit nothing for the chunk it
/// was given, and may emit bytes belonging to three chunks ago. Rather than
/// bend [`Plugin`] into something a subprocess could satisfy (and lose the
/// property that makes it portable to WASM) such a stage is *described* here
/// and *run* by the host.
///
/// A WASM guest can only ever produce [`Stage::Filter`]; spawning is a host
/// capability by construction.
pub enum Stage {
    Filter(Box<dyn Plugin>),
    External(ExternalStage),
}

impl Stage {
    /// Build a filter stage from a plugin
    pub fn filter(plugin: impl Plugin + 'static) -> Self {
        Self::Filter(Box::new(plugin))
    }
}

impl From<Box<dyn Plugin>> for Stage {
    fn from(plugin: Box<dyn Plugin>) -> Self {
        Self::Filter(plugin)
    }
}

/// A subprocess to run as a stage, with the relay's bytes on its stdin and its
/// stdout continuing downstream.
///
/// Always its own segment: it cannot share a task with inline stages, and
/// `detach = false` on one is a contradiction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalStage {
    /// Program and arguments, or a single shell command when `shell` is set.
    pub argv: Vec<String>,
    pub shell: bool,
    pub stderr: StderrMode,
    /// Display name, for logs and for attributing the child's stderr.
    pub name: String,
}

/// What to do with a child's stderr.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StderrMode {
    /// Forward to the relay's own stderr. Interleaves with dumps and logs.
    Inherit,
    /// Capture and re-emit as warnings tagged with the stage name.
    #[default]
    Log,
    Null,
}

/// One stage of a pipeline. Instances are per-direction and per-connection.
///
/// Synchronous on purpose: it is the only shape that maps onto a WASM guest
/// call, and it keeps the per-chunk cost at a function call rather than a
/// future poll. Anything that must await belongs on the effect side, where the
/// host performs it off the critical path.
pub trait Plugin: Send {
    /// The name of the plugin
    fn name(&self) -> &str;

    /// A chunk arrived from upstream. `input` is the same slice as
    /// [`Ctx::input`]; it is passed separately because it is the hot argument.
    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()>;

    /// Upstream reached EOF. Last chance to emit buffered bytes.
    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        let _ = ctx;
        Ok(())
    }
}

/// Constructs [`Plugin`] instances from a declared entry.
pub trait PluginFactory: Send + Sync + 'static {
    fn name(&self) -> &str;

    fn description(&self) -> &str {
        ""
    }

    /// Default placement. Overridable per entry with `detach = true|false`.
    fn execution(&self) -> Execution {
        Execution::Inline
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage>;
}
