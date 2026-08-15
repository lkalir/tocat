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

use serde::{Deserialize, Serialize};
use tocat_api::{
    Boundaries, BuildCtx, ByteSize, Ctx, Plugin, PluginError, PluginFactory, Result, Stage,
};

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
    pub bytes: Option<ByteSize>,

    /// How many `on_bytes` calls to let past before ending the stream.
    ///
    /// Shares [`ByteSize`]'s grammar so that a count is written the way every
    /// other quantity in a config is, which means the suffixes are binary here
    /// too: `packets = "1k"` is 1024 packets. Only the parse is borrowed. The
    /// cap is reported as the plain number it is, since `ByteSize`'s own
    /// `Display` would announce a packet limit in kibibytes.
    #[serde(alias = "chunks", alias = "messages")]
    pub packets: Option<ByteSize>,

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
    },
    Packets {
        cap: u64,
        seen: u64,
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

        let reason = match &self.kind {
            LimitKind::Bytes { cap, seen, .. } => format!(
                "limit of {} reached at {}",
                ByteSize(*cap as usize),
                ByteSize(*seen as usize),
            ),
            LimitKind::Packets { cap, .. } => {
                let unit = if *cap == 1 { "packet" } else { "packets" };

                format!("limit of {cap} {unit} reached")
            }
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
            LimitKind::Packets { cap, seen } => {
                *seen += 1;
                ctx.pass_through();

                *seen >= *cap
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
            (Some(bytes), None) => LimitKind::Bytes {
                cap: bytes.bytes() as u64,
                seen: 0,
                at_limit: config.at_limit.unwrap_or_default(),
            },
            (None, Some(packets)) => {
                // `bytes()` is the accessor whatever the quantity is: the
                // packet count borrows the size grammar and nothing else.
                let cap = packets.bytes() as u64;

                if config.at_limit.is_some() {
                    return Err(PluginError::config(
                        NAME,
                        "at-limit means nothing in a packet limit; it is an option of a byte \
                         limit, where one chunk straddles the cap",
                    ));
                }

                // The count is made when a chunk arrives, so a stage told to
                // pass none of them has no moment at which to say so: the
                // first chunk would have to pass before the halt.
                if cap == 0 {
                    return Err(PluginError::config(
                        NAME,
                        "packets must be at least 1: the count is made as a chunk passes, so a \
                         limit of none can never be announced",
                    ));
                }

                LimitKind::Packets { cap, seen: 0 }
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
            Some(ByteSize(4))
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
    fn a_limit_of_no_packets_is_refused() {
        assert!(
            try_build(json!({"packets": 0})).is_err(),
            "the first chunk would have to pass before the halt",
        );
    }
}
