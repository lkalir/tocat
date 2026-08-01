//! `tee` — mirror a path's bytes to a side channel, verbatim or as a hex dump.
//!
//! ```toml
//! [[plugin]]
//! name = "tee"
//! direction = "both"     # one instance per path, each with its own offset
//! file = "session.hex"   # omit (or "-") for stderr
//! format = "hex"
//! ```
//!
//! Two entries naming the same `file` share one buffered writer, so a
//! bidirectional dump interleaves at chunk granularity instead of racing.
//!
//! Entries are headed `[<source> -> <sink> | <stage>]`, so a dump file shared
//! by several taps and several connections stays separable: the endpoints
//! carry the peer under `fork`, and the stage name is whatever `as` said (or
//! `tee#1`, `tee#2` for unnamed repeats).
//!
//! `tee` never touches the payload: it calls `ctx.pass_through()`, so inserting
//! it costs one virtual call per chunk plus whatever the dump itself costs:
//! a `memcpy` into the host's staging buffer for raw, or formatting for hex.

use std::{fmt::Write as _, path::PathBuf};

use serde::{Deserialize, Serialize};
use tocat_api::{
    BuildCtx, ChannelId, ChannelTarget, Ctx, Plugin, PluginError, PluginFactory, Result, Stage,
};

pub const NAME: &str = "tee";

const DEFAULT_WIDTH: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DumpFormat {
    /// `hexdump -C`-ish rows behind a direction and offset header.
    Hex,
    /// The bytes themselves, unmodified.
    #[default]
    #[serde(alias = "raw", alias = "binary")]
    RawBinary,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TeeConfig {
    /// Destination. Absent, `-`, `stderr` or `/dev/stderr` all mean stderr.
    #[serde(default)]
    pub file: Option<PathBuf>,

    #[serde(default)]
    pub format: DumpFormat,

    /// Append to an existing file rather than truncating it.
    #[serde(default = "default_true")]
    pub append: bool,

    /// Overrides the hex header label, which is otherwise
    /// `"<upstream> -> <downstream> | <stage name>"`.
    #[serde(default)]
    pub label: Option<String>,

    /// Bytes per row in hex mode.
    #[serde(default = "default_width")]
    pub width: usize,
}

impl Default for TeeConfig {
    fn default() -> Self {
        Self {
            file: None,
            format: DumpFormat::default(),
            append: true,
            label: None,
            width: DEFAULT_WIDTH,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_width() -> usize {
    DEFAULT_WIDTH
}

impl TeeConfig {
    /// Resolve `file` to a channel target.
    ///
    /// stdout is rejected rather than supported: on a stdio endpoint it carries
    /// relay payload, and mixing a dump into it corrupts the transfer.
    pub fn target(&self) -> Result<ChannelTarget> {
        let Some(path) = &self.file else {
            return Ok(ChannelTarget::Stderr);
        };

        match path.to_str() {
            Some("-" | "stderr" | "/dev/stderr" | "/dev/fd/2") => Ok(ChannelTarget::Stderr),
            Some("stdout" | "/dev/stdout" | "/dev/fd/1") => Err(PluginError::config(
                NAME,
                "refusing to write to stdout, it may carry relay payload; use `-` for stderr",
            )),
            _ => Ok(ChannelTarget::File {
                path: path.clone(),
                append: self.append,
            }),
        }
    }
}

pub struct Tee {
    channel: ChannelId,
    format: DumpFormat,
    label: String,
    width: usize,
    offset: u64,
    /// Reused across chunks so hex mode does not allocate per call.
    scratch: String,
}

impl Tee {
    /// Bytes observed on this path so far.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

impl Plugin for Tee {
    fn name(&self) -> &str {
        NAME
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        // Zero-copy: downstream gets the original slice.
        ctx.pass_through();

        if input.is_empty() {
            return Ok(());
        }

        match self.format {
            DumpFormat::RawBinary => ctx.side_write(self.channel, input),
            DumpFormat::Hex => {
                self.scratch.clear();
                let _ = writeln!(
                    self.scratch,
                    "[{}] {} bytes @ {:#x}",
                    self.label,
                    input.len(),
                    self.offset
                );
                write_hex_dump(&mut self.scratch, input, self.offset, self.width);
                self.scratch.push('\n');
                ctx.side_write(self.channel, self.scratch.as_bytes());
            }
        }

        self.offset += input.len() as u64;

        Ok(())
    }
}

pub struct TeeFactory;

impl PluginFactory for TeeFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "mirror the stream to a file or stderr, verbatim or as a hex dump"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: TeeConfig = ctx.config()?;

        if config.width == 0 {
            return Err(PluginError::config(NAME, "width must be at least 1"));
        }

        let channel = ctx.open_channel(config.target()?)?;
        // Two questions a dump has to answer: which stream, and which tap.
        // The stream comes from the endpoints (with the peer already folded in
        // under fork), the tap from this stage's name — which the user can set
        // with `as`. Neighbouring stage names are static structure and belong
        // in the startup log, not on every entry.
        let label = config.label.clone().unwrap_or_else(|| {
            let meta = ctx.meta().label();
            let stage = ctx.stage().name;
            format!("{meta} | {stage}")
        });

        Ok(Stage::filter(Tee {
            channel,
            format: config.format,
            label,
            width: config.width,
            offset: 0,
            scratch: String::new(),
        }))
    }
}

/// Append offset / hex / ASCII rows for `buf` to `out`.
pub fn write_hex_dump(out: &mut String, buf: &[u8], start_offset: u64, width: usize) {
    let width = width.max(1);

    for (row, chunk) in buf.chunks(width).enumerate() {
        if row > 0 {
            out.push('\n');
        }

        let _ = write!(out, "{:08x}  ", start_offset + (row * width) as u64);

        for (i, byte) in chunk.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let _ = write!(out, "{byte:02x}");
        }

        // Pad the hex column so the ASCII gutter lines up on short rows.
        for _ in 0..(width - chunk.len()) * 3 {
            out.push(' ');
        }

        out.push_str("  |");
        for &byte in chunk {
            out.push(if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '.'
            });
        }
        out.push('|');
    }
}

#[must_use]
pub fn hex_dump(buf: &[u8], start_offset: u64, width: usize) -> String {
    let mut out = String::new();
    write_hex_dump(&mut out, buf, start_offset, width);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tocat_api::{Direction, EffectSink, Emit, LogLevel, PipelineMeta, StageInfo};

    use super::*;

    #[derive(Default)]
    struct Recorder(Vec<Vec<u8>>);

    impl EffectSink for Recorder {
        fn write(&mut self, _channel: ChannelId, bytes: &[u8]) {
            self.0.push(bytes.to_vec());
        }

        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}
    }

    struct CountingHost(u32);

    impl tocat_api::HostBuilder for CountingHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> Result<ChannelId> {
            let id = ChannelId(self.0);
            self.0 += 1;
            Ok(id)
        }
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "tcp://a", "STDIO")
    }

    fn stage(name: &str) -> StageInfo<'_> {
        StageInfo {
            index: 0,
            total: 1,
            name,
            upstream: "tcp://a",
            downstream: "STDIO",
        }
    }

    fn build_named(config: serde_json::Value, name: &str) -> Box<dyn Plugin> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = CountingHost(0);
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage(name), &mut host);
        match TeeFactory.build(&mut ctx).expect("build") {
            Stage::Filter(plugin) => plugin,
            Stage::External(_) => unreachable!("tee is a filter"),
        }
    }

    fn build(config: serde_json::Value) -> Box<dyn Plugin> {
        build_named(config, NAME)
    }

    fn feed(plugin: &mut dyn Plugin, sink: &mut Recorder, input: &[u8]) -> Emit {
        let meta = meta();
        let mut out = Vec::new();
        let mut emit = Emit::Pending;
        {
            let mut ctx = Ctx::new(&meta, NAME, input, &mut out, &mut emit, sink);
            plugin.on_bytes(&mut ctx, input).unwrap();
        }

        assert!(out.is_empty(), "tee must never materialise the payload");
        emit
    }

    #[test]
    fn passes_through_and_mirrors_raw_bytes() {
        let mut tee = build(json!({}));
        let mut sink = Recorder::default();

        assert_eq!(feed(tee.as_mut(), &mut sink, b"ping"), Emit::Passthrough);
        assert_eq!(sink.0, vec![b"ping".to_vec()]);
    }

    #[test]
    fn hex_header_tracks_offset() {
        let mut tee = build(json!({ "format": "hex" }));
        let mut sink = Recorder::default();

        feed(tee.as_mut(), &mut sink, b"aaaa");
        feed(tee.as_mut(), &mut sink, b"bb");

        let first = String::from_utf8(sink.0[0].clone()).unwrap();
        let second = String::from_utf8(sink.0[1].clone()).unwrap();

        assert!(first.starts_with("[tcp://a -> STDIO | tee] 4 bytes @ 0x0\n"));
        assert!(second.starts_with("[tcp://a -> STDIO | tee] 2 bytes @ 0x4\n"));
    }

    /// The point of the stage name: two taps on the same connection, sharing
    /// one dump file, have to stay tellable apart.
    #[test]
    fn header_carries_the_connection_and_the_tap() {
        let mut tee = build_named(json!({ "format": "hex" }), "audit");
        let mut sink = Recorder::default();

        feed(tee.as_mut(), &mut sink, b"x");

        let header = String::from_utf8(sink.0[0].clone()).unwrap();
        assert!(header.starts_with("[tcp://a -> STDIO | audit] 1 bytes @ 0x0\n"));
    }

    #[test]
    fn stdout_is_refused() {
        let config = TeeConfig {
            file: Some(PathBuf::from("/dev/stdout")),
            ..TeeConfig::default()
        };
        assert!(config.target().is_err());
    }

    #[test]
    fn hex_rows_align() {
        assert_eq!(hex_dump(b"hi", 0, 4), "00000000  68 69        |hi|");
        assert_eq!(
            hex_dump(b"hihi", 0, 2),
            "00000000  68 69  |hi|\n00000002  68 69  |hi|"
        );
    }
}
