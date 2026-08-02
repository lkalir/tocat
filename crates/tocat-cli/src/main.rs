//! tocat — a socat-inspired relay with a plugin pipeline between its endpoints.
//!
//! Startup is ordered around two constraints that are easy to break:
//!
//! * A `tracing` subscriber can only be installed once. `bootstrap_logging`
//!   returns a scoped guard so config-loading failures are reportable before
//!   the real sinks exist; it must be dropped before `init_logging` installs
//!   the global one.
//! * `--dump-config` and `--list-plugins` answer questions about the
//!   configuration, so they exit before anything opens a socket or a file.
//!
//! After that: merge the config file with the CLI, resolve endpoints, build the
//! plugin registry, then `Relay::new` — which constructs every declared plugin
//! and opens its side channels, so a bad declaration fails here rather than on
//! the first byte of the first connection.

mod buffer;
mod child;
mod cli;
mod config;
mod endpoint;
mod host;
mod logging;
mod progress;
mod pump;
mod relay;
mod shutdown;

use std::{process::ExitCode, time::Duration};

use clap::Parser;
use cli::Cli;
use config::{load_config, resolve};
use logging::{bootstrap_logging, init_logging};
use tocat_api::Registry;
use tracing::{debug, error};

use crate::{progress::Progress, relay::Relay};

/// How long teardown waits on blocking tasks before leaving them behind.
///
/// The synchronous copy path polls for shutdown between chunks, so it normally
/// stops on its own. This covers the case it cannot: a read that never returns,
/// such as a FIFO with no writer. Without a bound, dropping the runtime waits
/// on that read forever and the process becomes unkillable by signal.
const TEARDOWN_GRACE: Duration = Duration::from_millis(250);

fn main() -> anyhow::Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = runtime.block_on(start());
    runtime.shutdown_timeout(TEARDOWN_GRACE);

    result
}

async fn start() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();

    // Everything compiled into this binary. A WASM loader would add its
    // discovered modules to the same registry.
    let registry: Registry = tocat_plugins::native_registry();

    if cli.list_plugins {
        for factory in registry.iter() {
            println!("{:<12} {}", factory.name(), factory.description());
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Setup logging
    let cli_level = cli.verbose_level();
    let initial = cli_level.unwrap_or_default();

    let bootstrap = bootstrap_logging(initial);

    // Merge cli and config arguments
    let (mut config, source_file) = load_config(cli.config.clone(), cli.no_config)?;
    config.merge_cli(&cli)?;

    let level = match cli.log_level {
        Some(explicit) => explicit,
        None => cli_level.max(config.log_level).unwrap_or_default(),
    };

    config.log_level = Some(level);

    if cli.dump_config {
        println!("{}", toml::to_string(&config).unwrap());
        return Ok(ExitCode::SUCCESS);
    }

    // Need to drop our bootstrap logger here so we can correctly initiliaze the
    // real one
    drop(bootstrap);

    let _logging_guard = init_logging(&config.log, level)?;

    let settings = resolve(config)?;

    match &source_file {
        Some(path) => debug!("using config file: {}", path.display()),
        None => debug!("no config found, using defaults"),
    }

    let shutdown = shutdown::install();

    // Started before the relay so the display is up while the endpoints are
    // being connected: a `tcp:` that hangs on connect should look like
    // something waiting rather than like nothing happening.
    let progress = progress::start(settings.progress, &settings.source, &settings.sink);

    // Plugin construction happens here, so a bad declaration or an unopenable
    // dump file fails before either endpoint is touched.
    let relay = match Relay::new(
        settings.source,
        settings.sink,
        settings.plugins,
        registry,
        settings.buffer,
        progress.as_ref().map(Progress::meter),
    )
    .await
    {
        Ok(relay) => relay,
        Err(e) => {
            error!("Plugin setup failed: {e:#}");
            finish(progress).await;
            return Ok(ExitCode::FAILURE);
        }
    };

    let outcome = relay.run(shutdown).await;

    // Before the error is reported: the summary line belongs above the
    // diagnostics, not wedged between them.
    finish(progress).await;

    if let Err(e) = outcome {
        error!("Relay failed: {e:#}");
        return Ok(ExitCode::FAILURE);
    }

    Ok(ExitCode::SUCCESS)
}

/// Take the progress line down, if there was one.
async fn finish(progress: Option<Progress>) {
    if let Some(progress) = progress {
        progress.finish().await;
    }
}
