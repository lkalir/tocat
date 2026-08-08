//! `throttle` - hold a path to a bandwidth ceiling.
//!
//! ```toml
//! [[plugin]]
//! name = "throttle"
//! rate = "1MiB"
//! ```
//!
//! ```console
//! $ tocat file:big.iso tcp:host:9000 'throttle,rate=256k'
//! ```
//!
//! # It slows the reader, it does not buffer
//!
//! The obvious implementation holds bytes back and releases them on a timer.
//! This one does the opposite: every chunk passes through untouched, and the
//! stage asks the host to wait before reading again with [`Ctx::pace`]. Nothing
//! is ever queued here, so there is no ceiling to tune and no memory to grow
//! when the source outruns the limit.
//!
//! It also throttles the right end. A read that does not happen leaves the
//! receive buffer full, which closes the TCP window, which slows the peer at
//! source. Buffering here would let the sender keep sending at full speed into
//! memory that has to go somewhere, which is a queue, not a limit.
//!
//! # Allowance
//!
//! A token bucket: allowance accrues at `rate` bytes per second up to `burst`,
//! and each chunk spends its own length. Spending is not capped, so a chunk
//! larger than the bucket is paid for with a proportionally longer wait rather
//! than being split, which is what keeps the stage safe on a datagram path.
//! The bucket starts full, so a path that has just opened may move `burst`
//! bytes before the ceiling first bites.
//!
//! # Granularity is the read buffer
//!
//! The wait is applied between reads, so the stall lands in units of chunks.
//! At the 256 KiB default buffer, `rate = "64k"` is four seconds of silence
//! followed by 256 KiB in one go, which averages out but is not smooth. For
//! smooth pacing give the relay a buffer at or below the per-second rate
//! (`-b 64k` here). The average holds either way.
//!
//! Instances are per path: the default `direction = "both"` builds one each
//! way, each with its own bucket, so `rate = "1MiB"` is a megabyte per second
//! in each direction rather than between them.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tocat_api::{BuildCtx, ByteSize, Ctx, Plugin, PluginError, PluginFactory, Result, Stage};

pub const NAME: &str = "throttle";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ThrottleConfig {
    /// The ceiling, in bytes per second.
    #[serde(alias = "bandwidth", alias = "bps")]
    pub rate: ByteSize,

    /// How much unused allowance may accumulate, so a path that has been idle
    /// can resume at full speed briefly instead of from nothing. Defaults to
    /// one second of `rate`.
    #[serde(default)]
    pub burst: Option<ByteSize>,
}

pub struct Throttle {
    /// Bytes per second.
    rate: f64,
    /// Bucket capacity, in bytes.
    burst: f64,
    /// Allowance in hand. Negative is debt, and the wait is how long it takes
    /// to earn it back.
    tokens: f64,
    last: Instant,
}

impl Throttle {
    /// The wait this stage would ask for right now, or `None` when the path is
    /// within its allowance.
    fn debt(&self) -> Option<Duration> {
        (self.tokens < 0.0).then(|| Duration::from_secs_f64(-self.tokens / self.rate))
    }
}

impl Plugin for Throttle {
    fn name(&self) -> &str {
        NAME
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;

        // Earn for the time that passed, capped at the bucket, then spend the
        // chunk. Spending uncapped is deliberate: see the module docs.
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst) - input.len() as f64;

        ctx.pass_through();

        if let Some(wait) = self.debt() {
            ctx.pace(wait);
        }

        Ok(())
    }

    /// Nothing is buffered, split, coalesced or emitted on a schedule: a
    /// datagram goes out as the datagram it arrived as, just later.
    fn datagram_safe(&self) -> bool {
        true
    }
}

pub struct ThrottleFactory;

impl PluginFactory for ThrottleFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "hold this path to a bandwidth ceiling"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: ThrottleConfig = ctx.config()?;
        let rate = config.rate.bytes();

        if rate == 0 {
            return Err(PluginError::config(
                NAME,
                "rate must be greater than zero; to stop a stream use the `limit` plugin",
            ));
        }

        // A bucket smaller than one chunk would only mean the debt is paid one
        // chunk in arrears, but a zero bucket makes the first read wait for
        // bytes it has already been given, so the floor is one byte.
        let burst = config.burst.map_or(rate, ByteSize::bytes).max(1);

        Ok(Stage::filter(Throttle {
            rate: rate as f64,
            burst: burst as f64,
            tokens: burst as f64,
            last: Instant::now(),
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
        pace: Duration,
    }

    impl EffectSink for Recorder {
        fn write(&mut self, _channel: ChannelId, _bytes: &[u8]) {}

        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}

        fn pace(&mut self, delay: Duration) {
            self.pace = self.pace.max(delay);
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

    fn build(config: serde_json::Value) -> Box<dyn Plugin> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: NAME,
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage, &mut host);

        match ThrottleFactory.build(&mut ctx).expect("build") {
            Stage::Filter(plugin) => plugin,
            Stage::External(_) => unreachable!("throttle is a filter"),
        }
    }

    fn feed(plugin: &mut dyn Plugin, sink: &mut Recorder, input: &[u8]) -> Emit {
        let meta = meta();
        let mut emission = Emission::new();

        {
            let mut ctx = Ctx::new(&meta, NAME, input, &mut emission, sink);
            plugin.on_bytes(&mut ctx, input).expect("on_bytes");
        }

        assert!(
            emission.bytes().is_empty(),
            "throttle must never copy the payload",
        );

        emission.emit()
    }

    #[test]
    fn the_payload_is_never_touched() {
        let mut plugin = build(json!({"rate": "1k"}));
        let mut sink = Recorder::default();

        assert_eq!(feed(&mut *plugin, &mut sink, b"hello"), Emit::Passthrough);
    }

    #[test]
    fn traffic_within_the_burst_is_not_paced() {
        let mut plugin = build(json!({"rate": "1k", "burst": "1k"}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, &[0u8; 512]);

        assert_eq!(sink.pace, Duration::ZERO, "the bucket starts full");
    }

    #[test]
    fn overspending_asks_for_the_time_it_costs() {
        let mut plugin = build(json!({"rate": "1k", "burst": "1k"}));
        let mut sink = Recorder::default();

        // 1024 empties the bucket, the next 1024 is a full second of debt.
        feed(&mut *plugin, &mut sink, &[0u8; 1024]);
        feed(&mut *plugin, &mut sink, &[0u8; 1024]);

        let asked = sink.pace.as_secs_f64();
        assert!(
            (0.95..=1.0).contains(&asked),
            "expected about a second of waiting, got {asked}",
        );
    }

    #[test]
    fn a_chunk_larger_than_the_bucket_waits_rather_than_splitting() {
        let mut plugin = build(json!({"rate": "1k", "burst": "1k"}));
        let mut sink = Recorder::default();

        assert_eq!(
            feed(&mut *plugin, &mut sink, &[0u8; 4096]),
            Emit::Passthrough,
            "the chunk still goes out whole",
        );

        let asked = sink.pace.as_secs_f64();
        assert!(
            (2.95..=3.0).contains(&asked),
            "3 KiB of debt at 1 KiB/s is about three seconds, got {asked}",
        );
    }

    #[test]
    fn a_rate_of_zero_is_rejected() {
        let map = json!({"rate": 0}).as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: NAME,
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage, &mut host);

        assert!(ThrottleFactory.build(&mut ctx).is_err());
    }
}
