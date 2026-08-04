# `pipe`

Alias: `fifo`. Unidirectional like [`file`](file.md), but a rendezvous rather than storage: read as the source, written as the sink.

By default tocat holds the FIFO open read-write, making itself a writer. Two things follow: opening never blocks, and the stream never ends, so
producers can come and go without the relay noticing. That is the difference from `file:` pointed at the same path, which relays one producer and exits.
POSIX leaves read-write on a FIFO undefined, every platform tocat targets implements it, and there is no other way to hold one open.

```console
$ tocat pipe:/tmp/events,create,mode=660 tcp:collector:9000
$ myapp >> /tmp/events          # restart this all day
```

| Option      | Description                                                                                                             |
|-------------|---------------------------------------------------------------------------------------------------------------------------|
| `create`    | `mkfifo` the path if missing. On by default. A path that exists but is not a FIFO is an error                            |
| `hold`      | Hold the FIFO open across producers. On by default; `hold=false` gives one-shot behaviour with EOF, and warns that the open will block |
| `size=N`    | Kernel FIFO capacity, e.g. `size=1MiB`. Alias `pipe-size`. Linux only, best-effort. Not the same knob as `-b`, see [Buffers](../buffers.md) |
| `unlink`    | Remove the FIFO when the relay finishes                                                                                  |
| `mode=NNN`  | Octal permissions applied after creation, explicitly, so umask does not mask them                                        |
| `name=TEXT` | Label for logs and dumps. Default `pipe://path`                                                                          |

Both halves come from the same descriptor but are exposed half-duplex on purpose: a held FIFO is readable and writable, so treating it as duplex would
feed tocat's own writes back into its reader.

Two tocats sharing a FIFO chains relays with different pipelines, and a FIFO on each side makes a test harness you can drive entirely from a shell:

```console
$ tocat pipe:/tmp/in,create redact pipe:/tmp/out,create &
$ cat /tmp/out &
$ echo 'Authorization: Bearer abc123' > /tmp/in
```

Because a held FIFO never reaches end of stream, it behaves like a datagram source in one respect: end-of-stream work such as a `rate` summary or a
final short `block` never runs. Use `hold=false` when you want the relay to finish with its producer.
