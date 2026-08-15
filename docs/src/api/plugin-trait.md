# The Plugin trait

```rust,ignore
pub trait Plugin: Send {
    fn name(&self) -> &str;

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()>;

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> { Ok(()) }

    fn tick_interval(&self) -> Option<Duration> { None }
    fn on_tick(&mut self, ctx: &mut Ctx<'_>) -> Result<()> { Ok(()) }

    fn boundaries(&self) -> Boundaries { Boundaries::Fuse }
    fn needs(&self) -> Needs { Needs::Nothing }
}
```

Instances are per direction and per connection. Under `fork` that means a fresh
set for every accepted client, and a `both` entry means two instances, one per
path, so per-direction state (byte offsets, codec state) never leaks across
paths.

`input` is the same slice as `ctx.input()`; it is passed separately because it
is the hot argument.

## Deciding what happens to the chunk

Every call must say what becomes of the bytes. `Ctx` offers three answers and
one qualifier:

| Call                 | Meaning                                                                                        |
| -------------------- | ---------------------------------------------------------------------------------------------- |
| `ctx.pass_through()` | Forward the input unchanged, without copying it. The next stage gets the same slice            |
| `ctx.forward(bytes)` | Emit different bytes. Appends, so two calls emit both, in order, as one unit                   |
| `ctx.drop_chunk()`   | Swallow it. Emitting nothing does the same thing; this exists so a filter can state the intent |
| `ctx.boundary()`     | End the current unit. See [Units and boundaries](units.md)                                     |

Passthrough is the fast path and costs nothing: the pipeline hands the original
read buffer down the chain and ultimately to the socket. Mixing the two is
allowed and does the obvious thing, at the price of materialising the input:
`pass_through` followed by `forward` copies the input first so the order is
preserved.

Forwarding nothing is a legitimate steady state, not an error. `block` does it
for every chunk that does not fill a block.

What a stage must not do is block. No socket, no file, no sleep, no lock held
across a call. Everything that needs the outside world is requested through
`ctx` and performed by the host: see [Effects and channels](effects.md).

An error returned from any call fails that path rather than being logged and
ignored, on the grounds that a stage which cannot process the bytes it was given
has produced an incomplete or wrong stream. Use `PluginError::config` for
anything wrong with the entry and `PluginError::runtime` for anything that goes
wrong mid-stream.

## End of stream

`on_eof` is the last call a stage receives, and the last chance to emit: a codec
writes its epilogue here, a buffering stage releases what it holds. Anything
emitted continues downstream through the stages *below*, which then receive
their own `on_eof` in turn, so a chain of buffering stages drains from the top
down in one cascade. `ctx.input()` is empty, so there is nothing to pass
through.

End of stream is a normal event rather than an error, and it can also arrive
early: `ctx.halt()` is how a stage ends a transfer deliberately (this is all
`limit` does), and the path then closes down exactly as it would have anyway.

Some paths never reach it. A datagram source has no end of stream, and neither
does a held [`pipe`](../guide/endpoints/pipe.md). A stage whose only output
happens at `on_eof` therefore produces nothing at all on such a path, which is
why `rate` reports on a timer as well as in a summary.

## Boundaries

`boundaries` says what the stage does to the messages passing through it, and
`needs` says what it requires of the path it was placed on. Both are read once
after `build` and never on the per-chunk path.

| Answer                 | Means                                                           | Who says it                                      |
| ---------------------- | --------------------------------------------------------------- | ------------------------------------------------ |
| `Boundaries::Fuse`     | The units above do not reach the stage below                    | The default. `block`, `compress`, `process`      |
| `Boundaries::Preserve` | One unit in, one unit out                                       | Every observer, and any codec rewriting in place |
| `Boundaries::Seal`     | As preserve, and the boundary goes into the payload as well     | `frame`, and nothing else                        |
| `Boundaries::Split`    | The units below are read out of the bytes rather than inherited | `unframe`, and nothing else                      |

`Fuse` is the default because it claims nothing, which is the safe answer for a
stage that has not thought about it, including any plugin loaded from outside
the binary. A pure observer that only calls `pass_through` can say `Preserve`.
Anything holding bytes across calls, emitting on a tick, or reframing what it
was given should not, even when doing so is the whole point of the stage: the
host warns and relays anyway, so the honest answer costs nothing.

The difference between the two methods is the difference between a warning and
an error. `boundaries` is advisory, because rewriting the message stream is
sometimes exactly what was asked for. `needs` is not: a stage saying
`Needs::Upstream` cannot read what arrives unless every call carries one whole
message, and one saying `Needs::Downstream` cannot have what it wrote read back
unless the units it emitted survive. Neither is a matter of taste, so an unmet
requirement fails the build.

The two sides are separate because the stages that want them want opposite ones.
A stage that seals a message and appends a tag makes its own boundaries and
needs them to survive downwards; the stage that verifies and strips that tag
needs whole messages from above and does not care what happens below it.

The host answers a requirement by walking away from the stage until something
settles it. A datagram endpoint on that side supplies boundaries, a `frame`
below satisfies a downstream requirement however many stages fuse under it, an
`unframe` above satisfies an upstream one, and the first stage that carries
neither is named as the cause.
