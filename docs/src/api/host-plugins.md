# Host plugins

Some stages cannot be expressed as a synchronous byte transformer. A subprocess decides nothing per chunk, may emit output belonging to chunks it was
handed long ago, and has to be fed and drained at the same time or a filter like `sort` deadlocks against it.

Rather than bend `Plugin` into something a subprocess could satisfy, and lose the property that makes it portable, such a stage is *described* to the
host and *run* by it. `build` returns the other variant of `Stage`:

```rust,ignore
Ok(Stage::External(ExternalStage {
    argv,
    shell,
    stderr: config.stderr,
    name: ctx.stage().name.to_string(),
}))
```

The plugin never sees a byte. It validates the entry, decides whether the command line is a program with arguments or a shell string, and names itself
for logs and for attributing the child's stderr. The host owns the process, the pipes, the tasks, the concurrent feed and drain, the stdin close at end
of stream, and the exit status.

## Why the split exists

- **The trait stays honest.** If it had to cover concurrent, latency-shifted stages, every plugin author would face a contract shaped by a case they do
  not have, and the guarantees the simple case relies on would be weaker.
- **Capability is structural.** Spawning is a host capability by construction, so a WASM guest can only ever produce `Stage::Filter`. A guest cannot
  spawn a process because there is nothing in its interface that spawns a process, rather than because it is asked not to.
- **The host already owns the awkward parts.** Pipe sizing, killing the child when the connection drops, closing stdin so the child flushes, and
  mapping a non-zero exit onto a failed direction are host responsibilities anyway.

## What it still shares

From the user's point of view an external stage is an ordinary pipeline entry: it takes a direction, it takes `as=`, it appears in the resolved chain
and in logs the same way, and it sits where it was written. What differs is that it is always its own segment, so `detach = false` on one is rejected
rather than ignored, and that it can never carry datagrams: its stdin and stdout are byte streams, so message boundaries are gone the moment bytes
cross the pipe.

The one shipped example is [`process`](../guide/plugins/process.md). Its cost model is worth reading before adding another: two copies and two syscalls
per chunk each way, plus a process per connection per direction.
