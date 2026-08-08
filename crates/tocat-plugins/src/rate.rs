//! `rate` - measure and report throughput at a point on a path.
//!
//! ```toml
//! [[plugin]]
//! name = "rate"
//! interval = "10s"
//! unit = "bits"
//! ```
//!
//! A pure observer: it calls `ctx.pass_through()` and never touches the
//! payload, so it costs one virtual call per chunk and can sit anywhere in a
//! chain, including on a datagram path. What it measures is the traffic *at its
//! own position*, which is the point of having it as a stage rather than a
//! flag. Put one either side of a codec and the two reports are the before and
//! the after:
//!
//! ```console
//! $ tocat -f file:big.iso -t tcp:host:9000 -p 'rate,as=plain' -p compress -p 'rate,as=wire'
//! ```
//!
//! # Reporting is driven by the clock
//!
//! Reports come from [`Plugin::on_tick`], not from chunks arriving, so they
//! land on schedule and a stalled stream is reported rather than silent. The
//! per-chunk path is two adds and a branch: it never reads the clock after the
//! first chunk has stamped the start.
//!
//! A stall is announced once and then not repeated, because these are log
//! lines and a connection that goes quiet for an hour should not narrate it
//! seven hundred times. Samples written to a `file` are a time series rather
//! than a narrative, so those are written every interval, zeroes included.
//! For a live view of a transfer in progress, `--progress` is the better
//! instrument.

use std::{
    fmt,
    fmt::Write as _,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tocat_api::{
    BuildCtx, ChannelId, ChannelTarget, Ctx, LogLevel, Plugin, PluginError, PluginFactory, Result,
    Stage,
};

pub const NAME: &str = "rate";

const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

const BYTE_UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
const BIT_UNITS: &[&str] = &["bit", "Kbit", "Mbit", "Gbit", "Tbit", "Pbit"];

/// What the numbers are counted in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unit {
    /// Binary multiples of bytes: `1.23GiB`, `10.2MiB/s`.
    #[default]
    #[serde(alias = "byte")]
    Bytes,
    /// Decimal multiples of bits, as network equipment is specified:
    /// `9.84Gbit`, `81.6Mbit/s`.
    #[serde(alias = "bit")]
    Bits,
}

impl Unit {
    /// A byte count, in this unit.
    #[must_use]
    pub fn amount(self, bytes: u64) -> String {
        match self {
            Unit::Bytes => scaled(bytes as f64, 1024.0, BYTE_UNITS),
            Unit::Bits => scaled(bytes as f64 * 8.0, 1000.0, BIT_UNITS),
        }
    }

    /// A rate in bytes per second, in this unit.
    #[must_use]
    pub fn rate(self, bytes_per_second: f64) -> String {
        match self {
            Unit::Bytes => format!("{}/s", scaled(bytes_per_second, 1024.0, BYTE_UNITS)),
            Unit::Bits => format!("{}/s", scaled(bytes_per_second * 8.0, 1000.0, BIT_UNITS)),
        }
    }
}

/// Which level the reports are logged at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
}

impl From<ReportLevel> for LogLevel {
    fn from(level: ReportLevel) -> Self {
        match level {
            ReportLevel::Trace => LogLevel::Trace,
            ReportLevel::Debug => LogLevel::Debug,
            ReportLevel::Info => LogLevel::Info,
            ReportLevel::Warn => LogLevel::Warn,
        }
    }
}

/// How often to report.
///
/// Accepts a plain number of seconds or a suffixed string: `10`, `0.5`,
/// `500ms`, `2m`. Both spellings are needed because the command line has no way
/// to write a float: `interval=0.5` arrives as the string `"0.5"` while a
/// config file will more often hold a number.
///
/// Zero disables periodic reporting, leaving only the end-of-stream summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval(pub Duration);

impl Interval {
    /// The reporting period, or `None` when periodic reporting is off.
    #[must_use]
    pub fn period(self) -> Option<Duration> {
        (!self.0.is_zero()).then_some(self.0)
    }
}

impl Default for Interval {
    fn default() -> Self {
        Self(DEFAULT_INTERVAL)
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs_f64();

        if seconds > 0.0 && seconds < 1.0 {
            write!(f, "{}ms", self.0.as_millis())
        } else {
            write!(f, "{seconds}s")
        }
    }
}

impl Serialize for Interval {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // A visitor rather than an untagged enum: untagged reports "did not
        // match any variant" and swallows the reason, and "unknown time suffix
        // \"furlongs\"" is the whole value of parsing it here.
        struct IntervalVisitor;

        impl serde::de::Visitor<'_> for IntervalVisitor {
            type Value = Interval;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number of seconds, or a duration such as \"500ms\" or \"2m\"")
            }

            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Interval, E> {
                parse_interval(value).map(Interval).map_err(E::custom)
            }

            fn visit_f64<E: serde::de::Error>(
                self,
                value: f64,
            ) -> std::result::Result<Interval, E> {
                seconds_to_duration(value).map(Interval).map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(
                self,
                value: u64,
            ) -> std::result::Result<Interval, E> {
                self.visit_f64(value as f64)
            }

            fn visit_i64<E: serde::de::Error>(
                self,
                value: i64,
            ) -> std::result::Result<Interval, E> {
                self.visit_f64(value as f64)
            }
        }

        deserializer.deserialize_any(IntervalVisitor)
    }
}

fn seconds_to_duration(seconds: f64) -> std::result::Result<Duration, String> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("{seconds} is not a valid interval"));
    }

    Ok(Duration::from_secs_f64(seconds))
}

fn parse_interval(raw: &str) -> std::result::Result<Duration, String> {
    let trimmed = raw.trim();
    let digits = trimmed
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .trim_end();
    let suffix = trimmed[digits.len()..].trim().to_ascii_lowercase();

    let value: f64 = digits
        .parse()
        .map_err(|_| format!("{digits:?} is not a number"))?;

    let seconds = match suffix.as_str() {
        "" | "s" | "sec" | "secs" => value,
        "ms" => value / 1000.0,
        "m" | "min" | "mins" => value * 60.0,
        other => return Err(format!("unknown time suffix {other:?}; use ms, s or m")),
    };

    seconds_to_duration(seconds)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RateConfig {
    /// How often to report while bytes are flowing. `0` reports only at end of
    /// stream.
    #[serde(default)]
    pub interval: Interval,

    #[serde(default)]
    pub unit: Unit,

    /// Log a total, average and peak when the stream ends.
    #[serde(default = "default_true")]
    pub summary: bool,

    #[serde(default)]
    pub level: ReportLevel,

    /// Write the periodic samples to a file as CSV rather than logging them.
    /// The summary is logged either way.
    #[serde(default)]
    pub file: Option<PathBuf>,

    /// Append to an existing sample file rather than truncating it.
    #[serde(default = "default_true")]
    pub append: bool,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            interval: Interval::default(),
            unit: Unit::default(),
            summary: true,
            level: ReportLevel::default(),
            file: None,
            append: true,
        }
    }
}

fn default_true() -> bool {
    true
}

impl RateConfig {
    /// Resolve `file` to a channel target, or `None` to report through the log.
    ///
    /// stdout is rejected for the same reason `tee` rejects it: on a stdio
    /// endpoint it carries relay payload.
    pub fn target(&self) -> Result<Option<ChannelTarget>> {
        let Some(path) = &self.file else {
            return Ok(None);
        };

        match path.to_str() {
            Some("-" | "stderr" | "/dev/stderr" | "/dev/fd/2") => Ok(Some(ChannelTarget::Stderr)),
            Some("stdout" | "/dev/stdout" | "/dev/fd/1") => Err(PluginError::config(
                NAME,
                "refusing to write to stdout, it may carry relay payload; use `-` for stderr",
            )),
            _ => Ok(Some(ChannelTarget::File {
                path: path.clone(),
                append: self.append,
            })),
        }
    }
}

pub struct Rate {
    unit: Unit,
    level: LogLevel,
    /// `None` when periodic reporting is off, which is also what keeps the
    /// per-chunk path free of a clock read.
    period: Option<Duration>,
    summary: bool,
    channel: Option<ChannelId>,
    /// This instance's display name. Cached at build time: it cannot change,
    /// and a sample should not allocate one.
    stage: String,
    /// Set by the first chunk, so the elapsed time is the transfer's rather
    /// than the connection's. `None` means nothing ever arrived.
    started: Option<Instant>,
    last: Instant,
    total: u64,
    /// Bytes since the last report.
    window: u64,
    peak: f64,
    /// Whether the last report was of an idle window, so the next one can stay
    /// quiet rather than repeating it.
    stalled: bool,
    scratch: String,
}

impl Rate {
    /// Emit one periodic sample and start a new window.
    fn sample(&mut self, ctx: &mut Ctx<'_>, now: Instant) {
        let started = self.started.unwrap_or(now);
        let window = now.duration_since(self.last).as_secs_f64();
        let rate = if window > 0.0 {
            self.window as f64 / window
        } else {
            0.0
        };

        if rate > self.peak {
            self.peak = rate;
        }

        let idle = self.window == 0;

        match self.channel {
            // A time series wants its zeroes: a gap in a graph reads as
            // missing data rather than as a stall.
            Some(channel) => {
                self.scratch.clear();
                let _ = writeln!(
                    self.scratch,
                    "{},{:.3},{},{},{:.0}",
                    self.stage,
                    now.duration_since(started).as_secs_f64(),
                    self.total,
                    self.window,
                    rate,
                );
                ctx.side_write(channel, self.scratch.as_bytes());
            }
            // A log wants to be told once.
            None if idle && self.stalled => {}
            None if idle => {
                let message = format!(
                    "stalled, nothing in {window:.1}s, {} total",
                    self.unit.amount(self.total),
                );
                ctx.log(self.level, &message);
            }
            None => {
                let message = format!(
                    "{} ({} in {window:.1}s), {} total",
                    self.unit.rate(rate),
                    self.unit.amount(self.window),
                    self.unit.amount(self.total),
                );
                ctx.log(self.level, &message);
            }
        }

        self.stalled = idle;
        self.last = now;
        self.window = 0;
    }
}

impl Plugin for Rate {
    fn name(&self) -> &str {
        NAME
    }

    fn datagram_safe(&self) -> bool {
        // Pure observer: the payload is forwarded untouched
        true
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        // Zero-copy: downstream gets the original slice.
        ctx.pass_through();

        self.total += input.len() as u64;
        self.window += input.len() as u64;

        // First bytes: this is where the clock starts, so idle time waiting for
        // a peer is not averaged into the transfer's rate. Every chunk after it
        // costs two adds and this branch. The reporting is somebody else's
        // problem.
        if self.started.is_none() {
            let now = Instant::now();
            self.started = Some(now);
            self.last = now;
        }

        Ok(())
    }

    fn tick_interval(&self) -> Option<Duration> {
        self.period
    }

    fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        // Ticks run for the pipeline's whole life, but there is nothing to
        // report the rate of until something has arrived. A connection that
        // sits open and idle stays silent.
        if self.started.is_none() {
            return Ok(());
        }

        self.sample(ctx, Instant::now());

        Ok(())
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        // Nothing ever arrived
        let Some(started) = self.started else {
            return Ok(());
        };

        let now = Instant::now();

        // Flush the tail of the sample file so the last window is not lost.
        // The log path gets it in the summary instead.
        if self.channel.is_some() && self.window > 0 {
            self.sample(ctx, now);
        }

        if !self.summary {
            return Ok(());
        }

        let elapsed = now.duration_since(started);
        let seconds = elapsed.as_secs_f64();
        let average = if seconds > 0.0 {
            self.total as f64 / seconds
        } else {
            0.0
        };

        let mut message = format!(
            "transferred {} in {} ({} average",
            self.unit.amount(self.total),
            hms(elapsed),
            self.unit.rate(average),
        );

        // Peak is the fastest window, so it only means anything if there were
        // windows to compare.
        if self.period.is_some() && self.peak > 0.0 {
            let _ = write!(message, ", {} peak", self.unit.rate(self.peak));
        }

        message.push(')');
        ctx.log(self.level, &message);

        Ok(())
    }
}

pub struct RateFactory;

impl PluginFactory for RateFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "measure and report throughput at this point in the pipeline"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: RateConfig = ctx.config()?;

        let channel = match config.target()? {
            Some(target) => Some(ctx.open_channel(target)?),
            None => None,
        };

        Ok(Stage::filter(Rate {
            unit: config.unit,
            level: config.level.into(),
            period: config.interval.period(),
            summary: config.summary,
            channel,
            stage: ctx.stage().name.to_string(),
            started: None,
            last: Instant::now(),
            total: 0,
            window: 0,
            peak: 0.0,
            stalled: false,
            scratch: String::new(),
        }))
    }
}

/// `1.23GiB`-style: three significant figures, then the unit.
fn scaled(value: f64, step: f64, units: &[&str]) -> String {
    let mut value = if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    };
    let mut unit = 0;

    while value >= step && unit + 1 < units.len() {
        value /= step;
        unit += 1;
    }

    let digits = if unit == 0 || value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };

    format!("{value:.digits$}{}", units[unit])
}

/// Elapsed time to `H:MM:SS`.
#[must_use]
pub fn hms(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use std::{thread::sleep, time::Duration};

    use serde_json::json;
    use tocat_api::{
        Direction, EffectSink, Emission, Emit, HostBuilder, PipelineMeta, Result as PluginResult,
        StageInfo,
    };

    use super::*;

    #[derive(Default)]
    struct Recorder {
        writes: Vec<Vec<u8>>,
        logs: Vec<String>,
    }

    impl EffectSink for Recorder {
        fn write(&mut self, _channel: ChannelId, bytes: &[u8]) {
            self.writes.push(bytes.to_vec());
        }

        fn log(&mut self, _level: LogLevel, _stage: &str, message: &str) {
            self.logs.push(message.to_string());
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

        match RateFactory.build(&mut ctx).expect("build") {
            Stage::Filter(plugin) => plugin,
            Stage::External(_) => unreachable!("rate is a filter"),
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
            "rate must never materialise the payload",
        );

        emission.emit()
    }

    fn finish(plugin: &mut dyn Plugin, sink: &mut Recorder) {
        let meta = meta();
        let mut emission = Emission::new();
        let mut ctx = Ctx::new(&meta, NAME, &[], &mut emission, sink);
        plugin.on_eof(&mut ctx).expect("on_eof");
    }

    /// Stands in for the host's timer. The host owns the schedule, so a tick
    /// arriving is the whole of the contract; the stage does not second-guess
    /// whether it was due.
    fn tick(plugin: &mut dyn Plugin, sink: &mut Recorder) {
        let meta = meta();
        let mut emission = Emission::new();
        {
            let mut ctx = Ctx::new(&meta, NAME, &[], &mut emission, sink);
            plugin.on_tick(&mut ctx).expect("on_tick");
        }

        assert!(
            emission.bytes().is_empty(),
            "rate is an observer and emits nothing",
        );
    }

    #[test]
    fn passes_the_payload_through_untouched() {
        let mut rate = build(json!({}));
        let mut sink = Recorder::default();

        assert_eq!(feed(rate.as_mut(), &mut sink, b"ping"), Emit::Passthrough);
        assert!(rate.datagram_safe());
    }

    #[test]
    fn chunks_alone_never_report() {
        let mut rate = build(json!({ "interval": "1ms" }));
        let mut sink = Recorder::default();

        feed(rate.as_mut(), &mut sink, b"first");
        sleep(Duration::from_millis(5));
        feed(rate.as_mut(), &mut sink, b"second");

        assert!(sink.logs.is_empty(), "reporting belongs to the tick");
    }

    #[test]
    fn a_tick_reports_the_window() {
        let mut rate = build(json!({ "interval": "1ms" }));
        let mut sink = Recorder::default();

        feed(rate.as_mut(), &mut sink, &[0u8; 4096]);
        tick(rate.as_mut(), &mut sink);

        assert_eq!(sink.logs.len(), 1);
        assert!(sink.logs[0].contains("4.00KiB in"), "{}", sink.logs[0]);
    }

    /// The point of the hook: without it a stalled stream and a finished one
    /// look the same from inside a plugin.
    #[test]
    fn a_stall_is_reported_once() {
        let mut rate = build(json!({ "interval": "1ms" }));
        let mut sink = Recorder::default();

        feed(rate.as_mut(), &mut sink, b"payload");
        tick(rate.as_mut(), &mut sink);
        tick(rate.as_mut(), &mut sink);
        tick(rate.as_mut(), &mut sink);

        assert_eq!(sink.logs.len(), 2, "one rate line, then one stall line");
        assert!(sink.logs[1].starts_with("stalled"), "{}", sink.logs[1]);

        // Traffic resumes: the next tick has something to say again.
        feed(rate.as_mut(), &mut sink, b"payload");
        tick(rate.as_mut(), &mut sink);

        assert_eq!(sink.logs.len(), 3);
        assert!(!sink.logs[2].starts_with("stalled"), "{}", sink.logs[2]);
    }

    /// An open connection that never carries anything has no rate to report.
    #[test]
    fn ticks_before_the_first_chunk_are_silent() {
        let mut rate = build(json!({ "interval": "1ms" }));
        let mut sink = Recorder::default();

        tick(rate.as_mut(), &mut sink);
        tick(rate.as_mut(), &mut sink);

        assert!(sink.logs.is_empty());
    }

    #[test]
    fn the_configured_interval_is_what_the_host_is_asked_for() {
        assert_eq!(
            build(json!({ "interval": "250ms" })).tick_interval(),
            Some(Duration::from_millis(250)),
        );

        assert_eq!(
            build(json!({ "interval": 0 })).tick_interval(),
            None,
            "interval 0 means summary only, so no timer is built for it",
        );
    }

    #[test]
    fn summary_reports_the_total() {
        let mut rate = build(json!({ "interval": 0 }));
        let mut sink = Recorder::default();

        feed(rate.as_mut(), &mut sink, &[0u8; 2048]);
        finish(rate.as_mut(), &mut sink);

        let summary = sink.logs.last().expect("a summary");
        assert!(summary.contains("2.00KiB"), "unexpected summary: {summary}");
        assert!(
            !summary.contains("peak"),
            "no windows, so no peak: {summary}",
        );
    }

    /// Nothing arrived, so there is nothing to average over: a "0B in 0:00:00"
    /// line on every idle connection would be noise.
    #[test]
    fn silent_when_no_bytes_ever_arrive() {
        let mut rate = build(json!({}));
        let mut sink = Recorder::default();

        finish(rate.as_mut(), &mut sink);
        assert!(sink.logs.is_empty());
    }

    #[test]
    fn samples_go_to_the_channel_when_a_file_is_given() {
        let mut rate = build(json!({ "interval": "1ms", "file": "samples.csv" }));
        let mut sink = Recorder::default();

        feed(rate.as_mut(), &mut sink, b"payload");
        tick(rate.as_mut(), &mut sink);

        assert_eq!(sink.writes.len(), 1, "the sample is a channel write");
        assert!(sink.logs.is_empty(), "and not a log line");

        let row = String::from_utf8(sink.writes[0].clone()).unwrap();
        assert!(row.starts_with("rate,"), "unexpected row: {row}");
        assert_eq!(row.split(',').count(), 5, "five columns: {row}");
    }

    /// A gap in a time series reads as missing data, so idle windows are
    /// written even though the log would have stayed quiet.
    #[test]
    fn a_sample_file_records_idle_windows() {
        let mut rate = build(json!({ "interval": "1ms", "file": "samples.csv" }));
        let mut sink = Recorder::default();

        feed(rate.as_mut(), &mut sink, b"payload");
        tick(rate.as_mut(), &mut sink);
        tick(rate.as_mut(), &mut sink);
        tick(rate.as_mut(), &mut sink);

        assert_eq!(sink.writes.len(), 3);
    }

    #[test]
    fn interval_accepts_numbers_and_suffixes() {
        let cases = [
            (json!({ "interval": 10 }), Duration::from_secs(10)),
            (json!({ "interval": "0.5" }), Duration::from_millis(500)),
            (json!({ "interval": "500ms" }), Duration::from_millis(500)),
            (json!({ "interval": "2m" }), Duration::from_secs(120)),
            (json!({ "interval": 0 }), Duration::ZERO),
        ];

        for (config, expected) in cases {
            let parsed: RateConfig = serde_json::from_value(config.clone()).expect("config");
            assert_eq!(parsed.interval.0, expected, "for {config}");
        }

        assert!(serde_json::from_value::<RateConfig>(json!({ "interval": "5 furlongs" })).is_err());
        assert!(serde_json::from_value::<RateConfig>(json!({ "interval": -1 })).is_err());
    }

    #[test]
    fn units_scale() {
        assert_eq!(Unit::Bytes.amount(512), "512B");
        assert_eq!(Unit::Bytes.amount(2048), "2.00KiB");
        assert_eq!(Unit::Bytes.amount(1024 * 1024 * 1024), "1.00GiB");
        assert_eq!(Unit::Bytes.rate(1_048_576.0), "1.00MiB/s");
        assert_eq!(Unit::Bits.rate(1_250_000.0), "10.0Mbit/s");
    }

    #[test]
    fn elapsed_is_hours_minutes_seconds() {
        assert_eq!(hms(Duration::from_secs(0)), "0:00:00");
        assert_eq!(hms(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn stdout_is_refused() {
        let config = RateConfig {
            file: Some(PathBuf::from("/dev/stdout")),
            ..RateConfig::default()
        };
        assert!(config.target().is_err());
    }
}
