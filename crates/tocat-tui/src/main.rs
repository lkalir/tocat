//! tocat-tui: build a relay by typing into it, then watch it run.
//!
//! This exists to be a second frontend. Everything it does, it does through
//! `tocat-core` and nothing else: parse two endpoint specs and a plugin chain,
//! build a [`Relay`], run it, read a [`Meter`] while it runs, and ask it to
//! drain. If any of that needed something from the `tocat` binary, the crate
//! split would not have worked, and this would not compile.
//!
//! It is deliberately small. A frontend that reimplemented the relay would
//! prove nothing.
//!
//! # Shape
//!
//! Two screens and one loop. [`Screen::Build`] collects three strings;
//! [`Screen::Run`] shows what the meter says about the relay those strings
//! produced. The relay runs as a task, the loop polls for a key with a timeout,
//! and the timeout is also the redraw tick: nothing needs waking when a
//! counter moves, because the counter is read rather than pushed.

use std::{sync::Arc, time::Duration};

use ratatui::{
    DefaultTerminal,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Gauge, Paragraph},
};
use tocat_core::{
    endpoint::EndpointSpec,
    progress::Meter,
    relay::Relay,
    shutdown::{self, Trigger},
    spec::parse_plugin_spec,
};

/// How long to wait for a key before redrawing anyway.
///
/// Also the refresh rate, since the counters are polled rather than pushed. Ten
/// a second is smooth enough to look live and slow enough that the relay is not
/// competing with the display for the runtime.
const TICK: Duration = Duration::from_millis(100);

/// The copy buffer, which the command line spells `-b`.
const BUFFER: usize = 256 * 1024;

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let terminal = ratatui::init();
    let result = App::default().run(terminal, &runtime);

    // Before the error is printed, or it is printed into a screen that is about
    // to be torn down.
    ratatui::restore();

    result
}

/// Which of the three fields the keyboard is going into.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Field {
    #[default]
    Source,
    Plugins,
    Sink,
}

impl Field {
    fn next(self) -> Self {
        match self {
            Field::Source => Field::Plugins,
            Field::Plugins => Field::Sink,
            Field::Sink => Field::Source,
        }
    }
}

enum Screen {
    Build,
    /// The relay is running, or it has finished and left a result behind.
    Run {
        meter: Arc<Meter>,
        /// Dropping this drains the relay, which is why it is held here rather
        /// than passed to the task: the screen owning the trigger owns the
        /// relay's lifetime.
        trigger: Trigger,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
        outcome: Option<String>,
    },
}

struct App {
    screen: Screen,
    field: Field,
    source: String,
    plugins: String,
    sink: String,
    /// Whatever went wrong last, shown under the form. A build failure is the
    /// normal way to learn an endpoint spelling is wrong, so it belongs on the
    /// screen rather than in a log nobody is reading.
    error: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            screen: Screen::Build,
            field: Field::default(),
            source: "tcp-listen:127.0.0.1:9000,fork".to_owned(),
            plugins: String::new(),
            sink: "tcp:example.com:80".to_owned(),
            error: None,
        }
    }
}

impl App {
    fn run(
        mut self,
        mut terminal: DefaultTerminal,
        runtime: &tokio::runtime::Runtime,
    ) -> anyhow::Result<()> {
        loop {
            self.reap();
            terminal.draw(|frame| self.draw(frame))?;

            if !event::poll(TICK)? {
                continue;
            }

            let Event::Key(key) = event::read()? else {
                continue;
            };

            // Windows sends a release for every press; without this every
            // keystroke would arrive twice.
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match (&self.screen, key.code) {
                (_, KeyCode::Esc) => return Ok(()),

                (Screen::Build, KeyCode::Enter) => self.start(runtime),
                (Screen::Build, KeyCode::Tab) => self.field = self.field.next(),
                (Screen::Build, KeyCode::Backspace) => {
                    self.field_mut().pop();
                }
                (Screen::Build, KeyCode::Char(c)) => self.field_mut().push(c),

                (Screen::Run { .. }, KeyCode::Char('q')) => self.stop(),
                _ => {}
            }
        }
    }

    fn field_mut(&mut self) -> &mut String {
        match self.field {
            Field::Source => &mut self.source,
            Field::Plugins => &mut self.plugins,
            Field::Sink => &mut self.sink,
        }
    }

    /// Build the relay and hand it to the runtime.
    ///
    /// Everything that can go wrong goes wrong here rather than later: an
    /// endpoint that does not parse, a plugin that does not exist, a chain
    /// whose boundaries do not line up. That is the relay's design showing
    /// through, and it is what lets this screen report a mistake before
    /// anything has been opened.
    fn start(&mut self, runtime: &tokio::runtime::Runtime) {
        self.error = None;

        let build = || -> anyhow::Result<(Arc<Meter>, Trigger, tokio::task::JoinHandle<anyhow::Result<()>>)> {
            let source: EndpointSpec = self.source.trim().parse()?;
            let sink: EndpointSpec = self.sink.trim().parse()?;

            let plugins = self
                .plugins
                .split_whitespace()
                .map(parse_plugin_spec)
                .collect::<Result<Vec<_>, _>>()?;

            let meter = Arc::new(Meter::new(None));
            let (trigger, shutdown) = shutdown::channel();

            let relay = runtime.block_on(Relay::new(
                source,
                sink,
                plugins,
                tocat_plugins::native_registry(),
                BUFFER,
                Some(meter.clone()),
            ))?;

            let handle = runtime.spawn(relay.run(shutdown));

            Ok((meter, trigger, handle))
        };

        match build() {
            Ok((meter, trigger, handle)) => {
                self.screen = Screen::Run {
                    meter,
                    trigger,
                    handle,
                    outcome: None,
                };
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Ask the relay to drain, and stay on the screen so its outcome can be
    /// read.
    fn stop(&mut self) {
        if let Screen::Run { trigger, .. } = &self.screen {
            trigger.drain();
        }
    }

    /// Collect the relay's result once it has one.
    ///
    /// Polled rather than awaited, because this loop is not async: the point of
    /// a handle here is that finishing is something the screen notices rather
    /// than waits for.
    fn reap(&mut self) {
        let Screen::Run {
            handle, outcome, ..
        } = &mut self.screen
        else {
            return;
        };

        if outcome.is_some() || !handle.is_finished() {
            return;
        }

        *outcome = Some(match futures::executor::block_on(handle) {
            Ok(Ok(())) => "finished".to_owned(),
            Ok(Err(e)) => format!("{e:#}"),
            Err(e) => format!("the relay task did not finish: {e}"),
        });
    }

    fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        match &self.screen {
            Screen::Build => self.draw_build(frame),
            Screen::Run { meter, outcome, .. } => draw_run(frame, meter, outcome.as_deref()),
        }
    }

    fn draw_build(&self, frame: &mut ratatui::Frame<'_>) {
        let [head, source, plugins, sink, error, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        frame.render_widget(Paragraph::new("tocat".bold()).centered(), head);

        for (area, label, value, field) in [
            (source, "source", &self.source, Field::Source),
            (plugins, "plugins", &self.plugins, Field::Plugins),
            (sink, "sink", &self.sink, Field::Sink),
        ] {
            let block = Block::bordered().title(label);

            let block = if field == self.field {
                block.border_style(Style::new().add_modifier(Modifier::BOLD))
            } else {
                block
            };

            frame.render_widget(Paragraph::new(value.as_str()).block(block), area);
        }

        if let Some(message) = &self.error {
            frame.render_widget(
                Paragraph::new(message.as_str())
                    .red()
                    .block(Block::bordered().title("cannot start")),
                error,
            );
        }

        frame.render_widget(
            Paragraph::new("tab: next field   enter: start   esc: quit").dim(),
            help,
        );
    }
}

fn draw_run(frame: &mut ratatui::Frame<'_>, meter: &Meter, outcome: Option<&str>) {
    let [head, counts, gauge, status, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let (forward, reverse) = meter.read();
    let elapsed = meter.started().elapsed();

    frame.render_widget(Paragraph::new("running".bold()).centered(), head);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::from(format!("{} out", bytes(forward))).bold(),
            Span::from("   "),
            Span::from(format!("{} in", bytes(reverse))).bold(),
            Span::from(format!("   {} connections", meter.connections())),
            Span::from(format!("   {:.0}s", elapsed.as_secs_f64())),
        ]))
        .block(Block::bordered().title("transferred")),
        counts,
    );

    // Only meaningful when the forward total was knowable, which for a socket
    // it is not. Shown as motion rather than progress in that case: the ratio
    // is bytes against a rolling ceiling, so it says "something is happening"
    // and nothing more.
    let expected = meter.expected().unwrap_or(0);
    let ratio = if expected > 0 {
        (forward as f64 / expected as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    frame.render_widget(
        Gauge::default()
            .block(Block::bordered().title("progress"))
            .ratio(ratio),
        gauge,
    );

    if let Some(outcome) = outcome {
        frame.render_widget(
            Paragraph::new(outcome).block(Block::bordered().title("outcome")),
            status,
        );
    }

    frame.render_widget(Paragraph::new("q: drain and stop   esc: quit").dim(), help);
}

/// Enough of a human readable size for a display that is not the point of the
/// exercise. The binary's own formatter is not reachable from here, which is
/// the split working as intended rather than a gap.
fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = count as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
