# Effects and channels

A plugin never opens a file, never sleeps and never touches a socket. It asks, and the host does it after the call returns. Everything is staged
per call and applied in one pass, which is what lets the flush overlap the downstream write.

| Call                       | Asks the host to                                                                          |
|----------------------------|---------------------------------------------------------------------------------------------|
| `ctx.side_write(id, bytes)`| Append to a side channel opened at build time                                              |
| `ctx.log(level, message)`  | Emit a log record, tagged with this stage's display name                                   |
| `ctx.pace(delay)`          | Wait `delay` before reading upstream again                                                 |
| `ctx.halt(reason)`         | Stop reading upstream, as if it had reached end of stream                                  |
| `ctx.rearm()`              | Restart this stage's [tick schedule](ticks.md) from now                                    |

## Side channels

At build time a stage describes the sink it wants and receives an opaque `ChannelId`; at run time it queues writes against that id.

```rust,ignore
let channel = ctx.open_channel(ChannelTarget::file("session.hex"))?;
```

The host owns the descriptor and de-duplicates identical targets, so two entries naming the same path share one buffered writer and their output
interleaves at chunk granularity rather than racing. Each sink has its own lock, so two directions contend only when they genuinely target the same
file. A side write costs one `extend_from_slice` into a staging buffer that is reused across chunks, so there is no allocation and no lock inside the
plugin call.

`ChannelTarget` is `Stderr` or `File { path, append }`. There is deliberately no stdout: on a stdio endpoint that stream carries relay payload, and the
host refuses a file target that resolves to it. stderr is written unbuffered, since it is shared with the log writer and a dump stranded behind a
partial line would be worse than the syscall.

Channels are opened once for the whole process, before any endpoint is touched, so an unwritable dump file is a startup error rather than a surprise on
the first byte. That also means a stage cannot ask for a new target after startup.

## Logging

`ctx.log` takes a `LogLevel` and a message. The host emits it under the `plugin` target with a `stage` field, so a user can select plugin output with
`RUST_LOG=plugin=info` and can tell two instances apart by the name `as` gave them. Do not repeat the stage name in the message.

## Pacing and halting

`pace` is the entire throttling mechanism: the host simply does not read for that long. Nothing is buffered, and on a socket the stalled read closes
the receive window and slows the peer at source. It is applied after whatever was emitted has been written, so the bytes in hand are never held hostage
by the wait. Where several stages ask on one chunk, the longest request wins rather than the sum.

`halt` ends the transfer deliberately. Everything already emitted is still written, `on_eof` still cascades, and the path closes down its normal way, so
the relay exits successfully and the reason is logged at info. It is a decision, not a failure, and must not be reported as one. Where several stages
ask, the first wins, which is the one nearest upstream.

Both default to doing nothing on the `EffectSink` trait, so a host with no reader to hold (a test harness, an offline driver) is not obliged to honour
them.
