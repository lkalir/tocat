# Units and boundaries

Whatever a stage emits in one call is delivered as one unit, however many times it forwards: the pieces concatenate and travel together. A stage that
needs them kept apart calls `ctx.boundary()` between them, and each unit then becomes

- one write at a byte sink,
- one message at a datagram sink,
- one parcel across a `detach` boundary,
- and one `on_bytes` call at every stage below.

That is the difference between a stage that merely accumulates bytes and one that records them. `block` forwards its block and then calls `boundary`;
nothing else that ships with tocat calls it at all.

Unit counts multiply down a chain, which is why this is an explicit request rather than something inferred from a stage emitting more than once. A
stage that forwards a header and then a body has not asked for two writes and should not silently get them.

Two conveniences worth knowing. The trailing unit needs no boundary: whatever is forwarded after the last one is closed automatically, so no bytes can
be lost by forgetting. And a boundary with nothing forwarded since the last one is ignored, so a stage cannot emit an empty unit by accident.

## What it costs

Passing through stays free underneath a framing stage. A stage handed framed bytes is called once per unit, and one that hands every unit back
untouched copies nothing and still answers passthrough, exactly as it would on an unframed chunk. The copy starts at the first unit it rewrites, drops
or reframes, and even then the pipeline ping-pongs between two buffers it owns rather than allocating.

What does cost is the call count. Every stage below a boundary runs once per unit rather than once per chunk, so `block,size=512` against a 256 KiB
buffer runs the rest of the pipeline 512 times per read, and a `detach` below that turns each unit into its own task wakeup. Ask for boundaries when
the splits are the point, which is what a tape drive, a raw device, an `O_DIRECT` sink or a datagram peer needs, and not otherwise.

A byte sink is the one destination that does not pay: it has no framing of its own, so the whole emission goes out in one write. The peer cannot tell
one call from several and one syscall is cheaper.

## Where a unit shows up

The host sees an emission as `Emitted`, which is the bytes plus the offsets that frame them. An empty boundary list means one unit covering everything,
which is the shape of every chunk off the wire, so the common path allocates nothing and every destination can ignore units entirely.

## Boundaries and datagrams

A stage declares through `datagram_safe` whether it may sit on a path carrying messages. `boundary` is what makes the useful case sayable rather than
silent: a stage that emits several units emits several messages. That is still a rewrite of the peer's message stream rather than a preservation of it,
so a stage doing it should report false and let the host decide whether to warn.

Declaring the truth matters more than declaring safety. `block` is not boundary preserving and says so, and it is still the right stage to reach for
when one datagram per 1400 bytes is exactly what you want. See [The datagram model](../design/datagrams.md).
