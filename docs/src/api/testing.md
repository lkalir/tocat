# Testing a stage

Because a stage is synchronous and reaches nothing outside itself, a test is a plain unit test: build it, feed it chunks, assert on what came out and
on what it asked for. No runtime, no sockets, no temporary files. Every plugin in the tree is tested this way.

Two small doubles are all it takes. A `HostBuilder` to hand out channel ids at build time:

```rust,ignore
struct CountingHost(u32);

impl HostBuilder for CountingHost {
    fn open_channel(&mut self, _target: ChannelTarget) -> Result<ChannelId> {
        let id = ChannelId(self.0);
        self.0 += 1;
        Ok(id)
    }
}
```

and an `EffectSink` that records whatever the stage asks for. Only `write` and `log` are required; `pace` and `halt` default to doing nothing, so
implement them when the stage under test uses them.

```rust,ignore
#[derive(Default)]
struct Recorder(Vec<Vec<u8>>);

impl EffectSink for Recorder {
    fn write(&mut self, _channel: ChannelId, bytes: &[u8]) { self.0.push(bytes.to_vec()); }
    fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}
}
```

Building goes through the factory, so the config path is exercised too:

```rust,ignore
let map = json!({ "format": "hex" }).as_object().unwrap().clone();
let meta = PipelineMeta::new(Direction::SourceToSink, "tcp://a", "STDIO");
let stage = StageInfo { index: 0, total: 1, name: "audit", upstream: "tcp://a", downstream: "STDIO" };
let mut host = CountingHost(0);
let mut ctx = BuildCtx::new("tee", &map, &meta, stage, &mut host);
let mut plugin = match TeeFactory.build(&mut ctx)? { Stage::Filter(p) => p, _ => unreachable!() };
```

Driving it means constructing an `Emission` and a `Ctx` around each chunk, then reading the emission back:

```rust,ignore
let mut emission = Emission::new();
{
    let mut ctx = Ctx::new(&meta, "audit", input, &mut emission, &mut recorder);
    plugin.on_bytes(&mut ctx, input)?;
}

assert_eq!(emission.emit(), Emit::Passthrough);
assert_eq!(emission.bytes(), b"");
assert_eq!(emission.bounds(), &[] as &[usize]);
```

`Emission` exposes exactly what the host reads: `bytes()` is everything emitted concatenated, `bounds()` is the framing (empty means one unit),
`emit()` is what the stage decided (`Pending`, `Passthrough` or `Buffered`), and `rearm_requested()` says whether it asked for its schedule to be
restarted. `reset()` readies it for the next call while keeping the allocations, which is what the host does between chunks.

Three assertions are worth making by habit:

- **A pure observer must never materialise the payload.** `emit() == Passthrough` and `bytes().is_empty()` together are what prove `tee` and `rate` are
  free.
- **Framing is what you meant.** Assert on `bounds()`, not just on the concatenated bytes, or a stage that forgot its boundaries looks correct.
- **The end matters.** Call `on_eof` and assert on what it flushed, especially for anything holding bytes or writing an epilogue.

Ticks are just as direct: call `on_tick` with a `Ctx` whose input is empty, and check both what was emitted and `rearm_requested()`. Nothing has to
wait for a real interval to pass.
