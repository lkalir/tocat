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

use std::{collections::BTreeMap, fmt, sync::Arc};

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

        Self {
            meta,
            stages,
            names,
            a: Vec::new(),
            b: Vec::new(),
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

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
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
    fn transform_then_observe_keeps_the_transformed_bytes() {
        let mut p = Pipeline::new(meta(), vec![Box::new(Upper), Box::new(Observer)]);
        let mut sink = Recorder::default();

        assert_eq!(p.process(b"hi", &mut sink).unwrap(), b"HI");
        assert_eq!(sink.writes[0].1, b"HI".to_vec());
    }
}
