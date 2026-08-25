//! progress.rs: a `pv`-style progress line on stderr.
//!
//! Three pieces. [`Meter`] is the shared count: two atomics, one per path,
//! plus the clock and (when it is knowable) the expected total. [`Counted`] is
//! the adapter that feeds it, wrapped around a read half so the count happens
//! inside `poll_read`, measuring at the endpoint, before any stage sees the
//! bytes, which is what `pv` reports and what the `rate` plugin deliberately
//! does not. [`Painter`] redraws the line on a timer.
//!
//! The timer is the reason this is a host feature rather than a plugin. A
//! plugin runs when a chunk arrives, so a stalled stream is indistinguishable
//! from a finished one and an elapsed-time display would freeze. The painter
//! ticks whether or not bytes are moving, which is exactly what you want when
//! watching a transfer that might have wedged.
//!
//! # Sharing stderr
//!
//! The line is drawn with a carriage return and no newline, so anything else
//! writing to stderr lands on top of it. Log output goes through [`LogWriter`],
//! which erases the line before each event and lets the next tick redraw it,
//! and both take the same `std::io::stderr` lock, so a frame and a log line
//! cannot interleave. Everything else on that stream, notably a `tee` pointed
//! at stderr, is outside that arrangement, which is why [`Relay`] warns when
//! the two are used together.
//!
//! No ANSI: erasing is a carriage return, spaces, and another carriage return.
//! `--progress=always` therefore produces something readable when stderr is
//! redirected to a file, in the same way `pv --force` does.
//!
//! [`Relay`]: crate::relay::Relay

use std::{
    fmt::Write as _,
    io::{IsTerminal, Write as _},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tocat_core::{
    endpoint::{EndpointSpec, Transport},
    progress::Meter,
};
use tokio::{sync::oneshot, task::JoinHandle, time::MissedTickBehavior};
use tracing_subscriber::fmt::MakeWriter;

/// How often the line is redrawn.
const REDRAW: Duration = Duration::from_millis(100);

/// Weight of the newest sample in the displayed rate. At [`REDRAW`] this gives
/// a time constant of roughly a second: responsive enough to see a stall,
/// steady enough to read.
const SMOOTHING: f64 = 0.25;

/// Used when the terminal will not say how wide it is.
const FALLBACK_WIDTH: usize = 80;

/// Below this a bar conveys nothing, so the space goes to the numbers instead.
const MIN_BAR: usize = 10;

/// 99:59:59. Anything longer is not an estimate, it is a shrug.
const MAX_ETA: f64 = 359_999.0;

const BYTE_UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/// Width of the progress line currently on screen, or 0 if there is none.
///
/// Global because the log writer has to know about a line drawn by a task it
/// has no handle on. It is the whole of the shared state: one usize, written
/// by whoever last touched the terminal.
static ON_SCREEN: AtomicUsize = AtomicUsize::new(0);

/// A running progress display.
pub struct Progress {
    meter: Arc<Meter>,
    stop: oneshot::Sender<()>,
    painter: JoinHandle<()>,
}

/// When to draw the progress line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ProgressMode {
    #[default]
    Never,
    /// Draw only when stderr is a terminal.
    Auto,
    /// Draw regardless, as `pv --force` does.
    Always,
}

impl Progress {
    #[must_use]
    pub fn meter(&self) -> Arc<Meter> {
        Arc::clone(&self.meter)
    }

    /// Stop redrawing, then leave a final summary line behind.
    ///
    /// Awaits the painter rather than aborting it, so the last thing written to
    /// the terminal is this line and not a half-drawn frame.
    pub async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.painter.await;

        let (forward, reverse) = self.meter.read();
        let moved = forward + reverse;
        let mut out = std::io::stderr().lock();
        let _ = erase(&mut out);

        // Nothing moved: the relay failed to start, or had nothing to do.
        // Either way a line of zeroes is not worth the row.
        if moved > 0 {
            let elapsed = self.meter.started().elapsed();
            let seconds = elapsed.as_secs_f64();
            let average = if seconds > 0.0 {
                moved as f64 / seconds
            } else {
                0.0
            };

            let _ = writeln!(
                out,
                "{} in {} ({}/s)",
                transferred(forward, reverse).trim_start(),
                hms(elapsed),
                bytes(average),
            );
        }

        let _ = out.flush();
    }
}

/// Start the display, if this mode and this terminal call for one.
#[must_use]
pub fn start(mode: ProgressMode, source: &EndpointSpec, sink: &EndpointSpec) -> Option<Progress> {
    let enabled = match mode {
        ProgressMode::Never => false,
        ProgressMode::Auto => std::io::stderr().is_terminal(),
        ProgressMode::Always => true,
    };

    if !enabled {
        return None;
    }

    let meter = Arc::new(Meter::new(expected_size(source, sink)));

    let (stop, halt) = oneshot::channel();
    let painter = tokio::spawn(paint(Arc::clone(&meter), halt));

    Some(Progress {
        meter,
        stop,
        painter,
    })
}

/// Bytes the forward path is expected to carry, when that is knowable.
///
/// Only a regular file has an answer. Under `fork` there is no answer even
/// then: the display aggregates every connection, and a percentage of one
/// connection's total against that sum would be nonsense.
fn expected_size(source: &EndpointSpec, sink: &EndpointSpec) -> Option<u64> {
    if source.is_fork() || sink.is_fork() {
        return None;
    }

    match &source.transport {
        Transport::File(e) => {
            let metadata = std::fs::metadata(&e.path).ok()?;
            metadata.is_file().then_some(metadata.len())
        }
        _ => None,
    }
}

async fn paint(meter: Arc<Meter>, mut halt: oneshot::Receiver<()>) {
    let mut painter = Painter::new(meter);
    let mut ticker = tokio::time::interval(REDRAW);

    // Falling behind should not produce a burst of catch-up frames; the
    // display only ever shows the present.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = &mut halt => break,
            _ = ticker.tick() => painter.draw(),
        }
    }
}

struct Painter {
    meter: Arc<Meter>,
    /// Smoothed rate, in bytes per second.
    rate: f64,
    /// The previous sample: when it was taken, and the count at the time.
    last: (Instant, u64),
    /// Reused so a frame does not allocate. ASCII only, which is what makes
    /// truncating it to the terminal width safe.
    line: String,
    seen: bool,
}

impl Painter {
    fn new(meter: Arc<Meter>) -> Self {
        let started = meter.started();

        Self {
            meter,
            rate: 0.0,
            last: (started, 0),
            line: String::new(),
            seen: false,
        }
    }

    fn draw(&mut self) {
        let now = Instant::now();
        let (forward, reverse) = self.meter.read();
        let moved = forward + reverse;
        let window = now.duration_since(self.last.0).as_secs_f64();

        if window > 0.0 {
            let instant = moved.saturating_sub(self.last.1) as f64 / window;

            // The first sample carrying bytes has nothing to smooth against,
            // so it stands as it is rather than climbing out of zero across
            // the first second of an already-running transfer.
            self.rate = if self.seen {
                self.rate * (1.0 - SMOOTHING) + instant * SMOOTHING
            } else {
                instant
            };

            self.seen = moved > 0;
            self.last = (now, moved);
        }

        let width = terminal_width();
        self.compose(forward, reverse, now, width);
        self.line.truncate(width);

        let mut out = std::io::stderr().lock();
        let previous = ON_SCREEN.swap(self.line.len(), Ordering::AcqRel);
        let padding = previous.saturating_sub(self.line.len());

        // Overwrite in place, then blank whatever the last frame left beyond
        // the end of this one.
        let _ = write!(out, "\r{}{:padding$}", self.line, "");
        let _ = out.flush();
    }

    fn compose(&mut self, forward: u64, reverse: u64, now: Instant, width: usize) {
        let elapsed = now.duration_since(self.meter.started());

        self.line.clear();
        let _ = write!(
            self.line,
            "{} {} [{:>9}/s]",
            transferred(forward, reverse),
            hms(elapsed),
            bytes(self.rate),
        );

        let connections = self.meter.connections();
        if connections > 1 {
            let _ = write!(self.line, " {connections} conns");
        }

        // A bar needs somewhere to be going.
        let Some(expected) = self.meter.expected().filter(|expected| *expected > 0) else {
            return;
        };

        let done = forward.min(expected);
        let fraction = done as f64 / expected as f64;
        let tail = format!(
            " {:>3.0}% ETA {}",
            fraction * 100.0,
            eta(expected - done, self.rate),
        );

        // ` [` + `]` around the bar itself.
        let room = width.saturating_sub(self.line.len() + tail.len() + 3);
        if room >= MIN_BAR {
            self.line.push(' ');
            bar(&mut self.line, fraction, room);
        }

        self.line.push_str(&tail);
    }
}

/// Erase the progress line, if one is on screen.
fn erase(out: &mut impl std::io::Write) -> std::io::Result<()> {
    let width = ON_SCREEN.swap(0, Ordering::AcqRel);

    if width > 0 {
        write!(out, "\r{:width$}\r", "")?;
    }

    Ok(())
}

/// stderr for the log sinks, aware of the progress line.
///
/// Wrapping the writer rather than teaching the painter about logging is what
/// keeps the two independent: with no progress line on screen this is
/// `std::io::stderr` with one relaxed load in front of it.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Held across both writes, and taken by the painter too, so an event
        // cannot land in the middle of a frame.
        let mut out = std::io::stderr().lock();
        erase(&mut out)?;
        out.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().lock().flush()
    }
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

/// `1.23GiB out  340MiB in`, or just the one figure when nothing has come back.
fn transferred(forward: u64, reverse: u64) -> String {
    if reverse == 0 {
        format!("{:>9}", bytes(forward as f64))
    } else {
        format!(
            "{:>9} out {:>9} in",
            bytes(forward as f64),
            bytes(reverse as f64)
        )
    }
}

fn bar(out: &mut String, fraction: f64, width: usize) {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);

    out.push('[');
    for cell in 0..width {
        out.push(if cell + 1 < filled || filled == width {
            '='
        } else if cell < filled {
            '>'
        } else {
            ' '
        });
    }
    out.push(']');
}

fn eta(remaining: u64, rate: f64) -> String {
    // Under a byte a second the estimate is meaningless and the arithmetic is
    // one division away from an overflowing `Duration`.
    if rate <= 1.0 {
        return "--:--:--".to_string();
    }

    let seconds = remaining as f64 / rate;

    if !seconds.is_finite() || seconds > MAX_ETA {
        return "--:--:--".to_string();
    }

    hms(Duration::from_secs_f64(seconds))
}

/// `1.23GiB`-style: three significant figures, then the unit.
fn bytes(value: f64) -> String {
    let mut value = if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    };
    let mut unit = 0;

    while value >= 1024.0 && unit + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    let digits = if unit == 0 || value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };

    format!("{value:.digits$}{}", BYTE_UNITS[unit])
}

/// `H:MM:SS`, as `pv` prints elapsed time.
fn hms(duration: Duration) -> String {
    let seconds = duration.as_secs();

    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

#[cfg(unix)]
fn terminal_width() -> usize {
    match rustix::termios::tcgetwinsize(std::io::stderr()) {
        Ok(size) if size.ws_col > 0 => usize::from(size.ws_col),
        // Not a terminal, or a terminal that will not say how wide it is: 80 is the conventional
        // answer and the line degrades to it cleanly
        _ => FALLBACK_WIDTH,
    }
}

#[cfg(not(unix))]
fn terminal_width() -> usize {
    FALLBACK_WIDTH
}

#[cfg(test)]
mod tests {
    use tocat_api::Direction;

    use super::*;

    fn meter(expected: Option<u64>) -> Arc<Meter> {
        Arc::new(Meter::new(expected))
    }

    fn line(meter: &Arc<Meter>, rate: f64, width: usize) -> String {
        let mut painter = Painter::new(Arc::clone(meter));
        painter.rate = rate;
        painter.compose(
            meter.load_forward_count(),
            meter.load_reverse_count(),
            Instant::now(),
            width,
        );

        painter.line.clone()
    }

    #[test]
    fn scales_byte_counts() {
        assert_eq!(bytes(0.0), "0B");
        assert_eq!(bytes(512.0), "512B");
        assert_eq!(bytes(2048.0), "2.00KiB");
        assert_eq!(bytes(1_073_741_824.0), "1.00GiB");
        assert_eq!(bytes(f64::INFINITY), "0B");
    }

    #[test]
    fn one_direction_shows_one_figure() {
        let meter = meter(None);
        meter.counter(Direction::SourceToSink).add(2048);

        let line = line(&meter, 1024.0, 80);
        assert!(line.contains("2.00KiB"), "{line}");
        assert!(!line.contains(" in"), "nothing came back: {line}");
    }

    #[test]
    fn both_directions_are_labelled() {
        let meter = meter(None);
        meter.counter(Direction::SourceToSink).add(2048);
        meter.counter(Direction::SinkToSource).add(1024);

        let line = line(&meter, 1024.0, 80);
        assert!(line.contains("2.00KiB out"), "{line}");
        assert!(line.contains("1.00KiB in"), "{line}");
    }

    #[test]
    fn a_known_total_adds_a_bar_and_an_eta() {
        let meter = meter(Some(1000));
        meter.counter(Direction::SourceToSink).add(500);

        let line = line(&meter, 100.0, 100);
        assert!(line.contains(" 50%"), "{line}");
        assert!(line.contains("ETA 0:00:05"), "{line}");
        assert!(line.contains('='), "expected a bar: {line}");
    }

    /// The bar is what gives way when the terminal is narrow: the counts and
    /// the rate are the part you cannot infer from looking at it.
    #[test]
    fn a_narrow_terminal_drops_the_bar_not_the_numbers() {
        let meter = meter(Some(1000));
        meter.counter(Direction::SourceToSink).add(500);

        let line = line(&meter, 100.0, 44);
        assert!(!line.contains('='), "no room for a bar: {line}");
        assert!(line.contains(" 50%"), "{line}");
        assert!(line.contains("100B/s"), "the rate survives: {line}");
    }

    #[test]
    fn an_unknown_total_has_no_bar() {
        let meter = meter(None);
        meter.counter(Direction::SourceToSink).add(500);

        let line = line(&meter, 100.0, 100);
        assert!(!line.contains('%'), "{line}");
        assert!(!line.contains("ETA"), "{line}");
    }

    #[test]
    fn a_stalled_transfer_has_no_estimate() {
        assert_eq!(eta(1000, 0.0), "--:--:--");
        assert_eq!(eta(u64::MAX, 2.0), "--:--:--");
        assert_eq!(eta(1000, 100.0), "0:00:10");
    }

    #[test]
    fn the_bar_fills() {
        let mut out = String::new();
        bar(&mut out, 0.0, 4);
        assert_eq!(out, "[    ]");

        out.clear();
        bar(&mut out, 0.5, 4);
        assert_eq!(out, "[=>  ]");

        out.clear();
        bar(&mut out, 1.0, 4);
        assert_eq!(out, "[====]");
    }

    /// Frames are truncated to the terminal width, so anything non-ASCII in
    /// one would be a panic waiting for a narrow window.
    #[test]
    fn frames_are_ascii() {
        let meter = meter(Some(4096));
        meter.counter(Direction::SourceToSink).add(2048);
        meter.counter(Direction::SinkToSource).add(500);

        let line = line(&meter, 1024.0, 100);
        assert!(line.is_ascii(), "{line}");
    }
}
