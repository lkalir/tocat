//! host.rs — the host half of the plugin API.
//!
//! Plugins are synchronous and never touch the filesystem: they ask for a
//! channel at build time and stage bytes against it at run time.
//! [`ChannelPlan`] hands out the handles, [`Effects`] stages the bytes, and
//! [`Channels`] owns the writers and flushes them on the relay's runtime.
//!
//! Staging is per-channel and reused across chunks, so a side write costs one
//! `extend_from_slice` — no allocation and no lock inside the plugin call, and
//! the flush overlaps the downstream write.

use std::{path::Path, sync::Arc};

use anyhow::{Context, bail};
use tocat_api::{
    ChannelId, ChannelTarget, EffectSink, HostBuilder, LogLevel as PluginLogLevel, PluginError,
    Result as PluginResult,
};
use tokio::{
    fs::OpenOptions,
    io::{AsyncWriteExt, BufWriter, Stderr},
    sync::Mutex,
};
use tracing::{debug, error, info, trace, warn};

const DUMP_BUF: usize = 256 * 1024;

/// Collects the side channels plugins ask for, de-duplicating by target.
///
/// Two plugins naming the same file get the same [`ChannelId`] and therefore
/// the same writer. Cloneable, so each accepted connection can build its own
/// plugin instances against the frozen plan without touching a lock.
#[derive(Debug, Clone, Default)]
pub struct ChannelPlan {
    targets: Vec<ChannelTarget>,
    frozen: bool,
}

impl ChannelPlan {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop accepting new targets, once the channels have been opened.
    pub fn freeze(&mut self) {
        self.frozen = true;
    }

    #[must_use]
    pub fn targets(&self) -> &[ChannelTarget] {
        &self.targets
    }
}

impl HostBuilder for ChannelPlan {
    fn open_channel(&mut self, target: ChannelTarget) -> PluginResult<ChannelId> {
        if let ChannelTarget::File { path, .. } = &target
            && is_stdout(path)
        {
            return Err(PluginError::host(
                "refusing to open stdout as a side channel; it may carry relay payload",
            ));
        }

        if let Some(index) = self.targets.iter().position(|known| *known == target) {
            return Ok(ChannelId(index as u32));
        }

        if self.frozen {
            return Err(PluginError::host(format!(
                "channel plan is frozen but a plugin asked for a new target: {target:?}"
            )));
        }

        self.targets.push(target);
        Ok(ChannelId((self.targets.len() - 1) as u32))
    }
}

fn is_stdout(path: &Path) -> bool {
    matches!(
        path.to_str(),
        Some("-" | "stdout" | "/dev/stdout" | "/dev/fd/1")
    )
}

/// Per-task staging for the effects raised during one chunk.
pub struct Effects {
    staging: Vec<Vec<u8>>,
    dirty: Vec<usize>,
    logs: Vec<(PluginLogLevel, String, String)>,
    unknown: Option<ChannelId>,
}

impl Effects {
    #[must_use]
    pub fn new(channels: &Channels) -> Self {
        Self {
            staging: (0..channels.len()).map(|_| Vec::new()).collect(),
            dirty: Vec::new(),
            logs: Vec::new(),
            unknown: None,
        }
    }

    /// Nothing staged during the last plugin call.
    ///
    /// Checked by the pumps to skip `Channels::apply` altogether. It has to
    /// account for `unknown` as well as the buffers, since that is reported
    /// from `apply` and skipping would swallow it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty() && self.logs.is_empty() && self.unknown.is_none()
    }
}

impl EffectSink for Effects {
    fn write(&mut self, channel: ChannelId, bytes: &[u8]) {
        let index = channel.index();

        match self.staging.get_mut(index) {
            Some(buf) => {
                if buf.is_empty() {
                    self.dirty.push(index);
                }
                buf.extend_from_slice(bytes);
            }
            // Cannot fail the plugin call from here; surfaced on the next apply.
            None => self.unknown = Some(channel),
        }
    }

    fn log(&mut self, level: PluginLogLevel, stage: &str, message: &str) {
        self.logs
            .push((level, stage.to_string(), message.to_string()));
    }
}

/// Data sinks for plugins
enum Sink {
    Stderr(Stderr),
    File(BufWriter<tokio::fs::File>),
}

impl Sink {
    async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            // stderr is shared with tracing: keep it unbuffered so a dump is
            // not stranded behind a partial line.
            Sink::Stderr(w) => {
                w.write_all(bytes).await?;
                w.flush().await
            }
            Sink::File(w) => w.write_all(bytes).await,
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Stderr(w) => w.flush().await,
            Sink::File(w) => w.flush().await,
        }
    }
}

/// The opened side channels, shared by every pipeline in the process.
///
/// Each sink has its own mutex, so two directions contend only when they
/// genuinely target the same file.
pub struct Channels {
    sinks: Vec<Mutex<Sink>>,
}

impl Channels {
    pub async fn open(targets: &[ChannelTarget]) -> anyhow::Result<Arc<Self>> {
        let mut sinks = Vec::with_capacity(targets.len());

        for target in targets {
            let sink = match target {
                ChannelTarget::Stderr => Sink::Stderr(tokio::io::stderr()),
                ChannelTarget::File { path, append } => {
                    let file = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .append(*append)
                        .truncate(!*append)
                        .open(path)
                        .await
                        .with_context(|| format!("opening plugin channel {}", path.display()))?;

                    debug!(path = %path.display(), append, "opened plugin channel");
                    Sink::File(BufWriter::with_capacity(DUMP_BUF, file))
                }
            };

            sinks.push(Mutex::new(sink));
        }

        Ok(Arc::new(Self { sinks }))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sinks.len()
    }

    // Unused, but `len` without `is_empty` trips `clippy::len_without_is_empty`.
    #[must_use]
    #[allow(unused)]
    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    /// Flush everything staged during the last plugin call.
    pub async fn apply(&self, effects: &mut Effects) -> anyhow::Result<()> {
        if let Some(channel) = effects.unknown.take() {
            bail!("plugin wrote to unknown channel {channel:?}");
        }

        if !effects.dirty.is_empty() {
            let mut dirty = std::mem::take(&mut effects.dirty);

            for index in dirty.drain(..) {
                let buf = &mut effects.staging[index];

                if buf.is_empty() {
                    continue;
                }

                let sink = self
                    .sinks
                    .get(index)
                    .with_context(|| format!("unknown channel {index}"))?;

                sink.lock().await.write_all(buf).await?;
                buf.clear();
            }

            effects.dirty = dirty;
        }

        for (level, stage, message) in effects.logs.drain(..) {
            match level {
                PluginLogLevel::Trace => trace!(target: "plugin", stage, "{message}"),
                PluginLogLevel::Debug => debug!(target: "plugin", stage, "{message}"),
                PluginLogLevel::Info => info!(target: "plugin", stage, "{message}"),
                PluginLogLevel::Warn => warn!(target: "plugin", stage, "{message}"),
                PluginLogLevel::Error => error!(target: "plugin", stage, "{message}"),
            }
        }

        Ok(())
    }

    /// Flush every channel
    ///
    /// Runs concurrenctly since, in the case of an error, this could be the
    /// last chance to write data to disk. Therefore, we don't want a single
    /// sink's failure to cancel all the other flushes.
    pub async fn flush(&self) -> anyhow::Result<()> {
        let flushes = self
            .sinks
            .iter()
            .enumerate()
            .map(|(index, sink)| async move {
                sink.lock()
                    .await
                    .flush()
                    .await
                    .with_context(|| format!("flushing plugin channel {index}"))
            });

        let mut failure = None;

        for result in futures_util::future::join_all(flushes).await {
            if let Err(e) = result {
                failure.get_or_insert(e);
            }
        }

        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
