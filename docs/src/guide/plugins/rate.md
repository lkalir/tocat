# `rate` - measure throughput

Reports how fast bytes are moving past this point on the path. Like [`tee`](tee.md) it never touches the payload, so it can go anywhere in a chain,
including on a datagram path.

```console
$ tocat -f tcp-listen:9000,fork -t tcp:backend:8080 -p 'rate,interval=30s'
$ tocat file:big.iso 'rate,as=plain' compress 'rate,as=wire' tcp:relay:9000
```

Each instance measures the traffic at its own position, so the two reports are the payload before compression and the bytes actually going on the wire.

| Option          | Description                                                                                                                      |
|-----------------|----------------------------------------------------------------------------------------------------------------------------------|
| `interval=TIME` | How often to report. Default `5s`. `0` reports only at end of stream                                                             |
| `unit=bits`     | Report decimal multiples of bits (`81.6Mbit/s`) rather than binary multiples of bytes (`10.2MiB/s`). Also `unit=bytes`, the default |
| `summary=false` | Suppress the total, average and peak line at end of stream                                                                       |
| `level=LEVEL`   | Level the reports are logged at: `trace`, `debug`, `info` (the default) or `warn`                                                |
| `file=PATH`     | Write the samples there as CSV instead of logging them. The summary is still logged                                              |
| `append=false`  | Truncate an existing sample file rather than appending to it                                                                     |

`interval` takes a plain number of seconds (`10`, `0.5`) or a suffixed string (`500ms`, `30s`, `2m`). This is the one place a fractional value is
accepted, which is why the suffixes here are only `ms`, `s` and `m` rather than the full duration grammar.

Reports are logs, so they follow the log sinks and the log level. Every record a stage emits is logged under the `plugin` target with a `stage` field,
so `RUST_LOG=plugin=info` selects plugin output and nothing else. The messages read:

```
10.2MiB/s (51.2MiB in 5.0s), 1.23GiB total
stalled, nothing in 5.0s, 1.23GiB total
transferred 1.23GiB in 0:02:04 (10.1MiB/s average, 11.8MiB/s peak)
```

The peak is the fastest window, so it appears only when there were windows to compare, which means not under `interval=0`.

With `file=` each interval writes one CSV row instead, with no header:

```
stage,elapsed,total,window,rate
```

`elapsed` is seconds since the first byte, `total` and `window` are byte counts, and `rate` is bytes per second for that window.

Reports are driven by the clock, not by chunks arriving, so they land on schedule and a stalled stream is reported rather than silent. The per-chunk
cost is two adds and a branch; the first chunk stamps the start, so idle time waiting for a peer is not averaged into the transfer's rate, and after
that the data path never reads the clock at all.

A stall is announced once and not repeated (a connection that goes quiet for an hour should not say so seven hundred times) and the report after
traffic resumes covers the whole gap. Samples written with `file=` are a time series rather than a narrative, so those are written every interval,
zeroes included. A stage that has never seen a byte stays quiet either way.

A datagram source never reaches end of stream, so it never prints a summary, but its periodic reports work as usual.

Ticking costs one timer per direction per connection for any pipeline containing a ticking stage, which multiplies under `fork`. `interval=0` is how
you opt out of it and keep only the summary.

For throughput at the endpoints rather than at a point inside the pipeline, and for a live display rather than log lines, see
[`--progress`](../progress.md).
