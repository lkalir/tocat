//! Composition: chains of [`Plugin`] stages, and the registry that builds them.
//!
//! # Cost model
//!
//! A chunk is threaded through the stages of a segment by reference. A stage
//! that only observes (`ctx.pass_through()`) does not cause a copy: the next
//! stage (and ultimately the socket) is handed the original read buffer. Only
//! a stage that rewrites bytes materialises them, and even then the pipeline
//! ping-pongs between two buffers it owns rather than allocating.
//!
//! So a pipeline of N observers costs N virtual calls per chunk and zero
//! copies, which is why running one is not much worse than not running one.
//!
//! # Framing
//!
//! A chunk off the wire is one unit: one call to the stage below, one write at
//! the far end. A stage that calls [`Ctx::boundary`] says it wants what it
//! emitted delivered as several, which is how a stage like `block` cuts a
//! stream into fixed-size records rather than merely accumulating them.
//!
//! Units cost something, so nothing pays for them unless it asked. A stage
//! that never calls `boundary` leaves the boundary list empty, which reads as
//! "one unit" everywhere and allocates nothing. Below a stage that did, every
//! stage is called once per unit, so unit counts multiply down a chain: this
//! is the reason `boundary` exists as an explicit request rather than
//! something inferred from a stage emitting more than once.
//!
//! Passing through is still free under a framing stage. A stage that hands
//! every unit back untouched copies nothing and answers [`Emit::Passthrough`],
//! exactly as it would on an unframed chunk; the copy starts at the first unit
//! it rewrites, drops or reframes.
//!
//! # Ticks
//!
//! A stage may also ask to be called on a schedule. The pipeline owns the
//! schedules (one deadline per ticking stage) and the host owns the timer
//! that asks whether any of them have come due. Splitting it that way means
//! the host wakes at one period (the shortest any stage asked for) rather than
//! holding a timer per stage, and a pipeline with nothing ticking costs
//! nothing at all: [`Pipeline::tick_interval`] returns `None` and no timer is
//! created.

use std::{
    collections::BTreeMap,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    Direction, PluginSpec,
    channel::HostBuilder,
    error::{PluginError, Result},
    normalize,
    plugin::{
        BuildCtx, Ctx, EffectSink, Emission, Emit, Execution, ExternalStage, PipelineMeta, Plugin,
        PluginFactory, Stage, StageInfo,
    },
};

const EMPTY: &[u8] = &[];

/// The framing of an unframed emission: one unit, covering everything.
const NO_BOUNDS: &[usize] = &[];

/// Which buffer the live bytes are sitting in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The caller's read buffer: nothing has been rewritten yet.
    Input,
    A,
    B,
}

/// What came out of a pipeline, and how it is framed.
///
/// [`bytes`](Self::bytes) is the whole emission and [`units`](Self::units) is
/// the pieces the segment asked for it to be delivered in. Unless a stage
/// called [`Ctx::boundary`] there is exactly one unit and the two say the same
/// thing, so a sink with no framing of its own (a byte stream, a file) can
/// write `bytes` in one call and ignore units entirely.
///
/// Where units are observable they matter: on a datagram sink one unit is one
/// message, and across a detached boundary one unit is one parcel and so one
/// call to the segment below.
#[derive(Debug, Clone, Copy)]
pub struct Emitted<'p> {
    bytes: &'p [u8],
    /// One offset per unit, each the end of its unit.
    ///
    /// Empty means unframed. Otherwise the last offset is always `bytes.len()`
    /// and the offsets ascend, so every byte belongs to exactly one unit and
    /// none can be dropped by a sink that iterates units.
    bounds: &'p [usize],
}

impl<'p> Emitted<'p> {
    /// One unit covering all of `bytes`, which is what an unframed chunk is.
    #[must_use]
    pub const fn whole(bytes: &'p [u8]) -> Self {
        Self {
            bytes,
            bounds: NO_BOUNDS,
        }
    }

    /// Nothing reached the end of the pipeline.
    #[must_use]
    pub const fn empty() -> Self {
        Self::whole(EMPTY)
    }

    /// Everything emitted, concatenated.
    #[must_use]
    pub const fn bytes(&self) -> &'p [u8] {
        self.bytes
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Each unit, in order. Exactly one unit when nothing declared any.
    ///
    /// Takes `self` by value so the iterator borrows the pipeline rather than
    /// this handle, which is what lets a caller iterate units while the
    /// emission is in flight.
    pub fn units(self) -> impl Iterator<Item = &'p [u8]> {
        let Self { bytes, bounds } = self;

        // Nothing declared any framing, so the whole emission is the one unit.
        // An empty emission has no units at all rather than one empty one.
        let unframed = bounds.is_empty() && !bytes.is_empty();

        units(bytes, bounds).chain(unframed.then_some(bytes))
    }
}

/// What one stage reads and what it writes into, borrowed out of a
/// [`Buffers`], along with the slot the writes land in.
type Halves<'b> = (Slot, &'b [u8], &'b [usize], &'b mut Emission);

/// The two halves a segment ping-pongs between.
///
/// Bytes and boundaries travel together, which is why each half is a whole
/// [`Emission`] rather than a buffer: a stage that rewrites its input writes
/// into the far half and declares the framing of what it wrote there, and a
/// stage that passes through leaves both where they are.
#[derive(Default)]
struct Buffers {
    a: Emission,
    b: Emission,
}

impl Buffers {
    /// What a stage reads and what it writes into, as disjoint borrows, plus
    /// the slot it wrote into so the caller cannot pick a different one.
    ///
    /// The destination is always the half the live bytes are not in, so a
    /// stage never reads and writes the same buffer. The first stage of a
    /// chunk reads the caller's buffer, which leaves both halves free.
    fn borrow<'b>(&'b mut self, live: Slot, input: &'b [u8]) -> Halves<'b> {
        match live {
            Slot::Input => (Slot::A, input, NO_BOUNDS, &mut self.a),
            Slot::A => (Slot::B, self.a.bytes(), self.a.bounds(), &mut self.b),
            Slot::B => (Slot::A, self.b.bytes(), self.b.bounds(), &mut self.a),
        }
    }

    /// The bytes currently live, and their framing.
    fn live<'b>(&'b self, live: Slot, input: &'b [u8]) -> (&'b [u8], &'b [usize]) {
        match live {
            Slot::Input => (input, NO_BOUNDS),
            Slot::A => (self.a.bytes(), self.a.bounds()),
            Slot::B => (self.b.bytes(), self.b.bounds()),
        }
    }
}

/// An ordered chain of stages that run inline on one task.
pub struct Pipeline {
    /// Pipeline metadata
    meta: PipelineMeta,
    /// Pipeline stages
    stages: Vec<Box<dyn Plugin>>,
    /// Display names, parallel to `stages`.
    ///
    /// Kept in their own vector rather than beside each plugin because
    /// `self.stages[i]` and `self.names[i]` are then disjoint fields, which
    /// the borrow checker will let the hot loop touch at the same time.
    names: Vec<String>,
    /// The ping-pong halves the stages rewrite into.
    bufs: Buffers,
    /// One entry per stage that asked to be ticked, in stage order.
    ticks: Vec<Schedule>,
}

/// When a ticking stage is next owed a call.
struct Schedule {
    stage: usize,
    period: Duration,
    next: Instant,
}

impl Pipeline {
    #[must_use]
    pub fn new(meta: PipelineMeta, stages: Vec<Box<dyn Plugin>>) -> Self {
        let names = stages.iter().map(|s| s.name().to_string()).collect();
        Self::with_names(meta, stages, names)
    }

    /// As [`Pipeline::new`], but with display names that may differ from the
    /// plugin names: aliases, or `#n` suffixes for repeated plugins.
    #[must_use]
    pub fn with_names(
        meta: PipelineMeta,
        stages: Vec<Box<dyn Plugin>>,
        names: Vec<String>,
    ) -> Self {
        debug_assert_eq!(stages.len(), names.len());

        // Asked once, here, so the per-chunk path never touches it. A zero
        // period would spin the host's timer, so it reads as "no ticks" (the
        // same answer as `None`), which is what a stage configured with an
        // interval of zero means by it.
        let start = Instant::now();
        let ticks = stages
            .iter()
            .enumerate()
            .filter_map(|(stage, plugin)| {
                let period = plugin.tick_interval().filter(|p| !p.is_zero())?;

                Some(Schedule {
                    stage,
                    period,
                    next: start + period,
                })
            })
            .collect();

        Self {
            meta,
            stages,
            names,
            bufs: Buffers::default(),
            ticks,
        }
    }

    #[must_use]
    pub fn meta(&self) -> &PipelineMeta {
        &self.meta
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    pub fn stage_names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    /// How often the host should ask this pipeline for ticks, or `None` when
    /// no stage wants any.
    ///
    /// The shortest period any stage asked for. A stage that wanted a longer
    /// one is simply not due on most of those wakeups, which is cheaper than a
    /// timer each.
    #[must_use]
    pub fn tick_interval(&self) -> Option<Duration> {
        self.ticks.iter().map(|schedule| schedule.period).min()
    }

    /// The next stage owed a tick at `now`, with its schedule advanced.
    fn due(&mut self, now: Instant) -> Option<usize> {
        let schedule = self
            .ticks
            .iter_mut()
            .find(|schedule| schedule.next <= now)?;

        schedule.next += schedule.period;

        // Slept through several periods: resume from now rather than firing
        // once for each one we missed. A stage that wants to know how long it
        // was actually away measures it itself.
        if schedule.next <= now {
            schedule.next = now + schedule.period;
        }

        Some(schedule.stage)
    }

    /// Restart one stage's schedule, as [`Ctx::rearm`] asked.
    ///
    /// The next tick falls a full period from now rather than from wherever
    /// the cadence had reached, which turns the interval a stage asked for
    /// into a delay it can rely on: a stage that starts holding bytes says so,
    /// and hears back an interval later rather than at the next wakeup that
    /// happens to be due.
    ///
    /// The clock is read here and only here, so a pipeline in which nothing
    /// ever asks never touches it on the per-chunk path. A stage that asked
    /// for no ticks has no schedule and is silently ignored.
    fn rearm(&mut self, stage: usize) {
        if let Some(schedule) = self.ticks.iter_mut().find(|s| s.stage == stage) {
            schedule.next = Instant::now() + schedule.period;
        }
    }

    /// Give one due stage its tick, and return what reached the end of the
    /// pipeline.
    ///
    /// `None` means nothing was due. Call it again until it says so: two
    /// stages can come due on the same wakeup, and each one's output has to be
    /// written before the next runs.
    ///
    /// Unlike [`process`](Pipeline::process) and [`finish`](Pipeline::finish)
    /// this does not run every stage. A tick belongs to one of them, and what
    /// it emits cascades through the stages *below* it only, the ones above
    /// are upstream of a chunk that did not come from them.
    pub fn tick<'p>(
        &'p mut self,
        now: Instant,
        sink: &mut dyn EffectSink,
    ) -> Result<Option<Emitted<'p>>> {
        let Some(index) = self.due(now) else {
            return Ok(None);
        };

        run_tick(
            &mut self.stages[index],
            &self.meta,
            &self.names[index],
            &mut self.bufs.a,
            sink,
        )?;

        // A stage may restart its own schedule from a tick as readily as from
        // a chunk, which is how one that has just given up waiting says so.
        if self.bufs.a.rearm_requested() {
            self.rearm(index);
        }

        // The overwhelming case: an observer that reports and forwards
        // nothing. No stage below it needs to hear about that.
        if self.bufs.a.bytes().is_empty() {
            return Ok(Some(Emitted::empty()));
        }

        self.drive(EMPTY, index + 1, Slot::A, false, sink).map(Some)
    }

    /// The first stage that must not carry datagrams, if any.
    #[must_use]
    pub fn datagram_hazard(&self) -> Option<&str> {
        self.stages
            .iter()
            .zip(&self.names)
            .find(|(stage, _)| !stage.datagram_safe())
            .map(|(_, name)| name.as_str())
    }

    /// Push one chunk through every stage.
    ///
    /// The result borrows `input` directly when every stage passed it through,
    /// and carries the framing of whatever reframed it otherwise.
    pub fn process<'p>(
        &'p mut self,
        input: &'p [u8],
        sink: &mut dyn EffectSink,
    ) -> Result<Emitted<'p>> {
        self.drive(input, 0, Slot::Input, false, sink)
    }

    /// Signal EOF, cascading each stage's final bytes through the ones below.
    pub fn finish<'p>(&'p mut self, sink: &mut dyn EffectSink) -> Result<Emitted<'p>> {
        self.drive(EMPTY, 0, Slot::Input, true, sink)
    }

    /// Thread bytes through `self.stages[from..]`.
    ///
    /// `live` says where they start out: [`Slot::Input`] for the caller's
    /// buffer, or a half of the segment's own, which is how a tick cascades
    /// into the stages below the one that fired.
    fn drive<'p>(
        &'p mut self,
        input: &'p [u8],
        from: usize,
        live: Slot,
        eof: bool,
        sink: &mut dyn EffectSink,
    ) -> Result<Emitted<'p>> {
        let mut live = live;

        for index in from..self.stages.len() {
            let (slot, src, src_bounds, dst) = self.bufs.borrow(live, input);

            run(
                &mut self.stages[index],
                &self.meta,
                &self.names[index],
                src,
                src_bounds,
                dst,
                sink,
                eof,
            )?;

            let emitted = dst.emit();
            let rearm = dst.rearm_requested();

            if rearm {
                self.rearm(index);
            }

            if emitted != Emit::Passthrough {
                live = slot;
            }

            // A swallowed chunk cannot become bytes again further down. At end
            // of stream there is no early exit, because every stage still has
            // to hear about it.
            if !eof && self.bufs.live(live, input).0.is_empty() {
                return Ok(Emitted::empty());
            }
        }

        let (bytes, bounds) = self.bufs.live(live, input);

        Ok(Emitted { bytes, bounds })
    }
}

/// Give one stage the bytes above it, and collect what it emitted into `dst`.
///
/// `in_bounds` frames `input` the way [`Emitted`] describes: empty is one
/// unit, which is the shape of every chunk off the wire and of everything a
/// stage that never calls [`Ctx::boundary`] produces. When it is not empty the
/// stage is called once per unit, because a stage handed framed bytes must not
/// see two units fused into one call.
///
/// Leaves [`Emit::Passthrough`] on `dst` only when nothing was copied, so the
/// caller can go on handing the source buffer down the chain.
fn run(
    plugin: &mut Box<dyn Plugin>,
    meta: &PipelineMeta,
    stage: &str,
    input: &[u8],
    in_bounds: &[usize],
    dst: &mut Emission,
    sink: &mut dyn EffectSink,
    eof: bool,
) -> Result<()> {
    dst.reset();

    if in_bounds.is_empty() {
        {
            let mut ctx = Ctx::new(meta, stage, input, dst, sink);

            if eof {
                // Every stage but the first is handed whatever the one above
                // it produced on its way out.
                if !input.is_empty() {
                    plugin.on_bytes(&mut ctx, input)?;
                }

                plugin.on_eof(&mut ctx)?;
            } else {
                plugin.on_bytes(&mut ctx, input)?;
            }
        }

        // Only when the stage declared framing of its own. Closing an unframed
        // emission would allocate on the hot path to say what empty says.
        if !dst.bounds().is_empty() {
            dst.close();
        }

        return Ok(());
    }

    let mut copied = false;

    for (index, unit) in units(input, in_bounds).enumerate() {
        dst.next_unit();

        {
            let mut ctx = Ctx::new(meta, stage, unit, dst, sink);
            plugin.on_bytes(&mut ctx, unit)?;
        }

        if !copied {
            if dst.emit() == Emit::Passthrough {
                // Nothing but the source buffer's own bytes so far, so there
                // is still nothing to copy.
                continue;
            }

            // The first unit this stage did not hand back verbatim, so the
            // ones before it have to become real bytes now.
            materialise(input, in_bounds, index, dst);
            copied = true;
        }

        if dst.emit() == Emit::Passthrough {
            dst.out.extend_from_slice(unit);
        }

        dst.close();
    }

    if eof {
        dst.next_unit();

        {
            let mut ctx = Ctx::new(meta, stage, EMPTY, dst, sink);
            plugin.on_eof(&mut ctx)?;
        }

        // An epilogue is bytes of its own, so it forces the copy that the
        // units before it avoided.
        if !copied && !dst.bytes().is_empty() {
            materialise(input, in_bounds, in_bounds.len(), dst);
            copied = true;
        }

        if copied {
            dst.close();
        }
    }

    dst.emit = if copied {
        Emit::Buffered
    } else {
        Emit::Passthrough
    };

    Ok(())
}

/// Execute a stage's tick. There is no input, so anything it wants downstream
/// it has to write.
fn run_tick(
    plugin: &mut Box<dyn Plugin>,
    meta: &PipelineMeta,
    stage: &str,
    dst: &mut Emission,
    sink: &mut dyn EffectSink,
) -> Result<()> {
    dst.reset();

    {
        let mut ctx = Ctx::new(meta, stage, EMPTY, dst, sink);
        plugin.on_tick(&mut ctx)?;
    }

    if !dst.bounds().is_empty() {
        dst.close();
    }

    Ok(())
}

/// Walk a buffer's units, given the offsets that frame it.
fn units<'a>(bytes: &'a [u8], bounds: &'a [usize]) -> impl Iterator<Item = &'a [u8]> {
    let mut start = 0;

    bounds.iter().map(move |&end| {
        let unit = &bytes[start..end];
        start = end;
        unit
    })
}

/// Copy the units a stage handed back verbatim in front of whatever it has
/// already written for the unit that broke the run.
///
/// At most once per stage per call, and only for a stage that was both handed
/// framed bytes and did something other than pass them through. A pipeline
/// with no framing in it never reaches this, and neither does an observer
/// sitting under one.
fn materialise(input: &[u8], in_bounds: &[usize], done: usize, dst: &mut Emission) {
    let prefix = if done == 0 { 0 } else { in_bounds[done - 1] };

    if prefix == 0 {
        return;
    }

    dst.out.splice(0..0, input[..prefix].iter().copied());

    // What the stage wrote is now further along by the length of the prefix,
    // and the prefix's own units come first.
    for bound in dst.bounds.iter_mut() {
        *bound += prefix;
    }

    dst.bounds.splice(0..0, in_bounds[..done].iter().copied());
}

impl fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipeline")
            .field("direction", &self.meta.direction)
            .field("stages", &self.stage_names().collect::<Vec<_>>())
            .finish()
    }
}

/// Everything that runs on one direction, split into segments.
///
/// One segment is the common case and runs entirely on the reading task. A
/// stage declared `Detached` starts a new segment, which the host runs on its
/// own task behind a bounded channel; subsequent inline stages join that
/// segment rather than spawning more.
/// One link in a chain: either stages the host calls inline, or a subprocess
/// it feeds and drains.
#[derive(Debug)]
pub enum Segment {
    Inline(Pipeline),
    Process(ExternalStage),
}

/// Data processing chain.
#[derive(Debug)]
pub struct Chain {
    meta: PipelineMeta,
    segments: Vec<Segment>,
}

impl Chain {
    #[must_use]
    pub fn new(meta: PipelineMeta, segments: Vec<Segment>) -> Self {
        Self { meta, segments }
    }

    #[must_use]
    pub fn meta(&self) -> &PipelineMeta {
        &self.meta
    }

    /// No stages at all: the host should use its plain copy path.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    #[must_use]
    pub fn into_segments(self) -> Vec<Segment> {
        self.segments
    }

    /// The first stage on this chain that must not carry datagrams, if any.
    ///
    /// A subprocess never can: its stdin and stdout are byte streams, so
    /// message boundaries are gone the moment bytes cross the pipe.
    #[must_use]
    pub fn datagram_hazard(&self) -> Option<&str> {
        self.segments().iter().find_map(|segment| match segment {
            Segment::Inline(pipeline) => pipeline.datagram_hazard(),
            Segment::Process(external) => Some(external.name.as_str()),
        })
    }

    #[must_use]
    pub fn stage_names(&self) -> Vec<&str> {
        self.segments
            .iter()
            .flat_map(|segment| match segment {
                Segment::Inline(pipeline) => pipeline.stage_names().collect::<Vec<_>>(),
                Segment::Process(external) => vec![external.name.as_str()],
            })
            .collect()
    }
}

/// Every plugin this binary knows how to build.
#[derive(Default)]
pub struct Registry {
    factories: BTreeMap<String, Arc<dyn PluginFactory>>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, factory: impl PluginFactory) -> &mut Self {
        self.register_arc(Arc::new(factory))
    }

    pub fn register_arc(&mut self, factory: Arc<dyn PluginFactory>) -> &mut Self {
        self.factories.insert(normalize(factory.name()), factory);
        self
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn PluginFactory>> {
        self.factories.get(&normalize(name))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn PluginFactory>> {
        self.factories.values()
    }

    /// The names plugins call themselves, not the normalized keys they are
    /// stored under: these are shown to people, in `--list-plugins` and in the
    /// suggestions on an unknown plugin.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        // Keys are normalized, but name() preserves the original
        self.factories.values().map(|f| f.name())
    }

    /// Build the chain for one direction.
    ///
    /// Entries that do not apply to `meta.direction` are skipped. On the
    /// sink-to-source path the survivors are mirrored, so a declaration
    /// `[a, b]` nests as `a(b(payload))` in both directions.
    pub fn build(
        &self,
        specs: &[PluginSpec],
        meta: &PipelineMeta,
        host: &mut dyn HostBuilder,
    ) -> Result<Chain> {
        let mut selected: Vec<&PluginSpec> = specs
            .iter()
            .filter(|spec| spec.direction.contains(meta.direction))
            .collect();

        if meta.direction == Direction::SinkToSource {
            selected.reverse();
        }

        let display = display_names(&selected);

        // Neighbours, as seen on this path: the upstream endpoint, every
        // stage, then the downstream endpoint. Stage `i` sits at `i + 1`.
        let mut labels = Vec::with_capacity(display.len() + 2);
        labels.push(meta.upstream().to_string());
        labels.extend(display.iter().cloned());
        labels.push(meta.downstream().to_string());

        let total = selected.len();
        let mut segments: Vec<Segment> = Vec::new();
        let mut draft: Option<SegmentDraft> = None;

        for (index, spec) in selected.iter().enumerate() {
            let factory = self
                .get(&spec.name)
                .ok_or_else(|| PluginError::unknown(&spec.name, self.names()))?
                .clone();

            let execution = match spec.detach {
                Some(true) => Execution::Detached,
                Some(false) => Execution::Inline,
                None => factory.execution(),
            };

            let stage_info = StageInfo {
                index,
                total,
                name: &display[index],
                upstream: &labels[index],
                downstream: &labels[index + 2],
            };

            let mut ctx = BuildCtx::new(&spec.name, &spec.config, meta, stage_info, host);

            match factory.build(&mut ctx)? {
                Stage::Filter(plugin) => {
                    // A detached stage starts a new segment; so does the first
                    // stage after a subprocess, since it cannot run inside one.
                    if draft.is_none() || execution == Execution::Detached {
                        if let Some(ready) = draft.take() {
                            segments.push(Segment::Inline(ready.into_pipeline(meta.clone())));
                        }
                        draft = Some(SegmentDraft::default());
                    }

                    draft
                        .as_mut()
                        .expect("a draft was just ensured")
                        .push(plugin, display[index].clone());
                }
                Stage::External(external) => {
                    if spec.detach == Some(false) {
                        return Err(PluginError::config(
                            &spec.name,
                            "runs as a subprocess and always has its own task; `detach = false` \
                             cannot be honoured",
                        ));
                    }

                    if let Some(ready) = draft.take() {
                        segments.push(Segment::Inline(ready.into_pipeline(meta.clone())));
                    }

                    segments.push(Segment::Process(external));
                }
            }
        }

        if let Some(ready) = draft.take() {
            segments.push(Segment::Inline(ready.into_pipeline(meta.clone())));
        }

        Ok(Chain::new(meta.clone(), segments))
    }

    /// Build both directions at once. Returns `(source_to_sink,
    /// sink_to_source)`.
    ///
    /// Both share one `host`, which is how identical side-channel targets end
    /// up sharing a single writer.
    pub fn build_pair(
        &self,
        specs: &[PluginSpec],
        source: &str,
        sink: &str,
        peer: Option<&str>,
        host: &mut dyn HostBuilder,
    ) -> Result<(Chain, Chain)> {
        let forward = PipelineMeta::new(Direction::SourceToSink, source, sink).with_peer(peer);
        let reverse = PipelineMeta {
            direction: Direction::SinkToSource,
            ..forward.clone()
        };

        Ok((
            self.build(specs, &forward, host)?,
            self.build(specs, &reverse, host)?,
        ))
    }
}

/// A segment being assembled: stages and their display names, kept together so
/// the two vectors cannot drift out of step.
#[derive(Default)]
struct SegmentDraft {
    stages: Vec<Box<dyn Plugin>>,
    names: Vec<String>,
}

impl SegmentDraft {
    fn push(&mut self, plugin: Box<dyn Plugin>, name: String) {
        self.stages.push(plugin);
        self.names.push(name);
    }

    fn into_pipeline(self, meta: PipelineMeta) -> Pipeline {
        Pipeline::with_names(meta, self.stages, self.names)
    }
}

/// Display name per stage: the `as` alias when given, else the plugin name,
/// with `#n` appended when a name would otherwise appear twice on one path.
fn display_names(specs: &[&PluginSpec]) -> Vec<String> {
    let base: Vec<&str> = specs
        .iter()
        .map(|spec| spec.alias.as_deref().unwrap_or(spec.name.as_str()))
        .collect();

    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for name in &base {
        *seen.entry(name).or_insert(0) += 1;
    }

    let mut used: BTreeMap<&str, usize> = BTreeMap::new();
    base.iter()
        .map(|name| {
            if seen.get(name).copied().unwrap_or(0) > 1 {
                let n = used.entry(name).or_insert(0);
                *n += 1;
                format!("{name}#{n}")
            } else {
                (*name).to_string()
            }
        })
        .collect()
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("plugins", &self.names().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelId, DirectionSpec, plugin::LogLevel};

    #[derive(Default)]
    struct Recorder {
        writes: Vec<(ChannelId, Vec<u8>)>,
        logs: Vec<String>,
    }

    impl EffectSink for Recorder {
        fn write(&mut self, channel: ChannelId, bytes: &[u8]) {
            self.writes.push((channel, bytes.to_vec()));
        }

        fn log(&mut self, _level: LogLevel, stage: &str, message: &str) {
            self.logs.push(format!("{stage}: {message}"));
        }
    }

    /// Observes without touching the payload.
    struct Observer;

    impl Plugin for Observer {
        fn name(&self) -> &str {
            "observer"
        }

        fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
            ctx.side_write(ChannelId(0), input);
            ctx.pass_through();
            Ok(())
        }
    }

    struct Upper;

    impl Plugin for Upper {
        fn name(&self) -> &str {
            "upper"
        }

        fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
            let upper: Vec<u8> = input.iter().map(u8::to_ascii_uppercase).collect();
            ctx.forward(&upper);
            Ok(())
        }
    }

    /// Buffers everything, emits it reversed at EOF.
    #[derive(Default)]
    struct Reverse(Vec<u8>);

    impl Plugin for Reverse {
        fn name(&self) -> &str {
            "reverse"
        }

        fn on_bytes(&mut self, _ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
            self.0.extend_from_slice(input);
            Ok(())
        }

        fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
            let mut buf = std::mem::take(&mut self.0);
            buf.reverse();
            ctx.forward(&buf);
            Ok(())
        }
    }

    /// Emits on every tick, the way a keepalive would.
    struct Beacon(Duration);

    impl Plugin for Beacon {
        fn name(&self) -> &str {
            "beacon"
        }

        fn on_bytes(&mut self, ctx: &mut Ctx<'_>, _input: &[u8]) -> Result<()> {
            ctx.pass_through();
            Ok(())
        }

        fn tick_interval(&self) -> Option<Duration> {
            Some(self.0)
        }

        fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
            ctx.forward(b"ping");
            Ok(())
        }
    }

    /// Wants ticks but never emits on one. The shape almost every ticking
    /// stage actually has.
    struct Quiet(Duration);

    impl Plugin for Quiet {
        fn name(&self) -> &str {
            "quiet"
        }

        fn on_bytes(&mut self, ctx: &mut Ctx<'_>, _input: &[u8]) -> Result<()> {
            ctx.pass_through();
            Ok(())
        }

        fn tick_interval(&self) -> Option<Duration> {
            Some(self.0)
        }

        fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
            ctx.log(LogLevel::Info, "still here");
            Ok(())
        }
    }

    /// Cuts what it is given into fixed-size units, the way `block` does.
    /// Anything left over is emitted at EOF.
    struct Chop {
        size: usize,
        held: Vec<u8>,
    }

    impl Chop {
        fn new(size: usize) -> Self {
            Self {
                size,
                held: Vec::new(),
            }
        }
    }

    impl Plugin for Chop {
        fn name(&self) -> &str {
            "chop"
        }

        fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
            self.held.extend_from_slice(input);

            while self.held.len() >= self.size {
                let rest = self.held.split_off(self.size);
                ctx.forward(&self.held);
                ctx.boundary();
                self.held = rest;
            }

            Ok(())
        }

        fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
            if !self.held.is_empty() {
                let held = std::mem::take(&mut self.held);
                ctx.forward(&held);
                ctx.boundary();
            }

            Ok(())
        }
    }

    /// Drops every unit whose first byte is `skip`, and passes the rest
    /// through. Enough to break the all-passthrough run part way along.
    struct Sieve(u8);

    impl Plugin for Sieve {
        fn name(&self) -> &str {
            "sieve"
        }

        fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
            if input.first() == Some(&self.0) {
                ctx.drop_chunk();
            } else {
                ctx.pass_through();
            }

            Ok(())
        }
    }

    /// Passes everything through and writes an epilogue at end of stream, the
    /// way a codec closing a frame does.
    struct Trailer(&'static [u8]);

    impl Plugin for Trailer {
        fn name(&self) -> &str {
            "trailer"
        }

        fn on_bytes(&mut self, ctx: &mut Ctx<'_>, _input: &[u8]) -> Result<()> {
            ctx.pass_through();
            Ok(())
        }

        fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
            ctx.forward(self.0);
            Ok(())
        }
    }

    /// Passes everything through and restarts its own schedule on the way,
    /// the way a stage that has just started holding bytes does.
    struct Restart(Option<Duration>);

    impl Plugin for Restart {
        fn name(&self) -> &str {
            "restart"
        }

        fn on_bytes(&mut self, ctx: &mut Ctx<'_>, _input: &[u8]) -> Result<()> {
            ctx.rearm();
            ctx.pass_through();
            Ok(())
        }

        fn tick_interval(&self) -> Option<Duration> {
            self.0
        }
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
    }

    /// Comfortably past any schedule set at construction.
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    /// Every unit of an emission, for comparing against a literal.
    fn parts<'a>(emitted: &Emitted<'a>) -> Vec<&'a [u8]> {
        emitted.units().collect()
    }

    #[test]
    fn empty_pipeline_returns_the_input_slice() {
        let mut p = Pipeline::new(meta(), Vec::new());
        let mut sink = Recorder::default();
        let input = b"hello";

        let out = p.process(input, &mut sink).unwrap();
        assert!(std::ptr::eq(out.bytes().as_ptr(), input.as_ptr()));
    }

    #[test]
    fn observers_never_copy_the_payload() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Observer), Box::new(Observer)]);
        let mut sink = Recorder::default();
        let input = b"payload";

        let out = p.process(input, &mut sink).unwrap();

        assert!(
            std::ptr::eq(out.bytes().as_ptr(), input.as_ptr()),
            "a chain of observers must hand the original buffer downstream",
        );
        assert_eq!(sink.writes.len(), 2);
    }

    #[test]
    fn stages_chain_in_order() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Upper), Box::new(Reverse::default())]);
        let mut sink = Recorder::default();

        assert!(p.process(b"ab", &mut sink).unwrap().is_empty());
        assert!(p.process(b"cd", &mut sink).unwrap().is_empty());
        assert_eq!(p.finish(&mut sink).unwrap().bytes(), b"DCBA");
    }

    #[test]
    fn repeated_plugins_get_distinct_display_names() {
        let specs = [
            PluginSpec::new("tee", DirectionSpec::Both),
            PluginSpec::new("tee", DirectionSpec::Both).named("audit"),
            PluginSpec::new("tee", DirectionSpec::Both),
        ];
        let refs: Vec<&PluginSpec> = specs.iter().collect();

        assert_eq!(display_names(&refs), ["tee#1", "audit", "tee#2"]);
    }

    #[test]
    fn a_pipeline_with_nothing_ticking_has_no_schedule() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Observer)]);
        let mut sink = Recorder::default();

        assert_eq!(p.tick_interval(), None, "so the host builds no timer");
        assert!(p.tick(later(), &mut sink).unwrap().is_none());
    }

    /// One timer for the segment, at the shortest period asked for; the stage
    /// that wanted the longer one is simply not due on most wakeups.
    #[test]
    fn the_schedule_is_the_shortest_period_asked_for() {
        let p = Pipeline::new(
            meta(),
            vec![
                Box::new(Quiet(Duration::from_secs(30))),
                Box::new(Beacon(Duration::from_secs(5))),
            ],
        );

        assert_eq!(p.tick_interval(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn a_tick_cascades_through_the_stages_below_it() {
        let mut p = Pipeline::new(
            meta(),
            vec![Box::new(Beacon(Duration::from_secs(60))), Box::new(Upper)],
        );
        let mut sink = Recorder::default();

        assert!(
            p.tick(Instant::now(), &mut sink).unwrap().is_none(),
            "not due yet",
        );

        let now = later();
        assert_eq!(p.tick(now, &mut sink).unwrap().unwrap().bytes(), b"PING");
        assert!(
            p.tick(now, &mut sink).unwrap().is_none(),
            "one turn per stage per wakeup, however far behind the schedule is",
        );
    }

    /// The common case has to stay free: a stage that only reports must not
    /// push an empty chunk at everything below it.
    #[test]
    fn a_silent_tick_does_not_disturb_the_stages_below() {
        let mut p = Pipeline::new(
            meta(),
            vec![Box::new(Quiet(Duration::from_secs(60))), Box::new(Observer)],
        );
        let mut sink = Recorder::default();

        assert!(p.tick(later(), &mut sink).unwrap().unwrap().is_empty());
        assert!(sink.writes.is_empty(), "the observer below never ran");
        assert_eq!(sink.logs, ["quiet: still here"]);
    }

    /// Ticking is orthogonal to the data path: a stage above the beacon must
    /// not see its output, and the payload must be unaffected.
    #[test]
    fn ticks_and_chunks_do_not_interfere() {
        let mut p = Pipeline::new(
            meta(),
            vec![
                Box::new(Observer),
                Box::new(Beacon(Duration::from_secs(60))),
            ],
        );
        let mut sink = Recorder::default();

        assert_eq!(
            p.tick(later(), &mut sink).unwrap().unwrap().bytes(),
            b"ping"
        );
        assert!(
            sink.writes.is_empty(),
            "the observer sits above the beacon and saw nothing",
        );

        assert_eq!(
            p.process(b"payload", &mut sink).unwrap().bytes(),
            b"payload"
        );
        assert_eq!(sink.writes, [(ChannelId(0), b"payload".to_vec())]);
    }

    #[test]
    fn two_stages_due_at_once_each_get_a_turn() {
        let mut p = Pipeline::new(
            meta(),
            vec![
                Box::new(Beacon(Duration::from_secs(60))),
                Box::new(Quiet(Duration::from_secs(60))),
            ],
        );
        let mut sink = Recorder::default();
        let now = later();

        assert_eq!(p.tick(now, &mut sink).unwrap().unwrap().bytes(), b"ping");
        assert!(p.tick(now, &mut sink).unwrap().unwrap().is_empty());
        assert!(p.tick(now, &mut sink).unwrap().is_none());
    }

    #[test]
    fn transform_then_observe_keeps_the_transformed_bytes() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Upper), Box::new(Observer)]);
        let mut sink = Recorder::default();

        assert_eq!(p.process(b"hi", &mut sink).unwrap().bytes(), b"HI");
        assert_eq!(sink.writes[0].1, b"HI".to_vec());
    }

    /// Nothing asked for framing, so there is one unit and it is everything.
    #[test]
    fn an_unframed_emission_is_one_unit() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Upper)]);
        let mut sink = Recorder::default();

        let out = p.process(b"hi", &mut sink).unwrap();
        assert_eq!(parts(&out), [b"HI".as_slice()]);
    }

    #[test]
    fn an_empty_emission_has_no_units() {
        assert!(Emitted::empty().units().next().is_none());
    }

    /// The whole point of `boundary`: several units out of one call, and they
    /// stay separate rather than fusing into the concatenation.
    #[test]
    fn a_stage_can_emit_several_units_from_one_chunk() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Chop::new(2))]);
        let mut sink = Recorder::default();

        let out = p.process(b"abcdef", &mut sink).unwrap();

        assert_eq!(out.bytes(), b"abcdef", "the bytes are still the bytes");
        assert_eq!(parts(&out), [b"ab".as_slice(), b"cd", b"ef"]);
    }

    /// A stage under a framing stage is called once per unit, not once per
    /// chunk, and its output stays framed the same way.
    #[test]
    fn framing_survives_a_stage_that_rewrites_it() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Chop::new(2)), Box::new(Upper)]);
        let mut sink = Recorder::default();

        let out = p.process(b"abcdef", &mut sink).unwrap();
        assert_eq!(parts(&out), [b"AB".as_slice(), b"CD", b"EF"]);
    }

    /// The cost model has to hold under framing too: an observer below a
    /// framing stage sees each unit and still copies nothing.
    #[test]
    fn an_observer_under_a_framing_stage_still_copies_nothing() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Chop::new(2)), Box::new(Observer)]);
        let mut sink = Recorder::default();

        let out = p.process(b"abcdef", &mut sink).unwrap();

        assert_eq!(parts(&out), [b"ab".as_slice(), b"cd", b"ef"]);
        assert_eq!(
            sink.writes.len(),
            3,
            "the observer was called once per unit, not once per chunk",
        );
    }

    /// The copy starts at the first unit that is not handed back verbatim, and
    /// the units before it have to survive that.
    #[test]
    fn units_passed_through_before_a_drop_are_kept() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Chop::new(2)), Box::new(Sieve(b'c'))]);
        let mut sink = Recorder::default();

        let out = p.process(b"abcdef", &mut sink).unwrap();

        assert_eq!(out.bytes(), b"abef");
        assert_eq!(parts(&out), [b"ab".as_slice(), b"ef"]);
    }

    /// Same again with the drop first, which is the case where there is no
    /// prefix to keep.
    #[test]
    fn dropping_the_first_unit_keeps_the_rest() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Chop::new(2)), Box::new(Sieve(b'a'))]);
        let mut sink = Recorder::default();

        let out = p.process(b"abcdef", &mut sink).unwrap();

        assert_eq!(parts(&out), [b"cd".as_slice(), b"ef"]);
    }

    /// A stage that emits at EOF after passing every unit through has to force
    /// the copy it had been avoiding, or its epilogue would arrive alone.
    #[test]
    fn an_epilogue_after_a_run_of_passthroughs_keeps_both() {
        let mut p = Pipeline::new(
            meta(),
            vec![Box::new(Chop::new(4)), Box::new(Trailer(b"!"))],
        );
        let mut sink = Recorder::default();

        let out = p.process(b"abcdef", &mut sink).unwrap();
        assert_eq!(parts(&out), [b"abcd".as_slice()]);

        let out = p.finish(&mut sink).unwrap();
        assert_eq!(out.bytes(), b"ef!");
        assert_eq!(parts(&out), [b"ef".as_slice(), b"!"]);
    }

    /// A short tail is held until EOF, and arrives as a unit of its own.
    #[test]
    fn a_short_final_unit_is_emitted_at_end_of_stream() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Chop::new(4))]);
        let mut sink = Recorder::default();

        let out = p.process(b"abcdef", &mut sink).unwrap();
        assert_eq!(parts(&out), [b"abcd".as_slice()]);

        let out = p.finish(&mut sink).unwrap();
        assert_eq!(parts(&out), [b"ef".as_slice()]);
    }

    /// Two framing stages compose: the second reframes what the first handed
    /// it, one unit at a time.
    #[test]
    fn framing_stages_compose() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Chop::new(4)), Box::new(Chop::new(2))]);
        let mut sink = Recorder::default();

        let out = p.process(b"abcdefgh", &mut sink).unwrap();
        assert_eq!(parts(&out), [b"ab".as_slice(), b"cd", b"ef", b"gh"]);
    }

    /// Without a rearm the schedule is a cadence: it advances from wherever
    /// it had reached, which says nothing about how long the stage has
    /// actually been waiting. A rearm makes the interval mean "from now".
    #[test]
    fn a_stage_can_restart_its_own_schedule() {
        let period = Duration::from_secs(600);
        let mut p = Pipeline::new(meta(), vec![Box::new(Restart(Some(period)))]);
        let mut sink = Recorder::default();
        let start = Instant::now();

        // Fires, which moves the cadence on to two periods from construction.
        assert!(
            p.tick(start + period + Duration::from_secs(100), &mut sink)
                .unwrap()
                .is_some(),
        );
        assert!(
            p.tick(start + period + Duration::from_secs(200), &mut sink)
                .unwrap()
                .is_none(),
            "the cadence has moved past this",
        );

        p.process(b"payload", &mut sink).unwrap();

        assert!(
            p.tick(start + period + Duration::from_secs(300), &mut sink)
                .unwrap()
                .is_some(),
            "the chunk restarted the schedule, so a period from now is due \
             again well before the cadence would have come round",
        );
    }

    #[test]
    fn rearming_a_stage_that_asked_for_no_ticks_does_nothing() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Restart(None))]);
        let mut sink = Recorder::default();

        assert_eq!(
            p.process(b"payload", &mut sink).unwrap().bytes(),
            b"payload"
        );
        assert!(p.tick(later(), &mut sink).unwrap().is_none());
    }

    /// Buffering below a framing stage still works: the stage sees each unit
    /// and answers whenever it has something to say.
    #[test]
    fn a_buffering_stage_under_a_framing_stage_holds_across_units() {
        let mut p = Pipeline::new(
            meta(),
            vec![Box::new(Chop::new(2)), Box::new(Reverse::default())],
        );
        let mut sink = Recorder::default();

        assert!(p.process(b"abcd", &mut sink).unwrap().is_empty());
        assert_eq!(p.finish(&mut sink).unwrap().bytes(), b"dcba");
    }
}
