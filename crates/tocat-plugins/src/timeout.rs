//! `timeout` - end a path that has gone quiet.
//!
//! ```toml
//! [[plugin]]
//! name = "timeout"
//! direction = "source-to-sink"
//! timeout = "30s"
//! ```
//!
//! A pure observer: it forwards every chunk untouched and its only output is
//! [`Ctx::halt`], so ending a transfer this way is upstream end of stream
//! arriving early rather than a failure. Whatever the stages below are holding
//! is flushed, the sink is closed, and tocat exits successfully.
//!
//! # Why this counts ticks instead of asking for one
//!
//! The obvious implementation asks for a tick every `timeout` and halts on the
//! first one. It fires late, by up to a whole timeout, because the host owns
//! the timer: the segment's timer runs at a fixed cadence (the shortest period
//! any stage in it asked for) and a deadline set by [`Ctx::rearm`] is only
//! noticed at the next wakeup at or after it. A byte arriving just after a
//! wakeup pushes the deadline just past the following one, so a 30s timeout
//! fires at 60s.
//!
//! So the stage asks for a period a fraction of the timeout and counts
//! consecutive idle ticks instead, which bounds the error at one tick rather
//! than at one timeout. It also rearms on every chunk, so the count starts
//! from the last byte rather than from wherever the cadence had reached, and
//! the two together land the halt within a tick of the timeout.
//!
//! The cost is [`GRANULARITY`] wakeups per timeout per direction per
//! connection, and one clock read per chunk for the rearm. Both multiply under
//! `fork`, which is the reason the divisor is small and floored at
//! [`MIN_TICK`].

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tocat_api::{
    Boundaries, BuildCtx, Ctx, Interval, Plugin, PluginError, PluginFactory, Result, Stage,
};

pub const NAME: &str = "timeout";

/// How many ticks make up one timeout window.
///
/// The timeout lands within one tick of where it was asked for, so this is the
/// accuracy dial: four means "no later than 25% over". Raising it costs a
/// wakeup per direction per connection per timeout window.
const GRANULARITY: u32 = 4;

/// The fastest timer this stage will ask the host for.
///
/// A one-off `timeout=20ms` should not turn into a 5ms wakeup on every forked
/// connection. Below this the stage simply gets coarser: the tick is the whole
/// timeout and the halt can land at up to twice it.
const MIN_TICK: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TimeoutConfig {
    /// How long a path may go without carrying a byte before it is ended.
    ///
    /// Takes the usual duration grammar: `30`, `30s`, `2m`, `1m30s`.
    #[serde(alias = "wait", alias = "inactivity", alias = "idle")]
    pub timeout: Interval,
}

pub struct Timeout {
    /// Consecutive idle ticks that add up to the timeout. At least 1.
    limit: u32,
    /// Idle ticks since the last chunk, or since the path opened.
    idle: u32,
    /// The period asked for. Held only so `tick_interval` can answer it, since
    /// the host reads that once, after construction.
    tick: Duration,
    /// The configured timeout, kept for the halt message.
    timeout: Interval,
}

impl Plugin for Timeout {
    fn name(&self) -> &str {
        NAME
    }

    /// Read once, at the end of construction, so this is a plain field read.
    fn tick_interval(&self) -> Option<Duration> {
        Some(self.tick)
    }

    /// The clock, not the traffic, is what drives this stage: a stalled stream
    /// and a finished one are indistinguishable from `on_bytes` alone.
    fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        self.idle += 1;

        if self.idle >= self.limit {
            // Not an error: the host treats this as upstream EOF arriving
            // early, cascades `on_eof` through the stages below, and exits
            // successfully. Saying it once is enough, since the host stops
            // reading and no further tick can arrive.
            ctx.halt(&format!("no data for {}", self.timeout));
        }

        Ok(())
    }

    /// Nothing is inspected and nothing is copied: the stage below is handed
    /// the same slice. The only work is resetting the count and asking for the
    /// schedule to be restarted, so the window is measured from this byte.
    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, _input: &[u8]) -> Result<()> {
        ctx.pass_through();

        self.idle = 0;
        ctx.rearm();

        Ok(())
    }

    /// Safe on a datagram path: no bytes are held across calls, none are
    /// emitted from a tick, and a message goes out as the message it arrived
    /// as. Halting between datagrams is a short transfer, not a corrupt one.
    fn boundaries(&self) -> Boundaries {
        Boundaries::Preserve
    }
}

pub struct TimeoutFactory;

impl PluginFactory for TimeoutFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "end this direction when it has carried nothing for a while"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: TimeoutConfig = ctx.config()?;
        let timeout = config.timeout.duration();

        // A zero timeout would halt the path on its first tick, before
        // anything could arrive, which is a way of writing "relay nothing"
        // that nobody means.
        if timeout.is_zero() {
            return Err(PluginError::config(
                NAME,
                "timeout must be greater than zero",
            ));
        }

        // Never finer than MIN_TICK, and never coarser than the timeout
        // itself, which is what keeps `limit` at least 1 for a timeout below
        // MIN_TICK.
        let tick = (timeout / GRANULARITY).max(MIN_TICK).min(timeout);

        // Ceiling division: a window that does not divide evenly rounds up, so
        // the halt is never early. `limit` is at least 1 because tick <=
        // timeout.
        let limit = timeout.as_nanos().div_ceil(tick.as_nanos());
        let limit = u32::try_from(limit).unwrap_or(u32::MAX).max(1);

        Ok(Stage::filter(Timeout {
            limit,
            idle: 0,
            tick,
            timeout: config.timeout,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tocat_api::{
        ChannelId, ChannelTarget, Direction, EffectSink, Emission, Emit, HostBuilder, LogLevel,
        PipelineMeta, StageInfo,
    };

    use super::*;

    #[derive(Default)]
    struct Recorder {
        halts: Vec<String>,
    }

    impl EffectSink for Recorder {
        fn write(&mut self, _channel: ChannelId, _bytes: &[u8]) {}
        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}
        fn halt(&mut self, stage: &str, reason: &str) {
            self.halts.push(format!("{stage}: {reason}"));
        }
    }

    struct NullHost;

    impl HostBuilder for NullHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> Result<ChannelId> {
            Ok(ChannelId(0))
        }
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "tcp://a", "STDIO")
    }

    fn build(config: Value) -> Result<Box<dyn Plugin>> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: NAME,
            upstream: "tcp://a",
            downstream: "STDIO",
        };
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage, &mut host);

        match TimeoutFactory.build(&mut ctx)? {
            Stage::Filter(plugin) => Ok(plugin),
            Stage::External(_) => unreachable!("timeout is a filter"),
        }
    }

    /// One call, and what it emitted and asked for.
    fn feed(plugin: &mut dyn Plugin, sink: &mut Recorder, input: &[u8]) -> Emission {
        let meta = meta();
        let mut emission = Emission::new();
        {
            let mut ctx = Ctx::new(&meta, NAME, input, &mut emission, sink);
            plugin.on_bytes(&mut ctx, input).expect("on_bytes");
        }
        emission
    }

    fn tick(plugin: &mut dyn Plugin, sink: &mut Recorder) -> Emission {
        let meta = meta();
        let mut emission = Emission::new();
        {
            let mut ctx = Ctx::new(&meta, NAME, &[], &mut emission, sink);
            plugin.on_tick(&mut ctx).expect("on_tick");
        }
        emission
    }

    #[test]
    fn asks_for_a_fraction_of_the_timeout() {
        let plugin = build(json!({ "timeout": "40s" })).expect("build");
        assert_eq!(plugin.tick_interval(), Some(Duration::from_secs(10)));
    }

    #[test]
    fn a_short_timeout_is_floored_rather_than_spinning() {
        let plugin = build(json!({ "timeout": "20ms" })).expect("build");
        assert_eq!(plugin.tick_interval(), Some(Duration::from_millis(20)));
    }

    #[test]
    fn halts_after_a_whole_window_of_silence() {
        let mut plugin = build(json!({ "timeout": "40s" })).expect("build");
        let mut sink = Recorder::default();

        for _ in 0..GRANULARITY - 1 {
            tick(plugin.as_mut(), &mut sink);
            assert!(sink.halts.is_empty(), "halted early");
        }

        tick(plugin.as_mut(), &mut sink);
        assert_eq!(sink.halts.len(), 1);
        assert!(sink.halts[0].contains("no data for 40s"));
    }

    #[test]
    fn traffic_resets_the_window_and_costs_nothing() {
        let mut plugin = build(json!({ "timeout": "40s" })).expect("build");
        let mut sink = Recorder::default();

        for _ in 0..GRANULARITY * 3 {
            tick(plugin.as_mut(), &mut sink);

            let emission = feed(plugin.as_mut(), &mut sink, b"ping");

            // A pure observer must never materialise the payload.
            assert_eq!(emission.emit(), Emit::Passthrough);
            assert!(emission.bytes().is_empty());
            assert!(emission.rearm_requested(), "the window must be restarted");
        }

        assert!(sink.halts.is_empty(), "a busy path timed out");
    }

    #[test]
    fn rejects_a_zero_timeout() {
        assert!(build(json!({ "timeout": 0 })).is_err());
    }

    #[test]
    fn takes_its_aliases_and_rejects_anything_else() {
        assert!(build(json!({ "wait": "10s" })).is_ok());
        assert!(build(json!({ "idle": "10s" })).is_ok());
        assert!(build(json!({ "timeuot": "10s" })).is_err());
        assert!(build(json!({})).is_err());
    }
}
