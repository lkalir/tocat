//! `limit` - end a transfer after a fixed number of bytes.
//!
//! ```toml
//! [[plugin]]
//! name = "limit"
//! bytes = "10MiB"
//! ```
//!
//! ```console
//! $ tocat tcp:host:9000 limit,bytes=1MiB file:head.bin,truncate
//! ```
//!
//! Counts what passes *its own position*, so where it sits matters: before a
//! `compress` stage it caps the payload, after it caps the wire. Direction
//! matters too. The default `direction = "both"` builds one instance per path,
//! each with its own budget, so `bytes = "1MiB"` means a megabyte each way and
//! not a megabyte between them.
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
//! Exactly one chunk straddles the limit, and there are exactly three things
//! to do with it, which is the whole of `at-limit`:
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
//! a corrupt message rather than a short read.

use serde::{Deserialize, Serialize};
use tocat_api::{BuildCtx, ByteSize, Ctx, Plugin, PluginFactory, Result, Stage};

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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LimitConfig {
    /// How many bytes to let past before ending the stream.
    #[serde(alias = "max", alias = "size")]
    pub bytes: ByteSize,

    /// What to do with the chunk that crosses the limit.
    #[serde(default)]
    pub at_limit: AtLimit,
}

pub struct Limit {
    cap: u64,
    seen: u64,
    at_limit: AtLimit,
    /// Set once the limit is announced, so a chunk that was already in flight
    /// from an upstream stage cannot announce it again.
    stopped: bool,
}

impl Limit {
    /// Announce the end. Reports where the transfer actually stopped rather
    /// than the configured limit, since under `overshoot` they differ.
    fn stop(&mut self, ctx: &mut Ctx<'_>) {
        self.stopped = true;
        ctx.halt(&format!(
            "limit of {} reached at {}",
            ByteSize(self.cap as usize),
            ByteSize(self.seen as usize),
        ));
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

        // Saturating because `overshoot` leaves `seen` past `cap`. Nothing can
        // reach here in that state today (`stopped` is set in the same call)
        // but a panic one edit away is not worth the subtraction.
        let remaining = self.cap.saturating_sub(self.seen);
        let len = input.len() as u64;

        // The common case, and the only one on the hot path: still under the
        // limit, so the chunk goes on untouched and nothing is copied.
        if len < remaining {
            self.seen += len;
            ctx.pass_through();
            return Ok(());
        }

        // A chunk landing exactly on the limit goes whole under every mode:
        // there is nothing to split, drop or overshoot.
        if len == remaining {
            self.seen += len;
            ctx.pass_through();
        } else {
            match self.at_limit {
                AtLimit::Drop => ctx.drop_chunk(),
                AtLimit::Exact => {
                    ctx.forward(&input[..remaining as usize]);
                    self.seen += remaining;
                }
                AtLimit::Overshoot => {
                    self.seen += len;
                    ctx.pass_through();
                }
            }
        }

        self.stop(ctx);
        Ok(())
    }

    /// Safe on a datagram path unless the mode splits a message. Stopping
    /// between datagrams is a short transfer; stopping inside one is a
    /// corrupt message.
    fn datagram_safe(&self) -> bool {
        !self.at_limit.splits()
    }
}

pub struct LimitFactory;

impl PluginFactory for LimitFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "end the stream after a fixed number of bytes"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: LimitConfig = ctx.config()?;

        Ok(Stage::filter(Limit {
            cap: config.bytes.bytes() as u64,
            seen: 0,
            at_limit: config.at_limit,
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

    fn build(config: serde_json::Value) -> Box<dyn Plugin> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage(), &mut host);

        match LimitFactory.build(&mut ctx).expect("build") {
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
        assert!(!build(json!({"bytes": 8})).datagram_safe(), "exact splits");
        assert!(build(json!({"bytes": 8, "at-limit": "drop"})).datagram_safe());
        assert!(build(json!({"bytes": 8, "at-limit": "overshoot"})).datagram_safe());
    }

    #[test]
    fn the_mode_is_spelled_however_you_like() {
        assert_eq!(
            build_config(json!({"bytes": 8, "at_limit": "Overshoot"})).at_limit,
            AtLimit::Overshoot,
        );
    }

    #[test]
    fn an_unknown_mode_is_rejected() {
        let map = json!({"bytes": 8, "at-limit": "sideways"})
            .as_object()
            .expect("object")
            .clone();
        let meta = meta();
        let mut host = NullHost;
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage(), &mut host);

        assert!(LimitFactory.build(&mut ctx).is_err());
    }

    #[test]
    fn the_size_grammar_is_the_usual_one() {
        let mut plugin = build(json!({"bytes": "1k"}));
        let mut sink = Recorder::default();
        let chunk = vec![0u8; 1000];

        assert_eq!(feed(&mut *plugin, &mut sink, &chunk).len(), 1000);
        assert!(sink.halt.is_none(), "1k is 1024, so 1000 is under it");
    }
}
