//! The plugin traits and the contexts they are driven through.
//!
//! The lifecycle is four calls and two contexts. [`PluginFactory::build`] runs
//! once per direction per connection with a [`BuildCtx`]: this is where config
//! is deserialized, side channels are reserved, and anything derived from the
//! stage's fixed position is cached. [`Plugin::on_bytes`] then runs per chunk
//! with a [`Ctx`], [`Plugin::on_tick`] runs on a schedule the stage asks for,
//! and [`Plugin::on_eof`] once at the end: the last chance for a stage holding
//! buffered bytes to emit them, and where a codec writes its epilogue.
//!
//! The split between the two contexts is the point: everything expensive or
//! fallible belongs to build time, so the per-chunk path is a synchronous call
//! that either forwards a slice or writes into a buffer.
//!
//! A call emits one unit by default, however many times it forwards: the
//! pieces concatenate, and the host delivers them as one write. A stage that
//! needs them kept apart says so with [`Ctx::boundary`], which is what turns a
//! stage that merely accumulates bytes into one that records them.

use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
/// Severity of a record a stage asked the host to log.
///
/// The same type a guest writes into its outbox, since a level that crossed
/// the WebAssembly boundary and a level a native plugin passed to
/// [`Ctx::log`] are the same thing.
pub use tocat_wasm_abi::Level as LogLevel;
pub use tocat_wasm_abi::*;

use crate::{
    Direction,
    channel::{ChannelId, ChannelTarget, HostBuilder},
    error::{PluginError, Result},
    forgiving::Forgiving,
};

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

    /// Wait `delay` before reading upstream again.
    ///
    /// The one effect that acts on the reader rather than on a side channel. A
    /// stage cannot sleep: it is synchronous, it runs on the reading task, and
    /// a guest has no runtime to sleep on. So it asks, and the host holds off
    /// its next read. Several stages asking on one chunk get the longest of
    /// the requests, not the sum.
    ///
    /// Defaults to doing nothing, so a host that has no reader to hold (a test
    /// harness, an offline driver) is not obliged to honour it.
    fn pace(&mut self, delay: Duration) {
        let _ = delay;
    }

    /// Stop reading upstream, as if it had just reached end of stream.
    ///
    /// Everything already emitted is still written, `on_eof` still cascades,
    /// and the path closes down its normal way. This is how a stage ends a
    /// transfer deliberately, which is not a failure and must not be reported
    /// as one.
    fn halt(&mut self, stage: &str, reason: &str) {
        let _ = (stage, reason);
    }
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

/// What one call to a stage produced: the bytes it emitted, how they are
/// framed, and what it asked the host to do about its own schedule.
///
/// One struct rather than four borrows because they are written together and
/// read together, and because a stage's whole answer is exactly these four
/// things. The host keeps one per buffer and resets it between calls, so a
/// steady stream allocates nothing here after the first few chunks.
#[derive(Debug, Default)]
pub struct Emission {
    /// Bytes the stage wrote. Empty when it passed through or emitted nothing.
    pub(crate) out: Vec<u8>,
    /// One offset per unit, each the end of its unit. Empty means unframed,
    /// which is one unit covering everything.
    pub(crate) bounds: Vec<usize>,
    pub(crate) emit: Emit,
    /// Whether the stage asked for its tick schedule to be restarted.
    pub(crate) rearm: bool,
}

impl Emission {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ready for a fresh call, keeping the allocations.
    pub fn reset(&mut self) {
        self.out.clear();
        self.bounds.clear();
        self.emit = Emit::Pending;
        self.rearm = false;
    }

    /// Ready for the next unit of a framed call.
    ///
    /// What has been emitted so far stays, because units concatenate into one
    /// buffer, but the decision is per unit. A rearm request is not cleared
    /// either: it is about the stage, not about the unit.
    pub(crate) fn next_unit(&mut self) {
        self.emit = Emit::Pending;
    }

    /// Close the unit left open, so `bounds` always ends where `out` does.
    ///
    /// Does nothing when no bytes have arrived since the last boundary, so a
    /// stage cannot declare an empty unit, and nothing at all on an empty
    /// emission. Note that this frames an emission that had declared no
    /// framing, so the host calls it only where that is what it means.
    pub(crate) fn close(&mut self) {
        if self.bounds.last().copied().unwrap_or(0) < self.out.len() {
            self.bounds.push(self.out.len());
        }
    }

    /// Everything the stage emitted, concatenated.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.out
    }

    /// The framing of [`bytes`](Self::bytes). Empty means one unit.
    #[must_use]
    pub fn bounds(&self) -> &[usize] {
        &self.bounds
    }

    /// What the stage did with what it was given.
    #[must_use]
    pub fn emit(&self) -> Emit {
        self.emit
    }

    /// Whether the stage asked for its schedule to be restarted. See
    /// [`Ctx::rearm`].
    #[must_use]
    pub fn rearm_requested(&self) -> bool {
        self.rearm
    }
}

/// Handed to a plugin for each chunk.
///
/// A stage must say what happens to the chunk:
/// [`pass_through`](Self::pass_through) forwards it untouched,
/// [`forward`](Self::forward) emits different bytes, and doing neither drops
/// it. Passthrough is the fast path and costs nothing (the next stage receives
/// the same slice).
///
/// Several calls to `forward` in one turn emit one unit, not several: the
/// bytes concatenate and are delivered together. [`boundary`](Self::boundary)
/// is how a stage says otherwise.
pub struct Ctx<'a> {
    meta: &'a PipelineMeta,
    stage: &'a str,
    input: &'a [u8],
    emission: &'a mut Emission,
    sink: &'a mut dyn EffectSink,
}

impl<'a> Ctx<'a> {
    pub fn new(
        meta: &'a PipelineMeta,
        stage: &'a str,
        input: &'a [u8],
        emission: &'a mut Emission,
        sink: &'a mut dyn EffectSink,
    ) -> Self {
        Self {
            meta,
            stage,
            input,
            emission,
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
        match self.emission.emit {
            Emit::Pending => self.emission.emit = Emit::Passthrough,
            Emit::Passthrough => {}
            // Something was emitted already, so passthrough has to materialise.
            Emit::Buffered => {
                let input = self.input;
                self.emission.out.extend_from_slice(input);
            }
        }
    }

    /// Emit `bytes` downstream. Performs a copy. Use
    /// [`pass_through`](Self::pass_through) when the bytes are the input.
    ///
    /// Appends: calling this twice emits both, in order, as one unit. Call
    /// [`boundary`](Self::boundary) between them to emit two.
    pub fn forward(&mut self, bytes: &[u8]) {
        if self.emission.emit == Emit::Passthrough {
            let input = self.input;
            self.emission.out.extend_from_slice(input);
        }

        self.emission.emit = Emit::Buffered;
        self.emission.out.extend_from_slice(bytes);
    }

    /// Explicitly swallow the chunk. Emitting nothing does the same thing; this
    /// exists so a filter can state the intent.
    pub fn drop_chunk(&mut self) {
        if self.emission.emit == Emit::Passthrough {
            self.emission.emit = Emit::Pending;
        }
    }

    /// End the current unit. What was forwarded since the last boundary is
    /// delivered on its own: one write at a byte sink, one message at a
    /// datagram sink, one parcel across a detached boundary, and one
    /// [`on_bytes`](Plugin::on_bytes) call at every stage below.
    ///
    /// Only worth calling when those splits are the point, as with a stage
    /// cutting a stream into fixed-size records. Framing is not free: each
    /// stage below is then called once per unit rather than once per chunk, so
    /// a stage that emits many small units is asking the rest of the segment
    /// to run many times. A stage that only rewrites bytes should leave the
    /// framing it was given alone and say nothing.
    ///
    /// The trailing unit does not need one: whatever is forwarded after the
    /// last boundary is closed automatically, so no bytes can be lost by
    /// forgetting.
    ///
    /// Ignored when nothing has been forwarded since the last call, so a stage
    /// cannot emit an empty unit by accident.
    pub fn boundary(&mut self) {
        if self.emission.emit == Emit::Passthrough {
            let input = self.input;
            self.emission.out.extend_from_slice(input);
            self.emission.emit = Emit::Buffered;
        }

        self.emission.close();
    }

    /// Restart this stage's tick schedule: the next
    /// [`on_tick`](Plugin::on_tick) falls a full interval from now rather than
    /// wherever the existing cadence happens to land.
    ///
    /// A stage cannot read a clock, so it cannot measure how long it has been
    /// holding something. What it can do is say when the waiting started, and
    /// this is how. A stage that begins accumulating calls this, and its next
    /// tick then means "an interval since you asked" rather than "an interval
    /// since some earlier moment you know nothing about".
    ///
    /// Without it, [`tick_interval`](Plugin::tick_interval) is a cadence
    /// rather than a delay: a tick that came due while bytes were flowing
    /// fires at the next opportunity, which can be immediately after the bytes
    /// it is about arrived.
    ///
    /// Cheap to call, and harmless to call often: it sets a flag the host
    /// reads once at the end of the call. Ignored for a stage that asked for
    /// no ticks.
    pub fn rearm(&mut self) {
        self.emission.rearm = true;
    }

    /// Stage bytes for a side channel obtained from [`BuildCtx::open_channel`].
    pub fn side_write(&mut self, channel: ChannelId, bytes: &[u8]) {
        self.sink.write(channel, bytes);
    }

    pub fn log(&mut self, level: LogLevel, message: &str) {
        self.sink.log(level, self.stage, message);
    }

    /// Ask the host to wait `delay` before reading upstream again.
    ///
    /// Applied after this call returns and after whatever was emitted has been
    /// written, so the bytes in hand are never held hostage by the wait. On a
    /// socket this is real backpressure: the read stops, the receive buffer
    /// fills, the window closes and the peer slows down. Nothing is buffered
    /// on this side.
    pub fn pace(&mut self, delay: Duration) {
        self.sink.pace(delay);
    }

    /// Ask the host to stop reading upstream, as if it had reached end of
    /// stream. `reason` is logged against this stage.
    pub fn halt(&mut self, reason: &str) {
        self.sink.halt(self.stage, reason);
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
    ///
    /// Through [`Forgiving`], so option keys and enum values are matched the
    /// way every other identifier in tocat is: case-insensitively, with dashes
    /// and underscores treated as noise. A plugin declares its config exactly
    /// as it would otherwise, including `deny_unknown_fields`, and needs to
    /// know nothing about this.
    pub fn config<T: DeserializeOwned>(&self) -> Result<T> {
        T::deserialize(Forgiving(Value::Object(self.config.clone())))
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
    ///
    /// One call is one unit. Where a stage above declared framing with
    /// [`Ctx::boundary`] that is one call per unit rather than one per chunk,
    /// so a stage never has to unpick two of them from a single slice.
    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()>;

    /// Upstream reached EOF. Last chance to emit buffered bytes.
    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// How often this stage wants [`on_tick`](Plugin::on_tick) called, or
    /// `None` (the default) for never.
    ///
    /// Read once, at the end of construction, so it must not depend on
    /// anything that changes later. A stage whose interval is configurable
    /// reads its config in [`PluginFactory::build`] and answers from that.
    ///
    /// The host owns the clock. A guest cannot read one (a WASM module has no
    /// way to reach the host's time) which is why this is a period the stage
    /// *asks for* rather than a timestamp it checks. The cost falls on the
    /// relay: one timer per direction per connection for any pipeline
    /// containing a ticking stage, so a stage asking for milliseconds is
    /// asking every forked connection to wake up that often.
    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    /// The stage's schedule came due.
    ///
    /// Called from the same task, and under the same rules, as
    /// [`on_bytes`](Plugin::on_bytes), it just arrives without any. This is
    /// how a stage does anything that time rather than traffic should drive:
    /// report a measurement, release bytes it has been holding back, emit a
    /// keepalive. Without it a stalled stream and a finished one are
    /// indistinguishable from inside a plugin.
    ///
    /// [`Ctx::input`] is empty, so there is nothing to pass through; anything
    /// emitted here is emitted with [`Ctx::forward`] and continues downstream
    /// through the stages *below* this one, in the same way
    /// [`on_eof`](Plugin::on_eof) cascades. Emitting nothing is the common
    /// case and costs nothing.
    ///
    /// A stage that emits from here is fabricating a message boundary on a
    /// datagram path (the bytes belong to no datagram the peer sent) so it
    /// should report [`Boundaries::Fuse`] from
    /// [`boundaries`](Plugin::boundaries). A stage that only observes need
    /// not.
    ///
    /// What is emitted here is one unit unless [`Ctx::boundary`] says
    /// otherwise, exactly as in [`on_bytes`](Plugin::on_bytes).
    ///
    /// Ticks run for the life of the pipeline and stop at end of stream, so
    /// they arrive whether or not anything is moving, which is the point, and
    /// is what a keepalive needs. A stage that has nothing to say until the
    /// first chunk has arrived is expected to keep that state itself.
    fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// What this stage does to the message boundaries passing through it.
    ///
    /// On a byte stream a chunk is an arbitrary slice: a stage may buffer,
    /// split or coalesce freely, and the host is free to do the same. On a
    /// datagram path the chunk *is* the message: one `on_bytes` call per
    /// datagram, and whatever it emits is sent as exactly one datagram. A
    /// stage that buffers across calls, or emits two messages' worth from one,
    /// silently corrupts the protocol unless it says so here.
    ///
    /// The four answers, in the order a stage usually wants them:
    ///
    /// - [`Preserve`](Boundaries::Preserve): one unit in, one unit out. Every
    ///   observer, and every codec that rewrites a message in place.
    /// - [`Fuse`](Boundaries::Fuse): the units are gone below this stage.
    ///   Anything that buffers across calls, splits, coalesces, or emits from a
    ///   tick.
    /// - [`Seal`](Boundaries::Seal): as `Preserve`, and the boundary is also
    ///   written into the payload, so it outlives a stage below that fuses.
    ///   `frame` and nothing else.
    /// - [`Split`](Boundaries::Split): the units below are read out of the
    ///   bytes rather than inherited, so the ones from above do not survive.
    ///   `unframe` and nothing else.
    ///
    /// Defaults to `Fuse` because that is the answer that claims nothing,
    /// which is the safe one for a stage that has not thought about it,
    /// including any plugin loaded from outside this binary.
    ///
    /// Declaring the truth matters more than declaring safety. `block` fuses
    /// and says so, and it is still the right stage to reach for when one
    /// datagram per 1400 bytes is exactly what was wanted: the host warns and
    /// relays anyway.
    fn boundaries(&self) -> Boundaries {
        Boundaries::Fuse
    }

    /// What this stage needs of the path it was placed on.
    ///
    /// Unlike [`boundaries`](Plugin::boundaries), which the host only warns
    /// about, an unmet requirement is a build error: a stage saying this
    /// cannot do its job at all where it was put.
    ///
    /// [`Upstream`](Needs::Upstream) means every call must carry one whole
    /// message, so boundaries have to arrive from a datagram endpoint or from
    /// an `unframe`. [`Downstream`](Needs::Downstream) means the units this
    /// stage emits have to reach a datagram endpoint or a `frame`, or what it
    /// wrote cannot be read back. The two are separate because the stages
    /// that want them want opposite ones: a stage that seals a message and
    /// appends a tag makes its own boundaries and needs them to survive
    /// downwards, while the stage that verifies and strips that tag needs
    /// whole messages from above and does not care what happens below it.
    ///
    /// Read once, after `build`, alongside `boundaries`. Neither is consulted
    /// on the per-chunk path.
    fn needs(&self) -> Needs {
        Needs::Nothing
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
