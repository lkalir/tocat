# `block` - cut the stream into fixed-size records

Holds bytes back until it has a full block, then emits it as one unit.

```console
$ tocat file:stream.bin block,size=512,pad file:/dev/st0
$ tocat -f tcp-listen:9000,fork -t udp:collector:9999 -p 'block,size=1400,flush=250ms'
```

| Option       | Description                                                                                                           |
| ------------ | --------------------------------------------------------------------------------------------------------------------- |
| `size=SIZE`  | Bytes to accumulate before emitting. Must be greater than zero. Defaults to 4096                                      |
| `flush=TIME` | How long a partial block may wait. Absent it waits until end of stream; `0` emits on every write. Takes `500ms`, `2m` |
| `pad`        | Pad a short block out to `size` with zero bytes                                                                       |

This is `dd`'s `obs`, with the difference that a block is a *unit* rather than
merely an amount: each one is delivered on its own rather than being
concatenated with its neighbours. On a byte sink that only changes where the
writes fall, which is what a tape drive, a raw device or anything opened
`O_DIRECT` cares about. On a datagram sink each block becomes one message, and
across a `detach` boundary each block becomes one parcel. See
[Units and boundaries](../../api/units.md) for the mechanism.

A chunk off the wire is usually several blocks long, so one call can emit
several, and a full block never waits: it goes out the moment it fills. `flush`
bounds the short one, and the bound is real rather than approximate, because the
stage restarts its own schedule when a block starts filling. The interval is
therefore measured from the first byte held, not from wherever a shared cadence
happened to be, so `flush=250ms` means a byte waits at most 250ms and not "until
the next quarter-second boundary that comes round".

`flush=0` is not a timer at all: it emits whatever is in hand on every write,
and builds no schedule. Without `flush` there is no timer either, and a short
block waits for end of stream, which is what a device wants and an interactive
stream does not.

`pad` only ever affects a short block, which in practice means the last one and
any cut short by `flush`. A full block is already `size` bytes. An idle path
stays quiet: a tick with nothing in hand emits nothing, which is what stops
`pad` putting a block of zeroes on the wire once per interval forever.

Framing costs something, so reach for this only when the splits are the point.
Every stage below a `block` is called once per unit rather than once per chunk,
so `size=512` against a 256 KiB buffer runs the rest of the pipeline 512 times
per read. `detach` on a stage below one is worse still, since each block then
costs its own task wakeup.

Not safe on a datagram path: this stage holds bytes across calls and the
boundaries it emits are its own rather than the ones the peer sent. That is
sometimes exactly what you want (`block,size=1400` into a UDP sink is one
datagram per block), so it warns rather than refusing.

The block buffer is reserved at startup and the reservation is fallible, so
`size=64GiB` is a startup error rather than an allocation failure later.
