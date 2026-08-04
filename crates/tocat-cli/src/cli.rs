//! cli.rs: argument surface.
//!
//! Endpoints and plugins are written with the same `name:body,key=value`
//! grammar, so roles come from **position**, never from inspecting the text:
//! the outer positionals fill whichever endpoint slots `--from`/`--to` left
//! open, and whatever remains in the middle is the pipeline, in order. See
//! [`Cli::layout`]. Guessing whether `tee,format=hex` "looks like" a plugin
//! would turn a typo into a different program.
//!
//! This module only splits and shapes the arguments; parsing a plugin spec into
//! a [`tocat_api::PluginSpec`] belongs to `config`, which is also where the
//! precedence rules against the config file live.

use std::path::PathBuf;

use clap::Parser;

use crate::{config::ByteSize, logging::LogLevel, progress::ProgressMode};

#[derive(Debug, Parser)]
#[command(version, about = "socat-inspired relay")]
pub struct Cli {
    #[arg(
        short,
        long,
        value_name = "PATH",
        conflicts_with = "no_config",
        help = "Configuration file to use."
    )]
    pub config: Option<PathBuf>,

    #[arg(long, help = "Disable configuration file merging.")]
    pub no_config: bool,

    #[arg(long, help = "Render the final configuration as TOML.")]
    pub dump_config: bool,

    #[arg(
        short = 'b',
        long,
        value_name = "SIZE",
        help = "Bytes per copy, e.g. 65536 or 256KiB. One buffer per direction per connection."
    )]
    pub buffer_size: Option<ByteSize>,

    #[arg(
        short = 'f',
        long = "from",
        value_name = "ADDR",
        help = "Source endpoint. Fills the first positional slot."
    )]
    pub source: Option<String>,

    #[arg(
        short = 't',
        long = "to",
        value_name = "ADDR",
        help = "Sink endpoint. Fills the last positional slot."
    )]
    pub sink: Option<String>,

    #[arg(
        short = 'p',
        long = "plugin",
        value_name = "SPEC",
        help = "Pipeline entry: NAME[:DIRECTION][,key=value...]. Repeatable, applied in order."
    )]
    pub plugins: Vec<String>,

    #[arg(
        long,
        help = "Ignore plugins declared in the configuration file.",
        conflicts_with = "no_config"
    )]
    pub no_plugins: bool,

    #[arg(long, help = "List the plugins compiled into this binary and exit.")]
    pub list_plugins: bool,

    #[arg(
        short = 'P',
        long,
        value_name = "WHEN",
        value_enum,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "auto",
        help = "Draw a progress line on stderr. Bare, or 'auto', draws \
                only when stderr is a terminal; 'always' draws regardless."
    )]
    pub progress: Option<ProgressMode>,

    #[arg(short, long, action = clap::ArgAction::Count, conflicts_with = "log_level", help = "Simple verbosity level.")]
    pub verbose: u8,

    #[arg(
        long,
        value_enum,
        value_name = "LEVEL",
        help = "Explicit verbosity level."
    )]
    pub log_level: Option<LogLevel>,

    #[arg(
        value_name = "SPEC",
        help = "SOURCE [PLUGIN ...] SINK. The outer specs are endpoints; \
                anything between them is a pipeline entry. Slots already \
                filled by --from/--to are skipped."
    )]
    pub positional: Vec<String>,
}

/// How the positional arguments were assigned to roles.
#[derive(Debug, Default)]
pub struct Layout {
    pub source: Option<String>,
    pub sink: Option<String>,
    pub plugins: Vec<String>,
}

impl Cli {
    /// Split the positionals into endpoints and pipeline entries.
    ///
    /// Roles come from position, never from inspecting the spec: an endpoint
    /// and a plugin are written the same way, and guessing between them would
    /// turn a typo into a different program. The endpoint slots are filled
    /// outermost-first from whatever `--from`/`--to` left open, so:
    ///
    /// ```text
    /// tocat SRC SINK                    -> no plugins
    /// tocat SRC tee compress SINK       -> two entries, in that order
    /// tocat -f SRC tee SINK             -> one entry ("SINK" fills the open slot)
    /// tocat -f SRC -t SINK tee          -> one entry (both slots already filled)
    /// ```
    ///
    /// A lone positional with both slots open is the source, matching the
    /// older two-positional form. If the endpoints come from a config file and
    /// you want to add one stage, use `-p` rather than a bare positional.
    pub fn layout(&self) -> Layout {
        let mut rest: Vec<String> = self.positional.clone();
        let mut source = self.source.clone();
        let mut sink = self.sink.clone();

        if source.is_none() && !rest.is_empty() {
            source = Some(rest.remove(0));
        }

        if sink.is_none() && !rest.is_empty() {
            sink = rest.pop();
        }

        Layout {
            source,
            sink,
            plugins: rest,
        }
    }

    pub fn verbose_level(&self) -> Option<LogLevel> {
        match self.verbose {
            0 => None,
            1 => Some(LogLevel::Debug),
            _ => Some(LogLevel::Trace),
        }
    }
}
