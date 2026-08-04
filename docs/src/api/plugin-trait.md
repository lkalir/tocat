# The Plugin trait

```rust,ignore
pub trait Plugin: Send {
    fn name(&self) -> &str;

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()>;

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> { Ok(()) }

    fn tick_interval(&self) -> Option<Duration> { None }
    fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> { Ok(()) }

    fn datagram_safe(&self) -> bool { false }
}
```

Instances are per direction and per connection. Under `fork` that means a fresh set for every accepted client, and a `both` entry means two instances,
one per path, so per-direction state (byte offsets, codec state) never leaks across paths.

`input` is the same slice as `ctx.input()`; it is passed separately because it is the hot argument.

## Deciding what happens to the chunk

Every call must say what becomes of the bytes. `Ctx` offers three answers and one qualifier:

| Call                      | Meaning                                                                                     |
|---------------------------|-----------------------------------------------------------------------------------------------|
| `ctx.pass_through()`      | Forward the input unchanged, without copying it. The next stage gets the same slice          |
| `ctx.forward(bytes)`      | Emit different bytes. Appends, so two calls emit both, in order, as one unit                 |
| `ctx.drop_chunk()`        | Swallow it. Emitting nothing does the same thing; this exists so a filter can state the intent |
| `ctx.boundary()`          | End the current unit. See [Units and boundaries](units.md)                                   |

Passthrough is the fast path and costs nothing: the pipeline hands the original read buffer down the chain and ultimately to the socket. Mixing the two
is allowed and does the obvious thing, at the price of materialising the input: `pass_through` followed by `forward` copies the input first so the
order is preserved.

Forwarding nothing is a legitimate steady state, not an error. `block` does it for every chunk that does not fill a block.

What a stage must not do is block. No socket, no file, no sleep, no lock held across a call. Everything that needs the outside world is requested
through `ctx` and performed by the host: see [Effects and channels](effects.md).

An error returned from any call fails that path rather than being logged and ignored, on the grounds that a stage which cannot process the bytes it was
given has produced an incomplete or wrong stream. Use `PluginError::config` for anything wrong with the entry and `PluginError::runtime` for anything
that goes wrong mid-stream.

## End of stream

`on_eof` is the last call a stage receives, and the last chance to emit: a codec writes its epilogue here, a buffering stage releases what it holds.
Anything emitted continues downstream through the stages *below*, which then receive their own `on_eof` in turn, so a chain of buffering stages drains
from the top down in one cascade. `ctx.input()` is empty, so there is nothing to pass through.

End of stream is a normal event rather than an error, and it can also arrive early: `ctx.halt()` is how a stage ends a transfer deliberately (this is
all `limit` does), and the path then closes down exactly as it would have anyway.

Some paths never reach it. A datagram source has no end of stream, and neither does a held [`pipe`](../guide/endpoints/pipe.md). A stage whose only
output happens at `on_eof` therefore produces nothing at all on such a path, which is why `rate` reports on a timer as well as in a summary.

## Datagram safety

`datagram_safe` is what the host checks when the destination on this path is a datagram endpoint, so that it can warn about a stage that may split,
merge or invent messages. It defaults to false, which is the safe answer for a stage that has not thought about it, including any plugin loaded from
outside the binary.

A pure observer that only calls `pass_through` can say true. Anything holding bytes across calls, emitting on a tick, or reframing what it was given
should not, even when doing so is the whole point of the stage: the host warns and relays anyway, so the honest answer costs nothing.
