# `limit` - end the transfer after N bytes or N packets

Counts what passes its own position and, on reaching the limit, asks the host to
stop reading.

```console
$ tocat tcp:host:9000 limit,bytes=10MiB file:sample.bin
$ tocat udp-listen:9000 limit,packets=100 file:capture.bin
$ tocat -f tcp-listen:9000,fork -t tcp:backend:8080 -p 'limit,bytes=1M,direction=source'
```

| Option          | Description                                                                                                            |
| --------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `bytes=SIZE`    | How many bytes to let past. Takes the usual size suffixes, so `1M` is 1 MiB. Aliases: `max`, `size`                    |
| `packets=N`     | How many chunks to let past. Takes the same suffixes as `bytes`, so `1k` is 1024 chunks. Aliases: `chunks`, `messages` |
| `at-limit=MODE` | What to do with the chunk that crosses a `bytes` limit: `drop`, `exact` (the default) or `overshoot`                   |

Give one of `bytes` and `packets`. An entry with both is refused, and an entry
with neither has nothing to count. To stop on whichever comes first, write two
entries:

```console
$ tocat udp-listen:9000 limit,packets=1000 limit,bytes=10MiB file:capture.bin
```

`at-limit` belongs to `bytes` and is refused alongside `packets`: a packet is
counted only once it has passed whole, so no packet is ever cut.

Reaching the limit is upstream end of stream arriving early, not an error: bytes
already emitted are written, the stages below get their end of stream, sinks are
flushed and closed, and tocat exits successfully. The stop is logged at info:

```
limit: limit of 10MiB reached at 10MiB
limit: limit of 100 packets reached
```

A byte limit names both the limit and where the transfer actually ended, which
differ under `overshoot`. A packet limit stops on the packet that reaches the
count, so there is only one number to report, and it says `packets` whichever of
the three spellings was written.

Anything still arriving after that (a chunk already in flight from a stage
above) is dropped rather than announcing the limit again.

`bytes=1MiB` is a megabyte from the source. `direction=both` builds one instance
per path, each with its own budget, so it is a megabyte in each direction rather
than a megabyte between them, and `packets=100` the same way. Position matters
too: before a `compress` stage it caps the payload, after it caps the wire.

## The crossing chunk

Exactly one chunk straddles a byte limit, and there are three things to do with
it.

| `at-limit`  | The crossing chunk | Guarantee        |
| ----------- | ------------------ | ---------------- |
| `drop`      | discarded whole    | at most `bytes`  |
| `exact`     | split at the limit | exactly `bytes`  |
| `overshoot` | forwarded whole    | at least `bytes` |

`exact` is the default and is what a byte count usually means. `drop` is the
hard ceiling: never put more than this many bytes into that file, that pipe,
that quota. `overshoot` is the one to reach for on a datagram path, where a
limit landing mid-message leaves a real choice: dropping throws away a message
already received on a transfer that is ending anyway, while overshooting
delivers it whole and then stops.

Splitting is also the only thing here that is unsafe on a datagram path, so the
stage reports itself safe under `drop` and `overshoot` and unsafe under `exact`.
Half a datagram is a corrupt message rather than a short read. A packet limit
splits nothing and is safe on any path.

## What counts as a packet

One chunk, meaning one delivery from whatever sits above the stage.

On a datagram path that is exactly one message, which is what the option is for:
`packets=100` on a UDP source takes a hundred datagrams and stops. Put the stage
below an [`unframe`](frame.md) to get the same thing on a byte stream, where the
messages are the ones `unframe` found.

Everywhere else a chunk is one read, sized by [`buffer-size`](../buffers.md) and
by when the peer's bytes happened to arrive. Two runs of the same transfer need
not stop in the same place. That is still a well defined thing to ask for, so
tocat counts it rather than refusing: reads are a usable proxy when what you
want is "let a hundred deliveries through and stop", and above a `process` stage
they are the child's writes. It is not a message count, and nothing in the
config will pretend otherwise.

A count is written the way sizes are, suffixes and all, so `packets=1k` is 1024
chunks rather than 1000. It is reported as the plain number it is: a limit of
`1k` logs as `limit of 1024 packets reached`.

Nothing is copied under either limit. A chunk below a byte limit is passed
through untouched and only the crossing one is ever materialised; a packet limit
materialises none at all.
