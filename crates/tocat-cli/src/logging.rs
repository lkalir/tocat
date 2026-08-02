//! logging.rs - diagnostic logging.
//!
//! Sinks are declarative: any number of `[[log]]` entries, each with its own
//! format, level and rotation, composed into one subscriber. Rotating file
//! sinks are non-blocking and hand back a [`WorkerGuard`]; [`LoggingGuard`]
//! holds them, and dropping it early silently truncates the log.
//!
//! This is *only* diagnostics. Use the `tocat-plugin-tee` for payload dumping.
//!
//! stderr sinks write through [`LogWriter`] rather than [`std::io::stderr`], so
//! an event erases the progress line before printing instead of landing on top
//! of it. With no progress line on screen the two are the same thing.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    EnvFilter, Layer, Registry, filter::LevelFilter, fmt, fmt::writer::BoxMakeWriter, prelude::*,
};

use crate::progress::LogWriter;

#[derive(Debug, Default, Deserialize, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogRotation {
    Minutely,
    Hourly,
    Daily,
    #[default]
    Never,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Compact,
    Pretty,
    Json,
}

#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Deserialize,
    clap::ValueEnum,
    Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for LevelFilter {
    fn from(value: LogLevel) -> Self {
        match value {
            LogLevel::Off => LevelFilter::OFF,
            LogLevel::Error => LevelFilter::ERROR,
            LogLevel::Warn => LevelFilter::WARN,
            LogLevel::Info => LevelFilter::INFO,
            LogLevel::Debug => LevelFilter::DEBUG,
            LogLevel::Trace => LevelFilter::TRACE,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LogSinkSpec {
    Stderr {
        #[serde(default)]
        format: LogFormat,
        level: Option<LogLevel>,
    },
    File {
        path: PathBuf,
        #[serde(default)]
        format: LogFormat,
        level: Option<LogLevel>,
        #[serde(default)]
        rotation: LogRotation,
        max_files: Option<usize>,
        #[serde(default)]
        truncate: bool,
    },
}

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

pub struct LoggingGuard {
    _guards: Vec<WorkerGuard>,
}

pub fn bootstrap_logging(level: LogLevel) -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(
        tracing_subscriber::registry().with(
            fmt::layer()
                .with_writer(LogWriter)
                .compact()
                .with_filter(filter(level)),
        ),
    )
}

pub fn init_logging(specs: &[LogSinkSpec], level: LogLevel) -> anyhow::Result<LoggingGuard> {
    let mut guards = Vec::new();
    let layers = if specs.is_empty() {
        stderr_sink(level)
    } else {
        specs
            .iter()
            .map(|spec| build_sink(spec, level, &mut guards))
            .collect::<anyhow::Result<Vec<_>>>()?
    };

    #[cfg(feature = "tokio-console")]
    let console_layer = Some(console_subscriber::spawn());
    #[cfg(not(feature = "tokio-console"))]
    let console_layer: Option<tracing_subscriber::layer::Identity> = None;

    tracing_subscriber::registry()
        .with(layers)
        .with(console_layer)
        .init();

    Ok(LoggingGuard { _guards: guards })
}

fn filter(level: LogLevel) -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::from(level).into())
        .from_env_lossy()
}

fn stderr_sink(level: LogLevel) -> Vec<BoxedLayer> {
    vec![
        fmt::layer()
            .with_writer(LogWriter)
            .compact()
            .with_filter(filter(level))
            .boxed(),
    ]
}

fn build_sink(
    spec: &LogSinkSpec,
    default_level: LogLevel,
    guards: &mut Vec<WorkerGuard>,
) -> anyhow::Result<BoxedLayer> {
    let (writer, format, level) = match spec {
        LogSinkSpec::Stderr { format, level } => (
            BoxMakeWriter::new(LogWriter),
            *format,
            level.unwrap_or(default_level),
        ),
        LogSinkSpec::File {
            path,
            format,
            level,
            rotation,
            max_files,
            truncate,
        } => {
            let (nb, guard) = match rotation {
                LogRotation::Never => {
                    let file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(!truncate)
                        .open(path)
                        .with_context(|| format!("opening log file {}", path.display()))?;
                    tracing_appender::non_blocking(file)
                }

                rotation => {
                    let directory = path.parent().unwrap_or_else(|| std::path::Path::new("."));
                    let prefix = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "tocat.log".to_string());
                    let strat = match rotation {
                        LogRotation::Minutely => Rotation::MINUTELY,
                        LogRotation::Hourly => Rotation::HOURLY,
                        LogRotation::Daily => Rotation::DAILY,
                        LogRotation::Never => unreachable!(),
                    };

                    let mut builder = RollingFileAppender::builder()
                        .rotation(strat)
                        .filename_prefix(prefix);

                    if let Some(max_files) = max_files {
                        builder = builder.max_log_files(*max_files);
                    }

                    let appender = builder.build(directory).with_context(|| {
                        format!("creating rolling log appender in {}", directory.display())
                    })?;

                    tracing_appender::non_blocking(appender)
                }
            };

            guards.push(guard);

            (
                BoxMakeWriter::new(nb),
                *format,
                level.unwrap_or(default_level),
            )
        }
    };

    let layer = fmt::layer().with_writer(writer);
    let filter = LevelFilter::from(level);

    Ok(match format {
        LogFormat::Compact => layer.compact().with_filter(filter).boxed(),
        LogFormat::Pretty => layer.pretty().with_filter(filter).boxed(),
        LogFormat::Json => layer.json().with_filter(filter).boxed(),
    })
}
