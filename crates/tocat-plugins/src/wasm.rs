//! `wasm` - run a WebAssembly guest as a stage.
//!
//! ```toml
//! [[plugin]]
//! name = "wasm"
//! module = "/usr/lib/tocat/redact.wasm"
//! direction = "source-to-sink"
//! config = { patterns = ["authorization", "cookie"] }
//! ```
//!
//! The guest is an ordinary stage: it sits where it was written, takes a
//! direction, forwards or rewrites what it is handed, and its effects are
//! applied by the host like any other stage's. What it cannot do is reach
//! anything. It imports no host functions at all, so it has no clock, no
//! files, no sockets and no way to spawn anything, and that is a property of
//! the module rather than a promise: a module that imports so much as WASI is
//! rejected when it is loaded.
//!
//! Nothing in the relay knows this stage is special. See [`abi`] for the guest
//! side of the contract.

mod abi;
mod engine;

use std::{path::PathBuf, time::Duration};

use engine::Guest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tocat_api::{
    Boundaries, BuildCtx, ByteSize, Ctx, Execution, Needs, Plugin, PluginError, PluginFactory,
    Result, Stage,
};

pub const NAME: &str = "wasm";

/// Enough for a few hundred instructions per byte of a full buffer, which
/// covers any straightforward transform and stops a runaway loop within
/// milliseconds. Per call, so a long connection cannot exhaust it.
fn default_fuel() -> u64 {
    100_000_000
}

/// Per instance, and an instance is per direction per connection, so this is
/// the number that multiplies under `fork`.
fn default_memory() -> ByteSize {
    ByteSize(64 * 1024 * 1024)
}

/// A guest with no options should see an empty table rather than `null`, so
/// that its own `Deserialize` fills in its defaults.
fn empty_options() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WasmConfig {
    /// The `.wasm` file to load. Compiled once per process however many
    /// stages and connections name it.
    #[serde(alias = "path", alias = "file")]
    pub module: PathBuf,

    /// Instructions one call may cost before the guest is trapped. 0 is
    /// unmetered, which gives a guest the ability to hang the relay.
    #[serde(default = "default_fuel")]
    pub fuel: u64,

    /// Ceiling on the guest's linear memory.
    #[serde(default = "default_memory")]
    pub memory_max: ByteSize,

    /// Handed to the guest verbatim, as JSON, once. tocat does not look
    /// inside: the guest owns its own options and its own error messages.
    #[serde(default = "empty_options")]
    pub config: Value,
}

pub struct Wasm {
    guest: Guest,
    tick: Option<Duration>,
    boundaries: Boundaries,
    needs: Needs,
}

impl std::fmt::Debug for Wasm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wasm")
            .field("guest", &"<guest>")
            .field("tick", &self.tick)
            .field("boundaries", &self.boundaries)
            .field("needs", &self.needs)
            .finish()
    }
}

impl Plugin for Wasm {
    fn name(&self) -> &str {
        NAME
    }

    fn tick_interval(&self) -> Option<Duration> {
        self.tick
    }

    fn boundaries(&self) -> Boundaries {
        self.boundaries
    }

    fn needs(&self) -> Needs {
        self.needs
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        self.guest.on_bytes(input)?;
        self.drain(ctx)
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        self.guest.on_eof()?;
        self.drain(ctx)
    }

    fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        self.guest.on_tick()?;
        self.drain(ctx)
    }
}

impl Wasm {
    /// Read the outbox the last call left and apply it.
    ///
    /// This is the whole of the host side of the ABI. It runs after every
    /// call, including one where the guest did nothing, because deciding
    /// whether it needs to run would cost as much as running it.
    fn drain(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        let outbox = self.guest.outbox()?;
        let memory = self.guest.memory();

        // Logs first, so anything the guest wanted to say about this chunk is
        // said even if it goes on to fail.
        let stride = abi::LOG_RECORD_LEN;
        let logs = abi::slice(
            memory,
            outbox.logs.ptr,
            outbox.logs.len.saturating_mul(stride),
        )?;

        for record in logs.chunks_exact(stride as usize) {
            let field = |at: usize| {
                u32::from_le_bytes([record[at], record[at + 1], record[at + 2], record[at + 3]])
            };

            let message = abi::slice(memory, field(4), field(8))?;
            ctx.log(abi::log_level(field(0)), &String::from_utf8_lossy(message));
        }

        match outbox.emit {
            abi::EMIT_PASSTHROUGH => ctx.pass_through(),
            abi::EMIT_DROP => ctx.drop_chunk(),
            abi::EMIT_BUFFERED => {
                let bytes = abi::slice(memory, outbox.bytes.ptr, outbox.bytes.len)?;
                let bounds = abi::bounds(memory, outbox.bounds, outbox.bytes.len)?;

                // Each span between boundaries becomes one unit, which is one
                // write at a byte sink and one datagram at a datagram sink.
                // The trailing one needs no boundary of its own.
                let mut start = 0;
                for end in bounds {
                    if end >= start {
                        ctx.forward(&bytes[start..end]);
                        ctx.boundary();
                        start = end;
                    }
                }

                ctx.forward(&bytes[start..]);
            }
            other => {
                return Err(PluginError::runtime(
                    NAME,
                    format!("guest set an unknown emit kind: {other}"),
                ));
            }
        }

        if outbox.has(abi::FLAG_REARM) {
            ctx.rearm();
        }

        if outbox.has(abi::FLAG_PACE) {
            ctx.pace(Duration::from_nanos(outbox.pace_ns));
        }

        // Halt and error are both "this path is over", and differ only in
        // whether that is a success. Reading the message before either keeps
        // the two paths identical.
        if outbox.has(abi::FLAG_HALT) || outbox.has(abi::FLAG_ERROR) {
            let message = abi::slice(memory, outbox.message.ptr, outbox.message.len)?;
            let message = String::from_utf8_lossy(message).into_owned();

            if outbox.has(abi::FLAG_ERROR) {
                return Err(PluginError::runtime(NAME, message));
            }

            ctx.halt(&message);
        }

        Ok(())
    }
}

pub struct WasmFactory;

impl PluginFactory for WasmFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "run a WebAssembly guest as a stage"
    }

    /// A guest call is a bounds check, a copy and an interpreted or JIT-ed
    /// body, which is enough per byte to be worth its own task. `detach =
    /// false` still works for a guest cheap enough not to want one.
    fn execution(&self) -> Execution {
        Execution::Detached
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: WasmConfig = ctx.config()?;

        let pre = engine::load(&config.module)?;

        // The guest's own options are JSON on the wire between host and
        // guest, which is the one representation both ends already have.
        let options = serde_json::to_vec(&config.config)
            .map_err(|e| PluginError::config(NAME, format!("config: {e}")))?;

        let guest = Guest::new(&pre, config.memory_max.bytes(), config.fuel, &options)?;

        Ok(Stage::filter(Wasm {
            tick: guest.tick_interval(),
            boundaries: guest.boundaries(),
            needs: guest.needs(),
            guest,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tocat_api::{ChannelId, Direction, EffectSink, Emission, Emit, LogLevel, PipelineMeta};

    use super::*;

    /// A guest that passes everything through, in the fewest exports the ABI
    /// allows. The outbox lives at address 0 and is written once, at
    /// instantiation, since passthrough says nothing else.
    const PASSTHROUGH: &str = r#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 0) "\01\00\00\00")
          (func (export "tocat_abi_version") (result i32) (i32.const 2))
          (func (export "tocat_outbox") (result i32) (i32.const 0))
          (func (export "tocat_alloc") (param i32) (result i32) (i32.const 64))
          (func (export "tocat_on_bytes") (param i32 i32)))
    "#;

    /// A guest that emits two units from one chunk: the fixed bytes at 128,
    /// split at offset 3.
    const FRAMED: &str = r#"
        (module
          (memory (export "memory") 1)
          ;; emit = 2 (buffered), bytes at 128 len 6, bounds at 120 len 1
          (data (i32.const 0) "\02\00\00\00\80\00\00\00\06\00\00\00\78\00\00\00\01\00\00\00")
          (data (i32.const 120) "\03\00\00\00")
          (data (i32.const 128) "abcdef")
          (func (export "tocat_abi_version") (result i32) (i32.const 2))
          (func (export "tocat_outbox") (result i32) (i32.const 0))
          (func (export "tocat_alloc") (param i32) (result i32) (i32.const 256))
          (func (export "tocat_on_bytes") (param i32 i32)))
    "#;

    /// A guest that spins. The relay's protection against this is fuel, not
    /// hope.
    const SPINS: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "tocat_abi_version") (result i32) (i32.const 2))
          (func (export "tocat_outbox") (result i32) (i32.const 0))
          (func (export "tocat_alloc") (param i32) (result i32) (i32.const 64))
          (func (export "tocat_on_bytes") (param i32 i32)
            (loop $forever (br $forever))))
    "#;

    /// A guest built against WASI, which is exactly what must not load.
    const IMPORTS: &str = r#"
        (module
          (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "tocat_abi_version") (result i32) (i32.const 2))
          (func (export "tocat_outbox") (result i32) (i32.const 0))
          (func (export "tocat_alloc") (param i32) (result i32) (i32.const 64))
          (func (export "tocat_on_bytes") (param i32 i32)))
    "#;

    /// A guest that refuses every chunk, which is what returning 0 from
    /// `tocat_alloc` means.
    const REFUSES: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "tocat_abi_version") (result i32) (i32.const 2))
          (func (export "tocat_outbox") (result i32) (i32.const 0))
          (func (export "tocat_alloc") (param i32) (result i32) (i32.const 0))
          (func (export "tocat_on_bytes") (param i32 i32)))
    "#;

    #[derive(Default)]
    struct Recorder {
        logs: Vec<String>,
        halts: Vec<String>,
    }

    impl EffectSink for Recorder {
        fn write(&mut self, _channel: ChannelId, _bytes: &[u8]) {}
        fn log(&mut self, _level: LogLevel, _stage: &str, message: &str) {
            self.logs.push(message.to_string());
        }
        fn halt(&mut self, _stage: &str, reason: &str) {
            self.halts.push(reason.to_string());
        }
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "tcp://a", "STDIO")
    }

    /// Straight to the guest, since these are about the ABI rather than about
    /// the entry: nothing here goes through `WasmFactory::build`, so nothing
    /// here needs a `BuildCtx` or a host to open channels against.
    fn guest(wat: &str, fuel: u64) -> Result<Wasm> {
        let pre = engine::compile(wat)?;
        let guest = Guest::new(&pre, 1 << 20, fuel, b"{}")?;

        Ok(Wasm {
            tick: guest.tick_interval(),
            boundaries: guest.boundaries(),
            needs: guest.needs(),
            guest,
        })
    }

    fn feed(plugin: &mut Wasm, sink: &mut Recorder, input: &[u8]) -> Result<Emission> {
        let meta = meta();
        let mut emission = Emission::new();
        {
            let mut ctx = Ctx::new(&meta, NAME, input, &mut emission, sink);
            plugin.on_bytes(&mut ctx, input)?;
        }

        Ok(emission)
    }

    #[test]
    fn passthrough_costs_nothing() {
        let mut plugin = guest(PASSTHROUGH, default_fuel()).expect("build");
        let mut sink = Recorder::default();

        let emission = feed(&mut plugin, &mut sink, b"ping").expect("on_bytes");

        assert_eq!(emission.emit(), Emit::Passthrough);
        assert!(emission.bytes().is_empty());
    }

    #[test]
    fn a_guest_frames_its_own_units() {
        let mut plugin = guest(FRAMED, default_fuel()).expect("build");
        let mut sink = Recorder::default();

        let emission = feed(&mut plugin, &mut sink, b"ignored").expect("on_bytes");

        assert_eq!(emission.emit(), Emit::Buffered);
        assert_eq!(emission.bytes(), b"abcdef");
        assert_eq!(emission.bounds().to_vec(), vec![3usize]);
    }

    #[test]
    fn a_runaway_guest_fails_the_path_rather_than_hanging_it() {
        let mut plugin = guest(SPINS, 100_000).expect("build");
        let mut sink = Recorder::default();

        let error = feed(&mut plugin, &mut sink, b"ping").expect_err("should trap");
        assert!(error.to_string().contains("tocat_on_bytes"));
    }

    #[test]
    fn a_guest_that_imports_anything_is_refused() {
        let error = guest(IMPORTS, default_fuel()).expect_err("should refuse");
        let message = error.to_string();

        assert!(message.contains("wasi_snapshot_preview1"));
        assert!(message.contains("fd_write"));
    }

    #[test]
    fn a_refused_chunk_is_an_error_rather_than_a_write_to_address_zero() {
        let mut plugin = guest(REFUSES, default_fuel()).expect("build");
        let mut sink = Recorder::default();

        let error = feed(&mut plugin, &mut sink, b"ping").expect_err("should refuse");
        assert!(error.to_string().contains("refused"));
    }

    #[test]
    fn config_defaults_are_the_documented_ones() {
        let config: WasmConfig =
            serde_json::from_value(json!({ "module": "guest.wasm" })).expect("config");

        assert_eq!(config.fuel, default_fuel());
        assert_eq!(config.memory_max.bytes(), 64 * 1024 * 1024);
        assert!(config.config.is_object());
    }
}
