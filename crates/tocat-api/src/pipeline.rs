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
    plugin::{
        BuildCtx, Ctx, EffectSink, Emit, Execution, ExternalStage, PipelineMeta, Plugin,
        PluginFactory, Stage, StageInfo,
    },
};

const EMPTY: &[u8] = &[];

/// Which buffer the live bytes are sitting in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The caller's read buffer — nothing has been rewritten yet.
    Input,
    A,
    B,
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
    /// Ping-pong buffer A
    a: Vec<u8>,
    /// Ping-pong buffer B
    b: Vec<u8>,
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
    /// plugin names — aliases, or `#n` suffixes for repeated plugins.
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
            a: Vec::new(),
            b: Vec::new(),
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
    ) -> Result<Option<&'p [u8]>> {
        let Some(index) = self.due(now) else {
            return Ok(None);
        };

        run_tick(
            &mut self.stages[index],
            &self.meta,
            &self.names[index],
            &mut self.a,
            sink,
        )?;

        // The overwhelming case: an observer that reports and forwards
        // nothing. No stage below it needs to hear about that.
        if self.a.is_empty() {
            return Ok(Some(EMPTY));
        }

        self.cascade(index + 1, sink).map(Some)
    }

    /// Push the bytes sitting in `a` through the stages from `from` onwards.
    fn cascade<'p>(&'p mut self, from: usize, sink: &mut dyn EffectSink) -> Result<&'p [u8]> {
        let mut in_a = true;

        for index in from..self.stages.len() {
            let emitted = if in_a {
                run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    &self.a,
                    &mut self.b,
                    sink,
                    false,
                )?
            } else {
                run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    &self.b,
                    &mut self.a,
                    sink,
                    false,
                )?
            };

            if emitted != Emit::Passthrough {
                in_a = !in_a;
            }

            let live = if in_a { self.a.len() } else { self.b.len() };

            // Swallowed: it cannot become bytes again further down.
            if live == 0 {
                return Ok(EMPTY);
            }
        }

        Ok(if in_a { &self.a[..] } else { &self.b[..] })
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
    /// Returns the bytes to send downstream, which may borrow `input` directly
    /// when every stage passed it through.
    pub fn process<'p>(
        &'p mut self,
        input: &'p [u8],
        sink: &mut dyn EffectSink,
    ) -> Result<&'p [u8]> {
        if self.stages.is_empty() {
            return Ok(input);
        }

        let mut src = Slot::Input;
        let mut dst_a = true;

        for index in 0..self.stages.len() {
            let emitted = match (src, dst_a) {
                (Slot::Input, true) => run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    input,
                    &mut self.a,
                    sink,
                    false,
                )?,
                (Slot::Input, false) => run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    input,
                    &mut self.b,
                    sink,
                    false,
                )?,
                (Slot::A, false) => run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    &self.a,
                    &mut self.b,
                    sink,
                    false,
                )?,
                (Slot::B, true) => run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    &self.b,
                    &mut self.a,
                    sink,
                    false,
                )?,
                _ => unreachable!("destination slot never aliases the source slot"),
            };

            if emitted != Emit::Passthrough {
                src = if dst_a { Slot::A } else { Slot::B };
                dst_a = !dst_a;
            }

            let live = match src {
                Slot::Input => input.len(),
                Slot::A => self.a.len(),
                Slot::B => self.b.len(),
            };

            // A swallowed chunk cannot become bytes again further down.
            if live == 0 {
                return Ok(EMPTY);
            }
        }

        Ok(match src {
            Slot::Input => input,
            Slot::A => &self.a[..],
            Slot::B => &self.b[..],
        })
    }

    /// Signal EOF, cascading each stage's final bytes through the ones below.
    pub fn finish<'p>(&'p mut self, sink: &mut dyn EffectSink) -> Result<&'p [u8]> {
        if self.stages.is_empty() {
            return Ok(EMPTY);
        }

        let mut src = Slot::Input;
        let mut dst_a = true;

        for index in 0..self.stages.len() {
            let emitted = match (src, dst_a) {
                (Slot::Input, true) => run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    EMPTY,
                    &mut self.a,
                    sink,
                    true,
                )?,
                (Slot::Input, false) => run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    EMPTY,
                    &mut self.b,
                    sink,
                    true,
                )?,
                (Slot::A, false) => run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    &self.a,
                    &mut self.b,
                    sink,
                    true,
                )?,
                (Slot::B, true) => run(
                    &mut self.stages[index],
                    &self.meta,
                    &self.names[index],
                    &self.b,
                    &mut self.a,
                    sink,
                    true,
                )?,
                _ => unreachable!("destination slot never aliases the source slot"),
            };

            // No early exit here: every stage still has to see EOF.
            if emitted != Emit::Passthrough {
                src = if dst_a { Slot::A } else { Slot::B };
                dst_a = !dst_a;
            }
        }

        Ok(match src {
            Slot::Input => EMPTY,
            Slot::A => &self.a[..],
            Slot::B => &self.b[..],
        })
    }
}

/// Execute the plugin stage
fn run(
    plugin: &mut Box<dyn Plugin>,
    meta: &PipelineMeta,
    stage: &str,
    input: &[u8],
    out: &mut Vec<u8>,
    sink: &mut dyn EffectSink,
    eof: bool,
) -> Result<Emit> {
    out.clear();

    let mut emit = Emit::Pending;
    let mut ctx = Ctx::new(meta, stage, input, out, &mut emit, sink);

    if eof {
        if !input.is_empty() {
            plugin.on_bytes(&mut ctx, input)?;
        }
        plugin.on_eof(&mut ctx)?;
    } else {
        plugin.on_bytes(&mut ctx, input)?;
    }

    Ok(emit)
}

/// Execute a stage's tick. There is no input, so anything it wants downstream
/// it has to write.
fn run_tick(
    plugin: &mut Box<dyn Plugin>,
    meta: &PipelineMeta,
    stage: &str,
    out: &mut Vec<u8>,
    sink: &mut dyn EffectSink,
) -> Result<()> {
    out.clear();

    // The emit flag is write-only here: with no input there is nothing to pass
    // through, so whether anything came out is a question about `out`.
    let mut emit = Emit::Pending;
    let mut ctx = Ctx::new(meta, stage, EMPTY, out, &mut emit, sink);

    plugin.on_tick(&mut ctx)
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
        self.factories.insert(factory.name().to_string(), factory);
        self
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn PluginFactory>> {
        self.factories.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn PluginFactory>> {
        self.factories.values()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.factories.keys().map(String::as_str)
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

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
    }

    /// Comfortably past any schedule set at construction.
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    #[test]
    fn empty_pipeline_returns_the_input_slice() {
        let mut p = Pipeline::new(meta(), Vec::new());
        let mut sink = Recorder::default();
        let input = b"hello";

        let out = p.process(input, &mut sink).unwrap();
        assert!(std::ptr::eq(out.as_ptr(), input.as_ptr()));
    }

    #[test]
    fn observers_never_copy_the_payload() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Observer), Box::new(Observer)]);
        let mut sink = Recorder::default();
        let input = b"payload";

        let out = p.process(input, &mut sink).unwrap();

        assert!(
            std::ptr::eq(out.as_ptr(), input.as_ptr()),
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
        assert_eq!(p.finish(&mut sink).unwrap(), b"DCBA");
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
        assert_eq!(p.tick(now, &mut sink).unwrap(), Some(&b"PING"[..]));
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

        assert_eq!(p.tick(later(), &mut sink).unwrap(), Some(&b""[..]));
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

        assert_eq!(p.tick(later(), &mut sink).unwrap(), Some(&b"ping"[..]));
        assert!(
            sink.writes.is_empty(),
            "the observer sits above the beacon and saw nothing",
        );

        assert_eq!(p.process(b"payload", &mut sink).unwrap(), b"payload");
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

        assert_eq!(p.tick(now, &mut sink).unwrap(), Some(&b"ping"[..]));
        assert_eq!(p.tick(now, &mut sink).unwrap(), Some(&b""[..]));
        assert!(p.tick(now, &mut sink).unwrap().is_none());
    }

    #[test]
    fn transform_then_observe_keeps_the_transformed_bytes() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Upper), Box::new(Observer)]);
        let mut sink = Recorder::default();

        assert_eq!(p.process(b"hi", &mut sink).unwrap(), b"HI");
        assert_eq!(sink.writes[0].1, b"HI".to_vec());
    }
}
