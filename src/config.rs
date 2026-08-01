use std::{fs, path::PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::{
    cli::Cli,
    endpoint::{Endpoint, EndpointSpec},
    logging::{LogLevel, LogSinkSpec},
};

const CONFIG_NAMES: &[&str] = &["tocat.toml", ".tocat.toml"];

#[derive(Debug)]
pub struct Settings {
    pub source: EndpointSpec,
    pub sink: EndpointSpec,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub source: Option<Endpoint>,
    pub sink: Option<Endpoint>,
    #[serde(rename = "log-level")]
    pub log_level: Option<LogLevel>,
    #[serde(default)]
    pub log: Vec<LogSinkSpec>,
}

impl Config {
    pub fn merge_cli(&mut self, cli: &Cli) {
        self.source = cli
            .source
            .clone()
            .or_else(|| cli.source_pos.clone())
            .map(Endpoint::Raw)
            .or_else(|| self.source.take());

        self.sink = cli
            .sink
            .clone()
            .or_else(|| cli.sink_pos.clone())
            .map(Endpoint::Raw)
            .or_else(|| self.sink.take());
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

    let source = spec(config.source, "source")?;
    let sink = spec(config.sink, "sink")?;

    check_dump(&source, "source")?;
    check_dump(&sink, "sink")?;

    Ok(Settings { source, sink })
}

fn check_dump(spec: &EndpointSpec, field: &str) -> anyhow::Result<()> {
    if let Some(path) = spec.dump_config().and_then(|c| c.file.as_ref())
        && matches!(
            path.to_str(),
            Some("/dev/stdout") | Some("-") | Some("/dev/fd/1")
        )
    {
        anyhow::bail!("{field} cannot dump to stdout; omit `file` to dump to stderr");
    }

    Ok(())
}
