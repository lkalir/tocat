//! cli.rs

use std::path::PathBuf;

use clap::Parser;

use crate::logging::LogLevel;

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
        short = 'f',
        long = "from",
        value_name = "ADDR",
        conflicts_with = "source_pos",
        help = "Source endpoint."
    )]
    pub source: Option<String>,

    #[arg(
        short = 't',
        long = "to",
        value_name = "ADDR",
        conflicts_with = "sink_pos",
        help = "Sink endpoint."
    )]
    pub sink: Option<String>,

    #[arg(short, long, action = clap::ArgAction::Count, conflicts_with = "log_level", help = "Simple verbosity level.")]
    pub verbose: u8,

    #[arg(
        long,
        value_enum,
        value_name = "LEVEL",
        help = "Explicit verbosity level."
    )]
    pub log_level: Option<LogLevel>,

    #[arg(value_name = "SOURCE", help = "Source endpoint.")]
    pub source_pos: Option<String>,

    #[arg(value_name = "SINK", help = "Sink endpoint.")]
    pub sink_pos: Option<String>,
}

impl Cli {
    pub fn verbose_level(&self) -> Option<LogLevel> {
        match self.verbose {
            0 => None,
            1 => Some(LogLevel::Debug),
            _ => Some(LogLevel::Trace),
        }
    }
}
