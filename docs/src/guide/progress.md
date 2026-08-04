# Progress

`--progress` draws a `pv`-style line on stderr while the relay runs.

```console
$ tocat --progress file:big.iso tcp:host:9000
  1.23GiB 0:00:12 [   102MiB/s] [=============>        ]  42% ETA 0:00:16
```

The line is counts, elapsed time and rate, then a bar, percentage and ETA where those are knowable. It counts bytes at the endpoints, before any stage
sees them, and it is redrawn on a timer (ten times a second) rather than when bytes arrive, so a stalled transfer is visibly stalled rather than
frozen. The displayed rate is smoothed over roughly a second.

`--progress` on its own means `auto`: draw only when stderr is a terminal. `--progress=always` draws regardless, which is what you want when stderr is
redirected to a file. `--progress=never` is the default: a relay is as often a daemon as it is a command. The same values work in the config file.

```toml
progress = "auto"
```

The bar, the percentage and the ETA need a total, which tocat only has when the source is a regular `file:` and neither endpoint forks. Everything else
gets the counts, the elapsed time and the rate. Traffic coming back from the sink is reported separately once there is any:

```
  1.23GiB out  340MiB in 0:00:12 [   102MiB/s]
```

Under `fork` the line aggregates every connection, and shows the count once more than one is open. An ETA longer than 99:59:59 is not an estimate, so
it reads `--:--:--`.

When the relay finishes, the line is erased and replaced with one summary line, unless nothing moved at all:

```
1.23GiB in 0:00:12 (102MiB/s)
```

Two things share stderr with the display. Logs are handled: an event erases the line, prints, and the next tick redraws it, all under the same stderr
lock, so a record cannot land in the middle of a frame. A [`tee`](plugins/tee.md) pointed at stderr is not, and tocat warns when the two are used
together. Send the dump to a file, or drop the flag. No ANSI is involved anywhere: erasing is a carriage return, spaces and another carriage return,
which is what makes `--progress=always` readable in a file.

Measuring is not free. A relay with no plugins and stream sockets on both sides is normally handed to `copy_bidirectional_with_sizes`, and counting
bytes needs a per-chunk hook that call does not offer, so `--progress` puts that case on the general copy path instead. See
[The data path](../design/data-path.md).

For throughput at a point *inside* the pipeline rather than at the endpoints, see the [`rate`](plugins/rate.md) plugin. The two answer different
questions: `--progress` is what the transfer is doing, `rate` is what a particular stage is seeing.
