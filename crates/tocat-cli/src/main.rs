//! tocat main

mod cli;
mod config;
mod endpoint;
mod logging;
mod relay;
mod shutdown;

use std::process::ExitCode;

use clap::Parser;
use cli::Cli;
use config::{load_config, resolve};
use logging::{bootstrap_logging, init_logging};
use tracing::{debug, error};

use crate::relay::run;

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let cli_level = cli.verbose_level();
    let initial = cli_level.unwrap_or_default();

    let bootstrap = bootstrap_logging(initial);

    let (mut config, source_file) = load_config(cli.config.clone(), cli.no_config)?;
    config.merge_cli(&cli);

    let level = match cli.log_level {
        Some(explicit) => explicit,
        None => cli_level.max(config.log_level).unwrap_or_default(),
    };

    config.log_level = Some(level);

    if cli.dump_config {
        println!("{}", toml::to_string(&config).unwrap());
        return Ok(ExitCode::SUCCESS);
    }

    drop(bootstrap);

    let _logging_guard = init_logging(&config.log, level)?;

    let settings = resolve(config)?;

    match &source_file {
        Some(path) => debug!("using config file: {}", path.display()),
        None => debug!("no config found, using defaults"),
    }

    debug!("{settings:#?}");

    let shutdown = shutdown::install();

    if let Err(e) = run(settings.source, settings.sink, shutdown).await {
        error!("Relay failed: {e:#}");
        return Ok(ExitCode::FAILURE);
    }

    Ok(ExitCode::SUCCESS)
}
