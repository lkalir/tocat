# `tee` - mirror the stream

Writes a copy of everything on its path to a file or to stderr, without touching
the payload.

```console
$ tocat tcp-listen:8080,fork tee,format=hex tcp:example.com:80
$ tocat tcp-listen:8080,fork 'tee:forward,file=req.bin' 'tee:reverse,file=resp.bin' tcp:example.com:80
```

| Option              | Description                                                                          |
| ------------------- | ------------------------------------------------------------------------------------ |
| `file=PATH`         | Where to write. Omitted, `-`, `stderr`, `/dev/stderr` or `/dev/fd/2` all mean stderr |
| `format=hex`        | Offset, hex and ASCII columns behind a header                                        |
| `format=raw-binary` | Payload bytes verbatim. Aliases `raw`, `binary`. The default                         |
| `append`            | Append to an existing file rather than truncating it. On by default                  |
| `width=N`           | Bytes per row in hex mode. Must be at least 1. Default is 16                         |
| `label=TEXT`        | Override the hex header label. Prefer `as`, which names the instance everywhere      |

Hex entries are headed with the hop and the stage, then the length and the
offset within this path:

```
[tcp://example.com:80_10.0.0.4:52134 -> STDIO | audit] 4 bytes @ 0x0
00000000  70 69 6e 67                                      |ping|
```

Entries naming the same file share one buffered writer, so their output
interleaves at chunk granularity rather than racing. Hex entries stay separable
because each carries the connection and the stage name in its header; raw
entries do not, so two raw tees on one file will produce something you cannot
untangle.

tocat refuses to write a dump to stdout, which may be carrying relayed payload.
A dump on stderr and [`--progress`](../progress.md) also do not mix, and tocat
warns when both are asked for: the painter and the log writer coordinate over
the line on screen, and a dump does not.

`tee` never materialises the payload: it passes the chunk through and copies
into the host's staging buffer, so inserting one costs a virtual call per chunk
plus the dump itself. Its position decides only what it sees: before a
[`compress`](compress.md) stage it captures the payload, after it captures the
wire.
