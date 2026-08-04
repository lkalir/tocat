# `limit` - end the transfer after N bytes

Counts bytes past its own position and, on reaching the limit, asks the host to stop reading.

```console
$ tocat tcp:host:9000 limit,bytes=10MiB file:sample.bin
$ tocat -f tcp-listen:9000,fork -t tcp:backend:8080 -p 'limit,bytes=1M,direction=source'
```

| Option           | Description                                                                                                   |
|------------------|---------------------------------------------------------------------------------------------------------------|
| `bytes=SIZE`     | How many bytes to let past. Required. Takes the usual size suffixes, so `1M` is 1 MiB. Aliases: `max`, `size` |
| `at-limit=MODE`  | What to do with the chunk that crosses the limit: `drop`, `exact` (the default) or `overshoot`                |

Reaching the limit is upstream end of stream arriving early, not an error: bytes already emitted are written, the stages below get their end of stream,
sinks are flushed and closed, and tocat exits successfully. The stop is logged at info, naming the limit and where the transfer actually ended, which
differ under `overshoot`:

```
limit: limit of 10MiB reached at 10MiB
```

Anything still arriving after that (a chunk already in flight from a stage above) is dropped rather than announcing the limit again.

The default `direction=both` builds one instance per path, each with its own budget, so `bytes=1MiB` means a megabyte in each direction rather than a
megabyte between them. Position matters too: before a `compress` stage it caps the payload, after it caps the wire.

## The crossing chunk

Exactly one chunk straddles the limit, and there are three things to do with it.

| `at-limit`  | The crossing chunk | Guarantee        |
|-------------|--------------------|------------------|
| `drop`      | discarded whole    | at most `bytes`  |
| `exact`     | split at the limit | exactly `bytes`  |
| `overshoot` | forwarded whole    | at least `bytes` |

`exact` is the default and is what a byte count usually means. `drop` is the hard ceiling: never put more than this many bytes into that file, that
pipe, that quota. `overshoot` is the one to reach for on a datagram path, where a limit landing mid-message leaves a real choice: dropping throws away
a message already received on a transfer that is ending anyway, while overshooting delivers it whole and then stops.

Splitting is also the only thing here that is unsafe on a datagram path, so the stage reports itself safe under `drop` and `overshoot` and unsafe under
`exact`. Half a datagram is a corrupt message rather than a short read.

Below the limit nothing is copied: the chunk is passed through untouched, and only the crossing one is ever materialised.
