# tocat-api

The plugin contract for [tocat](https://crates.io/crates/tocat), a
socat-inspired relay. Depend on this to write a plugin; depend on `tocat` to run
one.

```console
$ cargo add tocat-api
```

A plugin is a synchronous byte transformer. It is handed a chunk and decides
what to forward, and anything that touches the outside world (writing a dump
file, emitting a log line, waiting, stopping the transfer) is queued as an
effect for the host to perform rather than done in place:

```rust,ignore
impl Plugin for Upper {
    fn name(&self) -> &str { "upper" }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        ctx.forward(&input.to_ascii_uppercase());
        Ok(())
    }
}
```

Three properties follow from that split, and they are the reason for it: a stage
is testable without a runtime, nothing in it can stall a reactor thread, and the
same shape compiles to a WebAssembly guest, which cannot own a socket or a clock
at all.

This crate depends on serde and nothing else. No tokio, no I/O, no relay.

## Documentation

The plugin API section of the book, under `docs/` in the
[repository](https://github.com/lkalir/tocat), covers the trait, units and
boundaries, ticks, effects and testing.

## License

MIT or Apache-2.0, at your option.
