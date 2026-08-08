//! `compress` / `decompress`: zstd stages for tocat.
//!
//! # Direction
//!
//! `direction = "both"` would compress *both* paths, which is almost never what
//! anyone wants. Compression is usually asymmetric: declare the pair
//! explicitly:
//!
//! ```toml
//! # near end of a compressed tunnel
//! [[plugin]]
//! name = "compress"
//! direction = "source-to-sink"
//!
//! [[plugin]]
//! name = "decompress"
//! direction = "sink-to-source"
//! ```
//!
//! The far end runs the mirror image (`decompress` forward, `compress`
//! reverse) and the two relays form a compressed link over an otherwise
//! plaintext hop.
//!
//! # Latency
//!
//! A relay must not sit on bytes waiting for a better compression window, so
//! every chunk ends with a zstd flush: whatever arrived is on the wire before
//! the call returns. That costs ratio (a flush closes the current block) and
//! it is the right trade for an interactive stream. Set `flush = false` for
//! bulk transfers where throughput matters and nobody is waiting on a prompt;
//! output then appears only as zstd fills its internal buffer, and on EOF.

use std::io::Write;

use serde::{Deserialize, Serialize};
use tocat_api::{
    BuildCtx, Ctx, Execution, LogLevel, Plugin, PluginError, PluginFactory, Result, Stage,
};

pub const COMPRESS: &str = "compress";
pub const DECOMPRESS: &str = "decompress";

const DEFAULT_LEVEL: i32 = 3;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CompressConfig {
    /// zstd level, 1–22. Higher is smaller and slower; 3 is zstd's default and
    /// is usually the right answer for a relay.
    #[serde(default = "default_level")]
    pub level: i32,

    /// Flush after every chunk so bytes reach the peer immediately.
    #[serde(default = "default_true")]
    pub flush: bool,

    /// Log the compression ratio when the stream ends.
    #[serde(default)]
    pub report: bool,
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            level: DEFAULT_LEVEL,
            flush: true,
            report: false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DecompressConfig {
    /// Log the expansion ratio when the stream ends.
    #[serde(default)]
    pub report: bool,
}

fn default_level() -> i32 {
    DEFAULT_LEVEL
}

fn default_true() -> bool {
    true
}

/// Maps [std::io::Error] to [PluginError]
fn io(err: std::io::Error, name: &'static str) -> PluginError {
    PluginError::runtime(name, err)
}

pub struct Compress {
    encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
    flush_each_chunk: bool,
    report: bool,
    read: u64,
    written: u64,
    finished: bool,
}

impl Compress {
    /// Move whatever zstd has produced so far into the pipeline.
    fn drain(&mut self, ctx: &mut Ctx<'_>) {
        let out = self.encoder.get_mut();

        if !out.is_empty() {
            self.written += out.len() as u64;
            ctx.forward(out);
            out.clear();
        }
    }
}

impl Plugin for Compress {
    fn name(&self) -> &str {
        COMPRESS
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        self.read += input.len() as u64;

        self.encoder.write_all(input).map_err(|e| io(e, COMPRESS))?;

        if self.flush_each_chunk {
            self.encoder.flush().map_err(|e| io(e, COMPRESS))?;
        }

        self.drain(ctx);

        Ok(())
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        if self.finished {
            return Ok(());
        }

        // Closes the frame. Without the epilogue the peer's decoder will
        // reject the stream as truncated.
        self.encoder.do_finish().map_err(|e| io(e, COMPRESS))?;
        self.finished = true;
        self.drain(ctx);

        if self.report && self.read > 0 {
            let ratio = self.written as f64 / self.read as f64;
            let (read, written) = (self.read, self.written);
            ctx.log(
                LogLevel::Info,
                &format!("compressed {read} to {written} bytes ({ratio:.3})"),
            );
        }

        Ok(())
    }
}

pub struct Decompress {
    decoder: zstd::stream::write::Decoder<'static, Vec<u8>>,
    report: bool,
    read: u64,
    written: u64,
}

impl Decompress {
    fn drain(&mut self, ctx: &mut Ctx<'_>) {
        let out = self.decoder.get_mut();

        if !out.is_empty() {
            self.written += out.len() as u64;
            ctx.forward(out);
            out.clear();
        }
    }
}

impl Plugin for Decompress {
    fn name(&self) -> &str {
        DECOMPRESS
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        self.read += input.len() as u64;

        // zstd frames do not line up with read boundaries; the decoder holds
        // any partial frame until the rest arrives.
        self.decoder
            .write_all(input)
            .map_err(|e| io(e, DECOMPRESS))?;
        self.decoder.flush().map_err(|e| io(e, DECOMPRESS))?;

        self.drain(ctx);

        Ok(())
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        self.decoder.flush().map_err(|e| io(e, DECOMPRESS))?;
        self.drain(ctx);

        if self.report && self.read > 0 {
            let (read, written) = (self.read, self.written);
            ctx.log(
                LogLevel::Info,
                &format!("decompressed {read} to {written} bytes"),
            );
        }

        Ok(())
    }
}

pub struct CompressFactory;

impl PluginFactory for CompressFactory {
    fn name(&self) -> &str {
        COMPRESS
    }

    fn description(&self) -> &str {
        "zstd-compress this direction"
    }

    fn execution(&self) -> Execution {
        // Expensive per byte: worth a task of its own.
        Execution::Detached
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: CompressConfig = ctx.config()?;

        if !(1..=22).contains(&config.level) {
            return Err(PluginError::config(
                COMPRESS,
                format!("level {} is outside 1..=22", config.level),
            ));
        }

        let encoder = zstd::stream::write::Encoder::new(Vec::new(), config.level)
            .map_err(|e| io(e, COMPRESS))?;

        Ok(Stage::filter(Compress {
            encoder,
            flush_each_chunk: config.flush,
            report: config.report,
            read: 0,
            written: 0,
            finished: false,
        }))
    }
}

pub struct DecompressFactory;

impl PluginFactory for DecompressFactory {
    fn name(&self) -> &str {
        DECOMPRESS
    }

    fn description(&self) -> &str {
        "zstd-decompress this direction"
    }

    fn execution(&self) -> Execution {
        // Expensive per byte: worth a task of its own.
        Execution::Detached
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: DecompressConfig = ctx.config()?;

        let decoder =
            zstd::stream::write::Decoder::new(Vec::new()).map_err(|e| io(e, DECOMPRESS))?;

        Ok(Stage::filter(Decompress {
            decoder,
            report: config.report,
            read: 0,
            written: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tocat_api::{
        ChannelId, ChannelTarget, Direction, EffectSink, Emission, HostBuilder, PipelineMeta,
        StageInfo,
    };

    use super::*;

    struct NullHost;

    impl HostBuilder for NullHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> Result<ChannelId> {
            Ok(ChannelId(0))
        }
    }

    #[derive(Default)]
    struct Silent;

    impl EffectSink for Silent {
        fn write(&mut self, _channel: ChannelId, _bytes: &[u8]) {}
        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
    }

    fn build(factory: &dyn PluginFactory, config: Value) -> Box<dyn Plugin> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: factory.name(),
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(factory.name(), &map, &meta, stage, &mut host);
        match factory.build(&mut ctx).expect("build") {
            Stage::Filter(plugin) => plugin,
            Stage::External(_) => unreachable!("compress stages are filters"),
        }
    }

    fn feed(plugin: &mut dyn Plugin, input: &[u8]) -> Vec<u8> {
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;
        {
            let mut ctx = Ctx::new(&meta, "compress", input, &mut emission, &mut sink);
            plugin.on_bytes(&mut ctx, input).expect("on_bytes");
        }

        emission.bytes().to_vec()
    }

    fn finish(plugin: &mut dyn Plugin) -> Vec<u8> {
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;
        {
            let mut ctx = Ctx::new(&meta, "compress", &[], &mut emission, &mut sink);
            plugin.on_eof(&mut ctx).expect("on_eof");
        }

        emission.bytes().to_vec()
    }

    #[test]
    fn round_trips_across_chunk_boundaries() {
        let mut encoder = build(&CompressFactory, json!({}));
        let mut decoder = build(&DecompressFactory, json!({}));

        let mut wire = Vec::new();
        for chunk in [
            b"hello ".as_slice(),
            b"compressed ".as_slice(),
            b"world".as_slice(),
        ] {
            wire.extend_from_slice(&feed(encoder.as_mut(), chunk));
        }
        wire.extend_from_slice(&finish(encoder.as_mut()));

        // Hand the decoder a boundary that does not match the encoder's.
        let (head, tail) = wire.split_at(wire.len() / 2);
        let mut plain = feed(decoder.as_mut(), head);
        plain.extend_from_slice(&feed(decoder.as_mut(), tail));
        plain.extend_from_slice(&finish(decoder.as_mut()));

        assert_eq!(plain, b"hello compressed world");
    }

    #[test]
    fn flushing_makes_each_chunk_visible_immediately() {
        let mut encoder = build(&CompressFactory, json!({}));
        assert!(
            !feed(encoder.as_mut(), b"first").is_empty(),
            "a flushing encoder must emit before EOF",
        );
    }

    #[test]
    fn rejects_out_of_range_levels() {
        let map = json!({ "level": 30 }).as_object().unwrap().clone();
        let meta = meta();
        let mut host = NullHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: COMPRESS,
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(COMPRESS, &map, &meta, stage, &mut host);
        assert!(CompressFactory.build(&mut ctx).is_err());
    }
}
