//! `block`: cut a path into fixed-size records.
//!
//! Holds bytes back until it has `size` of them, then emits them as one unit,
//! which is `dd`'s `obs` with the pipeline's framing behind it. On a byte sink
//! that only changes where the writes fall; on a datagram sink or across a
//! detached boundary each block is delivered on its own, which is the point of
//! declaring a boundary rather than merely accumulating.
//!
//! `flush` bounds how long a partial block waits. Without it a short block
//! sits until end of stream, which is what a device wants and what an
//! interactive stream does not. The bound is a real one: the stage restarts
//! its own schedule when a block starts filling, so the interval is measured
//! from the first byte held rather than from wherever the host's cadence
//! happened to be.

use serde::{Deserialize, Serialize};
use tocat_api::{
    BuildCtx, ByteSize, Ctx, Interval, Plugin, PluginError, PluginFactory, Result, Stage,
};

pub const BLOCK: &str = "block";

const DEFAULT_BYTESIZE: usize = 4096;

#[derive(Debug, Clone, Deserialize, Serialize)]
// `default` on the container fills anything missing from the `Default` impl
// below, so the two cannot drift the way a per-field default can.
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct BlockConfig {
    /// Bytes to accumulate before emitting. Must be greater than zero.
    pub size: ByteSize,

    /// How long a partial block may wait. `None` waits indefinitely, so a
    /// short block is held until end of stream; `0` emits whatever is in hand
    /// on every write, which caps latency at the cost of short blocks.
    ///
    /// A full block never waits for this: it goes out as soon as it fills.
    pub flush: Option<Interval>,

    /// Pad a short block out to `size` with zero bytes.
    ///
    /// Only short blocks are affected, which in practice means the block at
    /// end of stream and any cut short by `flush`. A full block is already
    /// `size` bytes.
    pub pad: bool,
}

impl Default for BlockConfig {
    fn default() -> Self {
        Self {
            size: ByteSize(DEFAULT_BYTESIZE),
            flush: None,
            pad: false,
        }
    }
}

pub struct Block {
    /// Bytes in hand, never more than `size` of them.
    buf: Vec<u8>,
    /// The block size. Held separately from the buffer's capacity, which the
    /// allocator is free to round up and which is therefore not a number this
    /// stage may pad to or measure fullness against.
    size: usize,
    flush: Option<std::time::Duration>,
    pad: bool,
}

impl Block {
    /// Whether a partial block goes out on every write rather than waiting.
    fn immediate(&self) -> bool {
        self.flush.is_some_and(|dur| dur.is_zero())
    }

    /// Emit what is in hand as one block, if there is anything.
    ///
    /// The empty check is what keeps an idle stream quiet: `on_tick` arrives
    /// whether or not bytes are moving, and with `pad` set an unguarded emit
    /// would put a block of zeroes on the wire once per interval forever.
    fn emit(&mut self, ctx: &mut Ctx<'_>) {
        if self.buf.is_empty() {
            return;
        }

        if self.pad {
            self.buf.resize(self.size, 0);
        }

        ctx.forward(&self.buf);
        ctx.boundary();
        self.buf.clear();
    }
}

impl Plugin for Block {
    fn name(&self) -> &str {
        BLOCK
    }

    fn tick_interval(&self) -> Option<std::time::Duration> {
        // Zero means "every write", which `on_bytes` handles and which wants
        // no timer at all.
        self.flush.filter(|dur| !dur.is_zero())
    }

    fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        self.emit(ctx);
        Ok(())
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, mut input: &[u8]) -> Result<()> {
        // A chunk off the wire is usually several blocks long, so this is a
        // loop rather than a single fill: anything past the first block would
        // otherwise be dropped.
        while !input.is_empty() {
            // `flush` bounds how long these bytes wait, so the clock starts
            // where the block does. Without this a tick that came due while
            // the previous block was going out would fire the instant this one
            // began and cut it short, which is how a 2.5-block write ends up
            // as two blocks and a runt.
            if self.buf.is_empty() {
                ctx.rearm();
            }

            let n = input.len().min(self.size - self.buf.len());

            self.buf.extend_from_slice(&input[..n]);
            input = &input[n..];

            if self.buf.len() == self.size {
                self.emit(ctx);
            }
        }

        if self.immediate() {
            self.emit(ctx);
        }

        Ok(())
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        self.emit(ctx);
        Ok(())
    }

    // `datagram_safe` is left at its default of false, deliberately: this
    // stage holds bytes across calls and the boundaries it emits are its own
    // rather than the ones the peer sent, so on a datagram path it rewrites
    // the message stream. That is sometimes exactly what is wanted (`block` at
    // the MTU is a reasonable thing to ask for) which is why the host warns
    // rather than refuses.
}

pub struct BlockFactory;

impl PluginFactory for BlockFactory {
    fn name(&self) -> &str {
        BLOCK
    }

    fn description(&self) -> &str {
        "Accumulate bytes into fixed-size blocks"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: BlockConfig = ctx.config()?;

        let size = config.size.bytes();

        // A block with no room in it would drop every byte handed to it, so
        // this is rejected here rather than discovered on the wire.
        if size == 0 {
            return Err(PluginError::config(
                self.name(),
                "size must be greater than zero",
            ));
        }

        // Fallible, because the size comes from config and nothing stops
        // someone asking for a block larger than memory.
        let mut buf = Vec::new();
        buf.try_reserve_exact(size).map_err(|e| {
            PluginError::runtime(self.name(), format!("could not reserve {size} bytes: {e}"))
        })?;

        Ok(Stage::filter(Block {
            buf,
            size,
            flush: config.flush.map(Interval::duration),
            pad: config.pad,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};
    use tocat_api::{
        ChannelId, ChannelTarget, Direction, EffectSink, Emission, Emit, HostBuilder, LogLevel,
        PipelineMeta, StageInfo,
    };

    use super::*;

    struct NoHost;

    impl HostBuilder for NoHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> Result<ChannelId> {
            unreachable!("block opens no side channels")
        }
    }

    struct Silent;

    impl EffectSink for Silent {
        fn write(&mut self, _channel: ChannelId, _bytes: &[u8]) {}

        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
    }

    fn build(value: Value) -> Result<Box<dyn Plugin>> {
        let meta = meta();
        let map: Map<String, Value> = match value {
            Value::Object(map) => map,
            other => unreachable!("config must be an object, got {other}"),
        };
        let mut host = NoHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: BLOCK,
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(BLOCK, &map, &meta, stage, &mut host);

        match BlockFactory.build(&mut ctx)? {
            Stage::Filter(plugin) => Ok(plugin),
            Stage::External(_) => unreachable!("block is a filter"),
        }
    }

    fn built(value: Value) -> Box<dyn Plugin> {
        build(value).expect("build")
    }

    /// The blocks one call emitted.
    ///
    /// The pipeline splits an emission on the boundaries a stage declared;
    /// here that is done by hand, so the tests can assert on blocks rather
    /// than on their concatenation. Asserting on the concatenation would pass
    /// just as happily if this stage fused every block into one, which is the
    /// bug the boundaries exist to prevent.
    fn units(emission: &Emission) -> Vec<Vec<u8>> {
        let bytes = emission.bytes();
        let mut start = 0;

        emission
            .bounds()
            .iter()
            .map(|&end| {
                let unit = bytes[start..end].to_vec();
                start = end;
                unit
            })
            .collect()
    }

    fn feed(plugin: &mut dyn Plugin, input: &[u8]) -> Vec<Vec<u8>> {
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;

        {
            let mut ctx = Ctx::new(&meta, BLOCK, input, &mut emission, &mut sink);
            plugin.on_bytes(&mut ctx, input).expect("on_bytes");
        }

        assert_ne!(
            emission.emit(),
            Emit::Passthrough,
            "block never passes bytes through",
        );

        units(&emission)
    }

    /// `on_eof` when `eof`, `on_tick` otherwise. Both arrive with no input.
    fn drive(plugin: &mut dyn Plugin, eof: bool) -> Vec<Vec<u8>> {
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;

        {
            let mut ctx = Ctx::new(&meta, BLOCK, &[], &mut emission, &mut sink);

            if eof {
                plugin.on_eof(&mut ctx).expect("on_eof");
            } else {
                plugin.on_tick(&mut ctx).expect("on_tick");
            }
        }

        units(&emission)
    }

    /// Whether a write asked the host to restart the flush clock.
    fn rearms(plugin: &mut dyn Plugin, input: &[u8]) -> bool {
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;

        {
            let mut ctx = Ctx::new(&meta, BLOCK, input, &mut emission, &mut sink);
            plugin.on_bytes(&mut ctx, input).expect("on_bytes");
        }

        emission.rearm_requested()
    }

    fn tick(plugin: &mut dyn Plugin) -> Vec<Vec<u8>> {
        drive(plugin, false)
    }

    fn finish(plugin: &mut dyn Plugin) -> Vec<Vec<u8>> {
        drive(plugin, true)
    }

    #[test]
    fn nothing_is_emitted_until_a_block_is_full() {
        let mut block = built(json!({"size": 4}));

        assert!(feed(block.as_mut(), b"abc").is_empty());
        assert_eq!(feed(block.as_mut(), b"d"), [b"abcd".to_vec()]);
    }

    /// The case a single fill would silently truncate: a chunk off the wire is
    /// routinely many blocks long.
    #[test]
    fn a_chunk_larger_than_a_block_becomes_several() {
        let mut block = built(json!({"size": 2}));

        assert_eq!(
            feed(block.as_mut(), b"abcdef"),
            [b"ab".to_vec(), b"cd".to_vec(), b"ef".to_vec()],
        );
    }

    /// Every byte has to come out, in order, however the writes are cut up.
    #[test]
    fn no_bytes_are_lost_across_uneven_writes() {
        let mut block = built(json!({"size": 4}));

        let mut seen: Vec<Vec<u8>> = Vec::new();

        for write in [&b"ab"[..], b"cdefghi", b"", b"jk", b"lmn"] {
            seen.extend(feed(block.as_mut(), write));
        }

        seen.extend(finish(block.as_mut()));

        assert_eq!(
            seen,
            [
                b"abcd".to_vec(),
                b"efgh".to_vec(),
                b"ijkl".to_vec(),
                b"mn".to_vec(),
            ],
        );
    }

    #[test]
    fn a_short_block_is_emitted_at_end_of_stream() {
        let mut block = built(json!({"size": 8}));

        assert!(feed(block.as_mut(), b"abc").is_empty());
        assert_eq!(finish(block.as_mut()), [b"abc".to_vec()]);
    }

    #[test]
    fn end_of_stream_on_an_empty_buffer_emits_nothing() {
        let mut block = built(json!({"size": 8}));

        assert!(finish(block.as_mut()).is_empty());
    }

    #[test]
    fn a_short_block_is_padded_when_asked() {
        let mut block = built(json!({"size": 8, "pad": true}));

        assert!(feed(block.as_mut(), b"abc").is_empty());
        assert_eq!(finish(block.as_mut()), [b"abc\0\0\0\0\0".to_vec()]);
    }

    #[test]
    fn a_full_block_is_unaffected_by_padding() {
        let mut block = built(json!({"size": 4, "pad": true}));

        assert_eq!(feed(block.as_mut(), b"abcd"), [b"abcd".to_vec()]);
    }

    /// The reason `emit` checks for an empty buffer. Without it a padded stage
    /// would put a block of zeroes on the wire on every tick of an idle
    /// stream, forever.
    #[test]
    fn a_tick_on_an_idle_stream_emits_nothing() {
        let mut block = built(json!({"size": 8, "pad": true, "flush": "1s"}));

        assert!(tick(block.as_mut()).is_empty());
        assert!(tick(block.as_mut()).is_empty());
    }

    #[test]
    fn a_tick_releases_a_partial_block() {
        let mut block = built(json!({"size": 8, "flush": "1s"}));

        assert!(feed(block.as_mut(), b"abc").is_empty());
        assert_eq!(tick(block.as_mut()), [b"abc".to_vec()]);
        assert!(tick(block.as_mut()).is_empty(), "and nothing twice");
    }

    /// The interval has to be measured from the first byte held rather than
    /// from wherever the host's cadence had reached, or a tick that came due
    /// while the previous block was going out cuts this one short the moment
    /// it starts.
    #[test]
    fn the_flush_clock_restarts_when_a_block_starts_filling() {
        let mut block = built(json!({"size": 8, "flush": "1s"}));

        assert!(rearms(block.as_mut(), b"abc"), "a fresh block starts it");
        assert!(!rearms(block.as_mut(), b"de"), "adding to one does not");
        assert!(
            rearms(block.as_mut(), b"fghijk"),
            "and filling one starts the clock for the next",
        );
    }

    /// A flush interval bounds how long a partial block waits. It must not
    /// also stop a full one going out as soon as it fills.
    #[test]
    fn a_flush_interval_does_not_hold_back_a_full_block() {
        let mut block = built(json!({"size": 4, "flush": "1h"}));

        assert_eq!(feed(block.as_mut(), b"abcd"), [b"abcd".to_vec()]);
    }

    #[test]
    fn a_zero_flush_emits_on_every_write() {
        let mut block = built(json!({"size": 4096, "flush": 0}));

        assert_eq!(feed(block.as_mut(), b"ab"), [b"ab".to_vec()]);
        assert_eq!(feed(block.as_mut(), b"cd"), [b"cd".to_vec()]);
    }

    /// Zero means "every write", which is answered in `on_bytes`. Asking the
    /// host for a timer with that period would spin it.
    #[test]
    fn only_a_nonzero_flush_asks_for_a_timer() {
        assert_eq!(built(json!({})).tick_interval(), None);
        assert_eq!(built(json!({"flush": 0})).tick_interval(), None);
        assert_eq!(
            built(json!({"flush": "30s"})).tick_interval(),
            Some(std::time::Duration::from_secs(30)),
        );
    }

    #[test]
    fn the_default_size_applies_when_none_is_given() {
        let mut block = built(json!({}));

        assert!(feed(block.as_mut(), &vec![0u8; DEFAULT_BYTESIZE - 1]).is_empty());
        assert_eq!(
            feed(block.as_mut(), b"!"),
            [vec![0u8; DEFAULT_BYTESIZE - 1]
                .into_iter()
                .chain(*b"!")
                .collect::<Vec<u8>>()]
        );
    }

    /// A block with no room in it would drop every byte handed to it.
    #[test]
    fn a_zero_size_is_rejected() {
        assert!(build(json!({"size": 0})).is_err());
    }

    /// Buffering across calls and inventing boundaries is exactly what a
    /// datagram path cannot have done to it silently.
    #[test]
    fn block_is_not_datagram_safe() {
        assert!(!built(json!({})).datagram_safe());
    }
}
