# `file`

Alias: `open`. Files are unidirectional. They are read when they are the source
and written to when they are the sink, so the writing options are simply not
consulted on the source side, where the file is opened read-only.

```console
$ tocat file:/tmp/payload tcp:localhost:9000
$ tocat tcp-listen:9000 file:/tmp/capture,truncate
```

| Option      | Description                                                                       |
| ----------- | --------------------------------------------------------------------------------- |
| `append`    | Append instead of overwriting                                                     |
| `create`    | Create if missing. On by default                                                  |
| `truncate`  | Truncate on open. Alias `trunc`. Dropped under `append`, where the two contradict |
| `name=TEXT` | Accepted, but the label stays `file://path`: a file is identified by its path     |

`file:` pointed at a FIFO works, but the open blocks until a peer appears and
the stream ends when the last writer leaves. That is a legitimate thing to want,
so it warns rather than refusing. See [`pipe`](pipe.md) for the version that
outlives its producers.

Two other things follow from being a file:

- A regular file as the source is the one case where tocat knows the size of the
  transfer up front, which is what lets [`--progress`](../progress.md) draw a
  bar, a percentage and an ETA rather than only counts.
- With no plugins declared and the other endpoint also being `file:`, `pipe:` or
  `stdio:`, the relay takes its synchronous copy path. See
  [The data path](../../design/data-path.md).
