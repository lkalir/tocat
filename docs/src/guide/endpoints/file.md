# `file`

Alias: `open`. Files are unidirectional. They are read when they are the source
and written to when they are the sink, so the writing options are simply not
consulted on the source side, where the file is opened read-only.

Being unidirectional is what makes a transfer end.
`tocat file:in.bin
tcp:host:9000` exits at end of file because the reverse
direction does not exist, so nothing is left waiting on the peer. Opening the
file both ways would keep the relay alive indefinitely, which is why a device
that genuinely needs both directions gets its own scheme rather than a flag
here: see [`tty`](tty.md).

```console
$ tocat file:/tmp/payload tcp:localhost:9000
$ tocat tcp-listen:9000 file:/tmp/capture,truncate
```

| Option      | Description                                                                       |
| ----------- | --------------------------------------------------------------------------------- |
| `append`    | Append instead of overwriting                                                     |
| `create`    | Create if missing. On by default, off under `device`                              |
| `truncate`  | Truncate on open. Alias `trunc`. Dropped under `append`, where the two contradict |
| `device`    | Require the path to already be a block or character device. Alias `dev`           |
| `seek=SIZE` | Start at this offset rather than at the beginning                                 |
| `name=TEXT` | Accepted, but the label stays `file://path`: a file is identified by its path     |

## Devices

A block device or a plain character device is a file that happens to live in
`/dev`, so it belongs here rather than in a scheme of its own. Two options make
that safe and useful.

**`device` asserts the path already is one.** Without it, `create` is on, so a
wrong path or an unplugged adapter turns into a regular file of that name the
moment it is used as a sink. As root that is a new file in `/dev`. With it, the
path is checked before anything is opened, and the error tells you whether it
was missing or was the wrong kind of thing.

```console
$ tocat file:/dev/sda,device file:disk.img,truncate
```

`device` contradicts `create`, `truncate` and `append`, and asking for both is
an error rather than a silent override: a device has no length to change, and
nothing to create.

**`seek=SIZE` starts somewhere other than the beginning**, reading from that
offset as the source and writing to it as the sink. It takes the usual size
suffixes, so `seek=1MiB` is a mebibyte in.

```console
$ tocat file:/dev/sda,device,seek=1MiB file:partition.img,truncate
```

An offset into a block device that is not a multiple of its block size is
warned about rather than refused, since the kernel will not complain and the
symptom is a shifted image rather than an error. `seek` contradicts `append`,
where every write goes to the end whatever the offset says.

A terminal is the exception to all of this. It is duplex on one descriptor,
which this scheme cannot produce, and its settings have to be restored
afterwards, which this scheme has nowhere to keep: see [`tty`](tty.md).

## FIFOs

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
