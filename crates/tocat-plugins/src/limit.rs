//! `limit` - end a transfer after a fixed number of bytes or packets.
//!
//! ```toml
//! [[plugin]]
//! name = "limit"
//! bytes = "10MiB"
//! ```
//!
//! ```console
//! $ tocat tcp:host:9000 limit,bytes=1MiB file:head.bin,truncate
//! $ tocat udp-listen:9000 limit,packets=100 file:capture.bin
//! ```
//!
//! Counts what passes *its own position*, so where it sits matters: before a
//! `compress` stage it caps the payload, after it caps the wire. Direction
//! matters too: `direction = "both"` builds one instance per path, each with
//! its own budget, so `bytes = "1MiB"` means a megabyte each way and not a
//! megabyte between them.
//!
//! # Two counters, one stage
//!
//! `bytes` counts what each chunk carries, `packets` counts the calls. Which
//! one an instance runs is settled at build and held in [`LimitKind`], so the
//! per-chunk path matches a two-variant enum and no instance carries a counter
//! it never reads. Both are written in [`ByteSize`]'s grammar and both resolve
//! to a `u64` cap here, so the only thing that separates them below this point
//! is what is added to `seen`.
//!
//! Both at once is refused rather than run as one instance with two budgets,
//! because two entries already express that and express it more clearly:
//! `limit,packets=100 limit,bytes=1MiB` stops on whichever is reached first,
//! and each halt names the limit that was actually hit. `at-limit` is likewise
//! refused alongside `packets`, since a packet is counted only once it has
//! passed whole and there is no crossing chunk to decide about.
//!
//! # What a packet is here
//!
//! One `on_bytes` call. On a datagram path that is exactly one message, which
//! is what the option is for. On a byte stream it is one read, sized by the
//! copy buffer and by when the peer's bytes happened to arrive, so the same
//! transfer counted twice need not agree.
//!
//! The stage still does not declare `Needs::Upstream` to rule the stream case
//! out. A requirement is for a stage that cannot do its job where it was put,
//! and this one can: it counts calls and halts, exactly as configured. A read
//! count is a coarse proxy for a message count and is sometimes the thing
//! wanted, and the requirement would also refuse paths where calls do track
//! something real, such as a `process` stage above passing on its child's
//! writes. An `unframe` above turns calls back into messages on a stream.
//!
//! Since a spelling cannot be recovered once serde has matched an alias, the
//! halt line says "packets" whichever of `packets`, `chunks` or `messages` was
//! written.
//!
//! # Ending a stream is not an error
//!
//! On reaching the limit the stage asks the host to stop reading, through
//! [`Ctx::halt`]. That is upstream end of stream arriving early: bytes already
//! emitted are written, the remaining stages get their `on_eof`, sinks are
//! flushed and closed, and tocat exits successfully. Failing the pipeline
//! instead would report a deliberate stop as a fault and, worse, would abandon
//! whatever the downstream stages were holding.
//!
//! # The chunk that crosses the line
//!
//! Only a byte limit has one. Exactly one chunk straddles it, and there are
//! exactly three things to do with that chunk, which is the whole of
//! `at-limit`:
//!
//! | Mode        | The crossing chunk | Guarantee            |
//! |-------------|--------------------|----------------------|
//! | `drop`      | discarded whole    | at most `bytes`      |
//! | `exact`     | split at the limit | exactly `bytes`      |
//! | `overshoot` | forwarded whole    | at least `bytes`     |
//!
//! `exact` is the default and is what a byte count usually means. `drop` is
//! the hard ceiling: never put more than this many bytes into that file, that
//! pipe, that quota. `overshoot` is the one to reach for on a datagram path,
//! where a limit landing mid-message leaves a real choice: dropping throws
//! away a message already received on a transfer that is ending anyway, while
//! overshooting delivers it whole and then stops.
//!
//! Splitting is also the only thing here that is unsafe on a datagram path, so
//! `drop` and `overshoot` are both safe and `exact` is not: half a datagram is
//! a corrupt message rather than a short read. A packet limit never splits.

use rand::{Rng, RngExt, SeedableRng};
use rand_distr::{Distribution, Geometric};
use serde::{Deserialize, Serialize};
use tocat_api::{
    Boundaries, BuildCtx, ByteSize, Ctx, Plugin, PluginError, PluginFactory, Ratio, Result, Stage,
};

use crate::random::Prng;

pub const NAME: &str = "limit";

/// What to do with the one chunk that crosses the limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AtLimit {
    /// Discard it, ending under the limit. At most `bytes` are passed on.
    Drop,
    /// Split it, ending on the requested byte. Exactly `bytes` are passed on.
    #[default]
    Exact,
    /// Forward it whole, ending over the limit. At least `bytes` are passed
    /// on, and no message is ever cut in half.
    Overshoot,
}

impl AtLimit {
    /// Whether this mode cuts a chunk in two, which is the one thing a
    /// datagram path cannot survive.
    #[must_use]
    pub fn splits(self) -> bool {
        matches!(self, Self::Exact)
    }
}

/// A limit, either written down or drawn.
///
/// Three spellings, all resolved to a `u64` before the counting code sees
/// anything, so the hot path is the same whichever was written:
///
/// * `bytes=1MiB`, a number someone chose.
/// * `bytes=1KiB..1MiB`, uniform in a window. For when the stop has to happen
///   and has to be somewhere in particular.
/// * `bytes=25%`, a rate: the chance of stopping at each byte. For when the
///   property under test is "does anything break if this ends at an arbitrary
///   point", run many times, where a short stream often finishing untouched is
///   correct rather than a miss.
///
/// A rate is per **byte**, not per chunk, which is what makes it independent of
/// how the peer happened to write and of `-b`: four transfers of one byte and
/// one transfer of four behave identically under the same seed. A per chunk
/// roll would not, and a stopping point that moves with the buffer size cannot
/// be replayed.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Cap {
    /// A count, in the size grammar: `4096`, `1MiB`.
    Fixed(ByteSize),
    /// A range, as `MIN..MAX`, inclusive at both ends.
    Between(Between),
    /// A per byte probability: `25%`, `1/4`, `0.25`.
    Rate(Ratio),
}

/// `MIN..MAX`, in the same grammar as a fixed cap.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Between {
    pub min: ByteSize,
    pub max: ByteSize,
}

impl<'de> Deserialize<'de> for Between {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let raw = String::deserialize(deserializer)?;

        let (min, max) = raw
            .split_once("..")
            .ok_or_else(|| D::Error::custom(format!("{raw} is not MIN..MAX")))?;

        Ok(Self {
            min: min.trim().parse().map_err(D::Error::custom)?,
            max: max.trim().parse().map_err(D::Error::custom)?,
        })
    }
}

/// The offset a per byte rate stops at.
///
/// Rolling once per byte and drawing the offset once are the same
/// distribution, and the second is O(1) rather than O(bytes): a coin flipped
/// until it lands is geometric, so one uniform gives how many bytes pass first.
/// That keeps the copy path exactly as it is, which is the point, since the
/// alternative is a random number per byte on the hot path.
///
/// `p` at or above one stops before anything passes, which is the reading that
/// matches the formula: the probability is of stopping *at* a byte, so
/// certainty stops at the first. `Geometric` counts failures before the first
/// success, which is the same offset, and it saturates at `u64::MAX` for a `p`
/// too small to ever stop, which is "effectively never".
fn geometric<R: Rng>(p: f64, rng: &mut R) -> u64 {
    if p >= 1.0 {
        return 0;
    }

    match Geometric::new(p) {
        Ok(dist) => dist.sample(rng),
        // `p` arrives as an already validated ratio, so the constructor
        // cannot reject it. Saturating rather than panicking keeps a bad
        // configuration from tearing down a live pipeline: it reads as a
        // limit that never fires.
        Err(_) => u64::MAX,
    }
}

impl Cap {
    /// The number to count to, and the seed it came from if it was drawn.
    ///
    /// The seed is reported so that the halt message can carry it: a run that
    /// stopped somewhere interesting is only useful if the next run can stop
    /// there too.
    fn resolve(self, seed: Option<u64>) -> Result<(u64, Option<u64>), PluginError> {
        let range = match self {
            Cap::Fixed(size) => return Ok((size.bytes() as u64, None)),
            Cap::Between(range) => Some(range),
            Cap::Rate(_) => None,
        };

        // Without one, a seed is taken from the clock and reported, so an
        // unseeded run is still replayable after the fact.
        let seed = seed.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos() as u64)
        });

        // The generator is local: the cap is drawn once here and the seed is
        // what the halt message needs, so nothing downstream holds a `Prng`.
        let mut prng = Prng::seed_from_u64(seed);

        let Some(range) = range else {
            let Cap::Rate(rate) = self else {
                unreachable!()
            };
            let p = rate.value();

            if !(0.0..=1.0).contains(&p) {
                return Err(PluginError::config(
                    NAME,
                    format!("{p} is not a probability: give a rate between 0% and 100%"),
                ));
            }

            return Ok((geometric(p, &mut prng), Some(seed)));
        };

        let min = range.min.bytes() as u64;
        let max = range.max.bytes() as u64;

        if min > max {
            return Err(PluginError::config(
                NAME,
                format!("{min}..{max} is empty: the minimum is above the maximum"),
            ));
        }

        Ok((prng.random_range(min..=max), Some(seed)))
    }
}

/// Every option optional, and which combinations are legal settled in
/// `LimitFactory::build` rather than by the shape of this type.
///
/// An untagged enum or a flattened one would say "bytes or packets" in the
/// type, and would cost more than it says: `#[serde(flatten)]` reaches the
/// host's deserializer through `deserialize_map`, which has no field list, so
/// `at_limit` and `atLimit` would stop resolving to `at-limit`; untagged goes
/// through `deserialize_any`, which loses the same normalization for variant
/// names; and neither can honour `deny_unknown_fields`, so `at-limit` beside
/// `packets` would be ignored instead of refused.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LimitConfig {
    /// How many bytes to let past before ending the stream.
    #[serde(alias = "max", alias = "size")]
    pub bytes: Option<Cap>,

    /// How many `on_bytes` calls to let past before ending the stream.
    ///
    /// Shares [`ByteSize`]'s grammar so that a count is written the way every
    /// other quantity in a config is, which means the suffixes are binary here
    /// too: `packets = "1k"` is 1024 packets. Only the parse is borrowed. The
    /// cap is reported as the plain number it is, since `ByteSize`'s own
    /// `Display` would announce a packet limit in kibibytes.
    #[serde(alias = "chunks", alias = "messages")]
    pub packets: Option<Cap>,

    /// The seed for a drawn cap. Ignored by a fixed one, and refused with it,
    /// since a seed that does nothing reads as a setting that does.
    pub seed: Option<u64>,

    /// What to do with the chunk that crosses a byte limit.
    ///
    /// `Option` rather than defaulted here so that giving it alongside
    /// `packets`, where it decides nothing, can be told from leaving it out.
    pub at_limit: Option<AtLimit>,
}

/// The counter an instance runs, fixed at build.
///
/// Private, and deliberately not the config type: the config is what was
/// written and this is what will be counted, with the sizes already resolved
/// to `u64` and the `at-limit` default already applied.
enum LimitKind {
    Bytes {
        cap: u64,
        seen: u64,
        at_limit: AtLimit,
        /// `Some` when the cap was drawn, and only so the halt message can say
        /// how to reproduce it.
        seed: Option<u64>,
    },
    Packets {
        cap: u64,
        seen: u64,
        seed: Option<u64>,
    },
}

pub struct Limit {
    kind: LimitKind,
    /// Set once the limit is announced, so a chunk that was already in flight
    /// from an upstream stage cannot announce it again.
    stopped: bool,
}

impl Limit {
    /// Announce the end. A byte limit reports where the transfer actually
    /// stopped rather than the configured limit, since under `overshoot` they
    /// differ; a packet limit stops on the packet that reaches the cap, so
    /// there is only ever one number to report.
    fn stop(&mut self, ctx: &mut Ctx<'_>) {
        self.stopped = true;

        let (reason, seed) = match &self.kind {
            LimitKind::Bytes {
                cap, seen, seed, ..
            } => (
                format!(
                    "limit of {} reached at {}",
                    ByteSize(*cap as usize),
                    ByteSize(*seen as usize),
                ),
                *seed,
            ),
            LimitKind::Packets { cap, seed, .. } => {
                let unit = if *cap == 1 { "packet" } else { "packets" };

                (format!("limit of {cap} {unit} reached"), *seed)
            }
        };

        // A drawn cap says where it came from, so the run can be repeated.
        let reason = match seed {
            Some(seed) => format!("{reason} (seed={seed})"),
            None => reason,
        };

        ctx.halt(&reason);
    }
}

impl Plugin for Limit {
    fn name(&self) -> &str {
        NAME
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        if self.stopped {
            ctx.drop_chunk();
            return Ok(());
        }

        // Whether this chunk was the last one. Answered inside the match and
        // acted on outside it, so the borrow of `kind` is over before `stop`
        // takes the whole of `self`.
        let reached = match &mut self.kind {
            LimitKind::Bytes {
                cap,
                seen,
                at_limit,
                ..
            } => {
                // Saturating because `overshoot` leaves `seen` past `cap`.
                // Nothing can reach here in that state today (`stopped` is set
                // in the same call) but a panic one edit away is not worth the
                // subtraction.
                let remaining = cap.saturating_sub(*seen);
                let len = input.len() as u64;

                // The common case, and the only one on the hot path: still
                // under the limit, so the chunk goes on untouched and nothing
                // is copied.
                if len < remaining {
                    *seen += len;
                    ctx.pass_through();
                    return Ok(());
                }

                // A chunk landing exactly on the limit goes whole under every
                // mode: there is nothing to split, drop or overshoot.
                if len == remaining {
                    *seen += len;
                    ctx.pass_through();
                } else {
                    match *at_limit {
                        AtLimit::Drop => ctx.drop_chunk(),
                        AtLimit::Exact => {
                            ctx.forward(&input[..remaining as usize]);
                            *seen += remaining;
                        }
                        AtLimit::Overshoot => {
                            *seen += len;
                            ctx.pass_through();
                        }
                    }
                }

                true
            }
            // Nothing is ever copied or cut here: a packet is either inside the
            // count or after it, and the one that reaches the cap is passed on
            // whole before the stream ends.
            LimitKind::Packets { cap, seen, .. } => {
                // A cap of none is decided before the count rather than after
                // it, which is the only asymmetry between the two limits:
                // everywhere else a packet limit passes the chunk that reaches
                // the cap, and here there is no chunk it is allowed to pass.
                //
                // The byte limit expresses the same thing through `at-limit`,
                // where truncating a chunk to nothing is what a cap of zero
                // means. A packet cannot be truncated, so it is dropped.
                if *cap == 0 {
                    ctx.drop_chunk();

                    true
                } else {
                    *seen += 1;
                    ctx.pass_through();

                    *seen >= *cap
                }
            }
        };

        if reached {
            self.stop(ctx);
        }

        Ok(())
    }

    /// Safe on a datagram path unless the mode splits a message. Stopping
    /// between datagrams is a short transfer; stopping inside one is a
    /// corrupt message. Counting packets splits nothing.
    fn boundaries(&self) -> Boundaries {
        match &self.kind {
            LimitKind::Bytes { at_limit, .. } if at_limit.splits() => Boundaries::Fuse,
            _ => Boundaries::Preserve,
        }
    }
}

pub struct LimitFactory;

impl PluginFactory for LimitFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "end the stream after a fixed number of bytes or packets"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: LimitConfig = ctx.config()?;

        // A seed that changes nothing reads as a setting that does, so a fixed
        // cap refuses one rather than ignoring it.
        let drawn = matches!(config.bytes, Some(Cap::Between(_) | Cap::Rate(_)))
            || matches!(config.packets, Some(Cap::Between(_) | Cap::Rate(_)));

        if config.seed.is_some() && !drawn {
            return Err(PluginError::config(
                NAME,
                "seed is for a drawn limit: give a range as bytes=1KiB..1MiB, or a rate as \
                 bytes=25%",
            ));
        }

        let kind = match (config.bytes, config.packets) {
            (Some(_), Some(_)) => {
                return Err(PluginError::config(
                    NAME,
                    "bytes and packets are two limits, not one entry: give one of them, or two \
                     limit stages to stop on whichever is reached first",
                ));
            }
            (None, None) => {
                return Err(PluginError::config(
                    NAME,
                    "nothing to count: give bytes for a byte count or packets for a count of \
                     chunks arriving",
                ));
            }
            (Some(bytes), None) => {
                let (cap, seed) = bytes.resolve(config.seed)?;

                LimitKind::Bytes {
                    cap,
                    seen: 0,
                    at_limit: config.at_limit.unwrap_or_default(),
                    seed,
                }
            }
            (None, Some(packets)) => {
                // `bytes()` is the accessor whatever the quantity is: the
                // packet count borrows the size grammar and nothing else.
                let (cap, seed) = packets.resolve(config.seed)?;

                if config.at_limit.is_some() {
                    return Err(PluginError::config(
                        NAME,
                        "at-limit means nothing in a packet limit; it is an option of a byte \
                         limit, where one chunk straddles the cap",
                    ));
                }

                LimitKind::Packets { cap, seen: 0, seed }
            }
        };

        Ok(Stage::filter(Limit {
            kind,
            stopped: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tocat_api::{
        ChannelId, ChannelTarget, Direction, EffectSink, Emission, Emit, HostBuilder, LogLevel,
        PipelineMeta, Result as PluginResult, StageInfo,
    };

    use super::*;

    #[derive(Default)]
    struct Recorder {
        halt: Option<String>,
    }

    impl EffectSink for Recorder {
        fn write(&mut self, _channel: ChannelId, _bytes: &[u8]) {}

        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}

        fn halt(&mut self, _stage: &str, reason: &str) {
            self.halt.get_or_insert_with(|| reason.to_string());
        }
    }

    struct NullHost;

    impl HostBuilder for NullHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> PluginResult<ChannelId> {
            Ok(ChannelId(0))
        }
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
    }

    fn stage() -> StageInfo<'static> {
        StageInfo {
            index: 0,
            total: 1,
            name: NAME,
            upstream: "src",
            downstream: "sink",
        }
    }

    /// A build as the host performs it, for the configurations that are
    /// refused rather than run.
    fn try_build(config: serde_json::Value) -> Result<Stage> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage(), &mut host);

        LimitFactory.build(&mut ctx)
    }

    fn build(config: serde_json::Value) -> Box<dyn Plugin> {
        match try_build(config).expect("build") {
            Stage::Filter(plugin) => plugin,
            Stage::External(_) => unreachable!("limit is a filter"),
        }
    }

    /// The config as the plugin's own deserialization sees it, for the cases
    /// that are about parsing rather than about bytes.
    fn build_config(config: serde_json::Value) -> LimitConfig {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let ctx = BuildCtx::new(NAME, &map, &meta, stage(), &mut host);

        ctx.config().expect("config")
    }

    /// One chunk, returning what the stage emitted: the borrowed input on
    /// passthrough, the buffer otherwise.
    fn feed(plugin: &mut dyn Plugin, sink: &mut Recorder, input: &[u8]) -> Vec<u8> {
        let meta = meta();
        let mut emission = Emission::new();

        {
            let mut ctx = Ctx::new(&meta, NAME, input, &mut emission, sink);
            plugin.on_bytes(&mut ctx, input).expect("on_bytes");
        }

        match emission.emit() {
            Emit::Passthrough => input.to_vec(),
            Emit::Buffered => emission.bytes().to_vec(),
            Emit::Pending => Vec::new(),
        }
    }

    #[test]
    fn bytes_under_the_limit_pass_untouched() {
        let mut plugin = build(json!({"bytes": 16}));
        let mut sink = Recorder::default();

        assert_eq!(feed(&mut *plugin, &mut sink, b"hello"), b"hello");
        assert!(sink.halt.is_none(), "nothing to stop for yet");
    }

    #[test]
    fn the_crossing_chunk_is_split_and_the_stream_ends() {
        let mut plugin = build(json!({"bytes": 8}));
        let mut sink = Recorder::default();

        assert_eq!(feed(&mut *plugin, &mut sink, b"12345"), b"12345");
        assert_eq!(feed(&mut *plugin, &mut sink, b"67890"), b"678");
        assert!(sink.halt.is_some(), "the limit must stop the read");
    }

    #[test]
    fn landing_exactly_on_the_limit_still_stops() {
        let mut plugin = build(json!({"bytes": 5}));
        let mut sink = Recorder::default();

        assert_eq!(feed(&mut *plugin, &mut sink, b"12345"), b"12345");
        assert!(sink.halt.is_some());
    }

    #[test]
    fn drop_discards_the_crossing_chunk_whole() {
        let mut plugin = build(json!({"bytes": 8, "at-limit": "drop"}));
        let mut sink = Recorder::default();

        assert_eq!(feed(&mut *plugin, &mut sink, b"12345"), b"12345");
        assert!(
            feed(&mut *plugin, &mut sink, b"67890").is_empty(),
            "at most `bytes` means the whole chunk goes",
        );
        assert!(sink.halt.is_some());
    }

    #[test]
    fn overshoot_forwards_the_crossing_chunk_whole() {
        let mut plugin = build(json!({"bytes": 8, "at-limit": "overshoot"}));
        let mut sink = Recorder::default();

        assert_eq!(feed(&mut *plugin, &mut sink, b"12345"), b"12345");
        assert_eq!(
            feed(&mut *plugin, &mut sink, b"67890"),
            b"67890",
            "at least `bytes` means the message is not cut",
        );
        assert!(sink.halt.is_some());
    }

    #[test]
    fn every_mode_takes_a_chunk_that_lands_on_the_limit_whole() {
        for mode in ["drop", "exact", "overshoot"] {
            let mut plugin = build(json!({"bytes": 5, "at-limit": mode}));
            let mut sink = Recorder::default();

            assert_eq!(
                feed(&mut *plugin, &mut sink, b"12345"),
                b"12345",
                "{mode} cut a chunk that needed no decision",
            );
            assert!(sink.halt.is_some(), "{mode} did not stop");
        }
    }

    #[test]
    fn a_chunk_arriving_after_the_limit_is_dropped_quietly() {
        let mut plugin = build(json!({"bytes": 4}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, b"12345");
        let first = sink.halt.clone();

        assert!(feed(&mut *plugin, &mut sink, b"more").is_empty());
        assert_eq!(sink.halt, first, "the limit is announced once");
    }

    #[test]
    fn splitting_is_what_makes_it_unsafe_on_datagrams() {
        let boundaries = |config| build(config).boundaries();

        assert_eq!(
            boundaries(json!({"bytes": 8})),
            Boundaries::Fuse,
            "exact splits",
        );
        assert_eq!(
            boundaries(json!({"bytes": 8, "at-limit": "drop"})),
            Boundaries::Preserve,
        );
        assert_eq!(
            boundaries(json!({"bytes": 8, "at-limit": "overshoot"})),
            Boundaries::Preserve,
        );
        assert_eq!(
            boundaries(json!({"packets": 8})),
            Boundaries::Preserve,
            "counting chunks cuts none of them",
        );
    }

    #[test]
    fn the_mode_is_spelled_however_you_like() {
        assert_eq!(
            build_config(json!({"bytes": 8, "at_limit": "Overshoot"})).at_limit,
            Some(AtLimit::Overshoot),
        );
    }

    #[test]
    fn an_unknown_mode_is_rejected() {
        assert!(try_build(json!({"bytes": 8, "at-limit": "sideways"})).is_err());
    }

    #[test]
    fn the_size_grammar_is_the_usual_one() {
        let mut plugin = build(json!({"bytes": "1k"}));
        let mut sink = Recorder::default();
        let chunk = vec![0u8; 1000];

        assert_eq!(feed(&mut *plugin, &mut sink, &chunk).len(), 1000);
        assert!(sink.halt.is_none(), "1k is 1024, so 1000 is under it");
    }

    #[test]
    fn packets_are_counted_one_per_chunk() {
        let mut plugin = build(json!({"packets": 2}));
        let mut sink = Recorder::default();

        assert_eq!(feed(&mut *plugin, &mut sink, b"one"), b"one");
        assert!(sink.halt.is_none(), "one of the two has passed");

        assert_eq!(feed(&mut *plugin, &mut sink, b"two"), b"two");
        assert!(sink.halt.is_some(), "the second reaches the limit");
    }

    #[test]
    fn the_chunk_that_reaches_the_packet_limit_passes_whole() {
        let mut plugin = build(json!({"packets": 1}));
        let mut sink = Recorder::default();
        let chunk = vec![0u8; 4096];

        assert_eq!(
            feed(&mut *plugin, &mut sink, &chunk).len(),
            4096,
            "a packet limit never cuts a chunk, however long it is",
        );
        assert_eq!(sink.halt.as_deref(), Some("limit of 1 packet reached"));
    }

    #[test]
    fn a_chunk_after_the_packet_limit_is_dropped_quietly() {
        let mut plugin = build(json!({"packets": 1}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, b"first");
        let first = sink.halt.clone();

        assert!(feed(&mut *plugin, &mut sink, b"second").is_empty());
        assert_eq!(sink.halt, first, "the limit is announced once");
    }

    #[test]
    fn a_packet_count_is_written_like_any_other_quantity() {
        assert_eq!(
            build_config(json!({"chunks": 4})).packets,
            Some(Cap::Fixed(ByteSize(4)))
        );

        let mut plugin = build(json!({"packets": "1k"}));
        let mut sink = Recorder::default();

        for _ in 0..1023 {
            feed(&mut *plugin, &mut sink, b"chunk");
        }
        assert!(
            sink.halt.is_none(),
            "1k is 1024 packets, as it is 1024 bytes"
        );

        feed(&mut *plugin, &mut sink, b"chunk");
        assert_eq!(
            sink.halt.as_deref(),
            Some("limit of 1024 packets reached"),
            "a count is reported as a count, not as a size",
        );
    }

    #[test]
    fn the_two_counters_are_not_combined() {
        assert!(try_build(json!({"bytes": 8, "packets": 4})).is_err());
    }

    #[test]
    fn a_limit_with_nothing_to_count_is_refused() {
        assert!(try_build(json!({})).is_err());
    }

    #[test]
    fn at_limit_is_an_option_of_a_byte_limit() {
        assert!(
            try_build(json!({"packets": 4, "at-limit": "drop"})).is_err(),
            "a packet limit has no crossing chunk to decide about",
        );
    }

    #[test]
    fn a_limit_of_no_packets_passes_nothing() {
        let mut plugin = build(json!({ "packets": 0}));
        let mut sink = Recorder::default();

        assert_eq!(feed(&mut *plugin, &mut sink, b"nothing").len(), 0);
        assert!(sink.halt.is_some(), "the limit must stop the read");
    }

    /// A drawn cap has to land inside the window at both ends. The draw is the
    /// only place a limit can quietly exceed what was configured.
    #[test]
    fn a_range_cap_stays_inside_the_range() {
        let cap = Cap::Between(Between {
            min: ByteSize(1024),
            max: ByteSize(2048),
        });

        for seed in 0..512 {
            let (drawn, reported) = cap.resolve(Some(seed)).expect("resolve");

            assert!((1024..=2048).contains(&drawn), "seed {seed} drew {drawn}");
            assert_eq!(reported, Some(seed));
        }
    }

    /// A window one wide has one answer, so an off by one shows as the wrong
    /// number rather than as a rare failure.
    #[test]
    fn a_range_of_one_draws_that_one() {
        let cap = Cap::Between(Between {
            min: ByteSize(4096),
            max: ByteSize(4096),
        });

        assert_eq!(cap.resolve(Some(1)).expect("resolve").0, 4096);
    }

    /// The number in the halt message is what `seed` takes back, for every
    /// drawn form. Without this the seed is decoration.
    #[test]
    fn a_reported_seed_reproduces_the_cap() {
        let caps = [
            Cap::Between(Between {
                min: ByteSize(1),
                max: ByteSize(1 << 20),
            }),
            Cap::Rate(Ratio(0.001)),
        ];

        for cap in caps {
            let (drawn, reported) = cap.resolve(None).expect("resolve");
            let seed = reported.expect("a drawn cap reports its seed");

            assert_eq!(
                cap.resolve(Some(seed)).expect("resolve"),
                (drawn, Some(seed))
            );
        }
    }

    /// Nothing was drawn, so there is no seed to report and none to refuse.
    #[test]
    fn a_fixed_cap_reports_no_seed() {
        assert_eq!(
            Cap::Fixed(ByteSize(10)).resolve(None).expect("resolve"),
            (10, None)
        );
    }

    /// The two ends of the rate: certainty stops before the first byte, and a
    /// rate of zero never stops.
    #[test]
    fn the_ends_of_the_rate_are_the_readings_that_match_the_formula() {
        let mut prng = Prng::seed_from_u64(1);

        assert_eq!(geometric(1.0, &mut prng), 0);
        assert_eq!(geometric(0.0, &mut prng), u64::MAX);
    }

    /// A halt names the seed so the run can be repeated.
    #[test]
    fn the_halt_message_carries_the_seed() {
        let mut plugin = build(json!({"bytes": "8..8", "seed": 99}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, &[0u8; 16]);

        assert_eq!(
            sink.halt.as_deref(),
            Some("limit of 8 reached at 8 (seed=99)")
        );
    }
}
