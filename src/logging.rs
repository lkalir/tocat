use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncWriteExt, BufWriter, Stderr},
    sync::Mutex,
};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    EnvFilter, Layer, Registry, filter::LevelFilter, fmt, fmt::writer::BoxMakeWriter, prelude::*,
};

use crate::endpoint::{DumpConfig, DumpFormat};

fn stderr_dump() -> &'static Mutex<Stderr> {
    static STDERR: OnceLock<Mutex<Stderr>> = OnceLock::new();
    STDERR.get_or_init(|| Mutex::new(tokio::io::stderr()))
}

pub fn format_hex_dump(buf: &[u8], start_offset: u64) -> String {
    buf.chunks(16)
        .enumerate()
        .map(|(offset, chunk)| {
            let hex = chunk
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");

            let ascii: String = chunk
                .iter()
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();

            format!(
                "{:08x}  {hex:<47}  |{ascii}|",
                start_offset + (offset * 16) as u64
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub type SharedDumpFile = Arc<Mutex<BufWriter<tokio::fs::File>>>;

enum DumpSink {
    Disabled,
    Stderr,
    File(SharedDumpFile),
}

pub struct DumpLogger {
    label: String,
    format: DumpFormat,
    sink: DumpSink,
    offset: u64,
}

impl DumpLogger {
    pub async fn new_shared(
        label: impl Into<String>,
        config: Option<DumpConfig>,
        existing_file: Option<SharedDumpFile>,
    ) -> anyhow::Result<(Self, Option<SharedDumpFile>)> {
        let label = label.into();

        let Some(config) = config else {
            return Ok((
                Self {
                    label,
                    format: DumpFormat::RawBinary,
                    sink: DumpSink::Disabled,
                    offset: 0,
                },
                None,
            ));
        };

        let format = config.format.unwrap_or(DumpFormat::RawBinary);
        let (sink, handle) = match (existing_file, config.file.as_ref()) {
            (None, Some(path)) => {
                let f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .await
                    .with_context(|| format!("opening dump file {}", path.display()))?;
                let shared = Arc::new(Mutex::new(BufWriter::with_capacity(256 * 1024, f)));
                (DumpSink::File(shared.clone()), Some(shared))
            }
            (None, None) => (DumpSink::Stderr, None),
            (Some(f), _) => (DumpSink::File(f.clone()), Some(f)),
        };

        Ok((
            Self {
                label,
                format,
                sink,
                offset: 0,
            },
            handle,
        ))
    }

    pub async fn log_bytes(&mut self, buf: &[u8]) -> anyhow::Result<()> {
        if buf.is_empty() || matches!(self.sink, DumpSink::Disabled) {
            return Ok(());
        }

        let entry: Vec<u8> = match self.format {
            DumpFormat::Hex => format!(
                "[{}] {} bytes @ {:#x}\n{}\n",
                self.label,
                buf.len(),
                self.offset,
                format_hex_dump(buf, self.offset)
            )
            .into_bytes(),
            DumpFormat::RawBinary => buf.to_vec(),
        };

        self.offset += buf.len() as u64;

        match &self.sink {
            DumpSink::Disabled => unreachable!(),
            DumpSink::Stderr => {
                let mut out = stderr_dump().lock().await;
                out.write_all(&entry).await?;
                out.flush().await?;
            }
            DumpSink::File(f) => {
                f.lock().await.write_all(&entry).await?;
            }
        }

        Ok(())
    }

    pub async fn flush(&mut self) -> anyhow::Result<()> {
        if let DumpSink::File(f) = &self.sink {
            f.lock().await.flush().await?;
        }

        Ok(())
    }
}

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
                .with_writer(std::io::stderr)
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
            .with_writer(std::io::stderr)
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
            BoxMakeWriter::new(std::io::stderr),
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
