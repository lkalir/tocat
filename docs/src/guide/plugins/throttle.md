# `throttle` - constrict bandwidth

Holds the path to a ceiling by slowing the reader down.

```console
$ tocat file:big.iso throttle,rate=256k tcp:host:9000
$ tocat -f tcp-listen:9000,fork -t tcp:backend:8080 -p 'throttle,rate=1MiB,burst=4MiB'
```

| Option       | Description                                                                                                                  |
| ------------ | ---------------------------------------------------------------------------------------------------------------------------- |
| `rate=SIZE`  | The ceiling, in bytes per second. Required, and must be greater than zero. Aliases: `bandwidth`, `bps`                       |
| `burst=SIZE` | How much unused allowance may accumulate, so an idle path can resume at full speed briefly. Defaults to one second of `rate` |

Nothing is buffered. Every chunk passes through untouched and the stage asks the
relay to wait before reading again, so there is no queue to grow when the source
outruns the limit. It also throttles the right end: a read that does not happen
leaves the receive buffer full, which closes the TCP window and slows the sender
at source. Buffering here would let the sender keep sending at full speed into
memory that has to go somewhere, which is a queue, not a limit.

The allowance is a token bucket. It accrues at `rate` bytes per second up to
`burst`, and each chunk spends its own length. Spending is not capped, so a
chunk larger than the bucket is paid for with a proportionally longer wait
rather than being split, which is what keeps the stage safe on a datagram path:
a message goes out as the message it arrived as, just later. The bucket starts
full, so a path that has just opened may move `burst` bytes before the ceiling
first bites.

The wait lands between reads, so the stall comes in units of chunks. At the 256
KiB default buffer, `rate=64k` is four seconds of silence followed by 256 KiB at
once. The average is right either way, but for smooth pacing give the relay a
buffer at or below the per-second rate (`-b 64k` here).

The wait is also applied after the chunk in hand has been written, so nothing is
held hostage by it. Where several stages on one path ask to wait, the longest
request wins rather than the sum: two stages each asking for a second are
satisfied by one second.

As with [`limit`](limit.md), instances are per path, so `rate=1MiB` is a
megabyte per second each way rather than between them. To stop a stream rather
than slow it, use `limit`; `rate=0` is rejected.
