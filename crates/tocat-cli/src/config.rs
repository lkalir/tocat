//! config.rs - the config file, the CLI, and the merge between them.
//!
//! Precedence is CLI over file, per field: an endpoint given on the command
//! line replaces the file's, and `--no-plugins` drops the file's pipeline
//! entirely. Plugins are the exception to "replace": they accumulate, in the
//! order file, then inline positionals, then `-p`, so a standing `tee` in
//! `tocat.toml` can be extended with an ad-hoc stage without rewriting it.
//!
//! [`parse_plugin_spec`] handles the CLI's compact plugin grammar. Two keys are
//! read by the host and never reach the plugin: `as` names the instance, and
//! `detach` overrides its placement. Everything else is passed through
//! untouched as JSON for the plugin's own `Deserialize` impl, which is what
//! lets the host stay ignorant of plugin schemas (including, eventually,
//! those of WASM plugins it has never seen).

use std::{fs, path::PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tocat_api::{DirectionSpec, PluginSpec, normalize};

use crate::{
    cli::Cli,
    endpoint::{Endpoint, EndpointSpec},
    logging::{LogLevel, LogSinkSpec},
    progress::ProgressMode,
};

const CONFIG_NAMES: &[&str] = &["tocat.toml", ".tocat.toml"];

/// Copy buffer size, in bytes.
///
/// Defined in `tocat-api` so that the host and the plugins share one size
/// grammar, and re-exported here because this is where the config lives.
pub use tocat_api::ByteSize;

pub const DEFAULT_BUFFER: ByteSize = ByteSize(256 * 1024);

#[derive(Debug)]
pub struct Settings {
    pub source: EndpointSpec,
    pub sink: EndpointSpec,
    pub plugins: Vec<PluginSpec>,
    pub buffer: usize,
    pub progress: ProgressMode,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub source: Option<Endpoint>,
    pub sink: Option<Endpoint>,
    #[serde(rename = "log-level")]
    pub log_level: Option<LogLevel>,
    /// Bytes per copy. One buffer per direction per connection, so under `fork`
    /// this multiplies: 1 MiB across 1024 connections is 2 GiB resident.
    #[serde(rename = "buffer-size")]
    pub buffer_size: Option<ByteSize>,
    /// When to draw the progress line. Defaults to never: it is a foreground
    /// display, and a relay is as often a daemon as it is a command.
    #[serde(default)]
    pub progress: Option<ProgressMode>,
    #[serde(default)]
    pub log: Vec<LogSinkSpec>,
    /// Pipeline declarations, in `[[plugin]]` order.
    #[serde(default, rename = "plugin")]
    pub plugins: Vec<PluginSpec>,
}

impl Config {
    pub fn merge_cli(&mut self, cli: &Cli) -> anyhow::Result<()> {
        let layout = cli.layout();

        self.source = layout
            .source
            .map(Endpoint::Raw)
            .or_else(|| self.source.take());

        self.sink = layout.sink.map(Endpoint::Raw).or_else(|| self.sink.take());

        if let Some(size) = cli.buffer_size {
            self.buffer_size = Some(size);
        }

        if let Some(progress) = cli.progress {
            self.progress = Some(progress);
        }

        if cli.no_plugins {
            self.plugins.clear();
        }

        // Config entries first, then the inline pipeline, then `-p`. The
        // positionals read as the pipeline you drew on the command line, so
        // they keep their relative order; `-p` appends to it.
        for raw in layout.plugins.iter().chain(&cli.plugins) {
            self.plugins
                .push(parse_plugin_spec(raw).with_context(|| format!("invalid plugin {raw:?}"))?);
        }

        Ok(())
    }
}

/// Parse `NAME[:DIRECTION][,key=value]...`, e.g.
/// `tee:both,as=wire,file=session.hex,format=hex`.
///
/// `as` and `detach` are consumed by the host; everything else is handed to
/// the plugin.
///
/// Bare keys are `true`, so `tee,append` reads the way a flag should. Values
/// are coerced to bool/integer where they parse as one, since the plugin's
/// config type decides the real shape.
pub fn parse_plugin_spec(raw: &str) -> anyhow::Result<PluginSpec> {
    let mut parts = raw.split(',');
    let head = parts.next().unwrap_or_default().trim();

    if head.is_empty() {
        bail!("expected a plugin name");
    }

    let (name, direction) = match head.split_once(':') {
        Some((name, dir)) => (
            name,
            dir.parse::<DirectionSpec>()
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        ),
        None => (head, DirectionSpec::default()),
    };

    if name.is_empty() {
        bail!("expected a plugin name");
    }

    let mut config = Map::new();
    let mut detach = None;
    let mut alias = None;

    for opt in parts {
        let opt = opt.trim();
        if opt.is_empty() {
            continue;
        }

        let (key, value) = match opt.split_once('=') {
            Some((key, value)) => (key, coerce(value)),
            None => (opt, Value::Bool(true)),
        };

        // Reserved in every spelling, since that is how they are matched.
        match normalize(key).as_str() {
            "detach" => {
                detach = value.as_bool();
            }
            "as" => {
                alias = value.as_str().map(str::to_string);
            }
            // The key goes on as the user wrote it. Matching it against what the plugin declares is
            // the plugin's own deserialization, which needs the original to recognize a
            // `#[serde(alias)]`.
            _ => {
                config.insert(key.to_string(), value);
            }
        }
    }

    Ok(PluginSpec {
        name: name.to_string(),
        direction,
        alias,
        detach,
        config,
    })
}

fn coerce(value: &str) -> Value {
    match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => value
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(value.to_string())),
    }
}

pub fn load_config(
    explicit: Option<PathBuf>,
    no_config: bool,
) -> anyhow::Result<(Config, Option<PathBuf>)> {
    if no_config {
        return Ok((Config::default(), None));
    }

    if let Some(path) = explicit {
        let text = fs::read_to_string(&path).context("Failed to read config file")?;
        let config = toml::from_str(&text).context("Failed to parse toml file")?;
        return Ok((config, Some(path)));
    }

    for name in CONFIG_NAMES {
        match fs::read_to_string(name) {
            Ok(text) => {
                let config = toml::from_str(&text).context("Failed to parse toml file")?;
                return Ok((config, Some(PathBuf::from(name))));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context("unknown error"),
        }
    }

    Ok((Config::default(), None))
}

pub fn resolve(config: Config) -> anyhow::Result<Settings> {
    fn spec(endpoint: Option<Endpoint>, field: &str) -> anyhow::Result<EndpointSpec> {
        let Some(endpoint) = endpoint else {
            bail!("no {field} given");
        };
        endpoint
            .into_spec()
            .with_context(|| format!("invalid {field}"))
    }

    Ok(Settings {
        source: spec(config.source, "source")?,
        sink: spec(config.sink, "sink")?,
        plugins: config.plugins,
        buffer: config.buffer_size.unwrap_or(DEFAULT_BUFFER).bytes(),
        progress: config.progress.unwrap_or_default(),
    })
}
