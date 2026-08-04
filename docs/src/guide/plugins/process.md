# `process` - pipe through a subprocess

Hands this path's bytes to a program's stdin and takes its stdout back, so any filter that reads stdin and writes stdout becomes a stage.

```console
$ tocat tcp-listen:9000,fork 'process:forward,command=gzip -c' tcp:backend:8080
```

```toml
[[plugin]]
name = "process"
direction = "source-to-sink"
argv = ["gzip", "-c"]
stderr = "log"
```

| Option           | Description                                                                                           |
|------------------|---------------------------------------------------------------------------------------------------------|
| `argv=[...]`     | Program and arguments, passed directly. No shell, no globbing, no metacharacters. Config file only    |
| `command=STR`    | A shell command line, as `system:`. Alias `cmd`. Runs with tocat's privileges                         |
| `stderr=log`     | Capture the child's stderr and re-emit each line as a warning tagged with the stage name. The default |
| `stderr=inherit` | Let it go to tocat's stderr, where it interleaves with logs and dumps                                 |
| `stderr=null`    | Discard it                                                                                            |

Give exactly one of `argv` or `command`; both, neither, or an empty one is a startup error. Only `command` works on the command line, since there is no
way to write an array there (and a command containing a comma has to go in a config file, because commas separate options).

Filters that emit nothing until their input closes work correctly: tocat feeds the child and drains it concurrently, and closes its stdin at end of
stream so it flushes. `sort`, `tac` and `gzip` all behave. A loop that wrote a chunk and waited for the reply would deadlock against exactly those
filters, which is why the two halves run at once.

A non-zero exit fails that direction rather than being logged and ignored, since it means the bytes the child produced were incomplete or wrong.

Two exceptions to that shape are worth knowing. On a datagram *source* there is no end of stream, so the child's stdin is never closed and it never
flushes on its own: only a filter that streams its output is usable there. On a datagram *sink* the child's output has no boundaries in it, so the
messages sent are invented from wherever the reads land.

## Cost

Two copies and two syscalls per chunk in each direction, plus a process per connection per direction. Under `fork` with `direction = "both"`,
sixty-four clients means one hundred and twenty-eight children. That is the correct price for "any tool becomes a stage", but it should be paid
knowingly: prefer a native stage for anything hot.

Because the child is driven concurrently rather than per chunk, `process` always has its own task and `detach = false` on one is rejected rather than
ignored. It is a [host plugin](../../api/host-plugins.md) rather than an implementation of the `Plugin` trait for the same reason.
