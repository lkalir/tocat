# `stdio`

Also spelled `-`. Note that tocat's stdout carries relayed data when stdio is
the sink. Logs and dumps go to stderr for this reason, and tocat refuses to open
stdout as a dump target at all.

| Option      | Description                               |
| ----------- | ----------------------------------------- |
| `name=TEXT` | Label for logs and dumps. Default `STDIO` |

```console
$ dd if=/dev/zero | tocat - tcp:localhost:9000
```

Both descriptors are resized when they turn out to be pipes. Nobody declares
them as such, but `tocat … | pv` makes fd 1 one, and its 64 KiB default would
otherwise cap every write. See [Buffers](../buffers.md).

On the synchronous path tocat reads and writes the descriptors directly rather
than going through `std::io::stdin`/`stdout`: the standard handles bring a
`LineWriter` that scans every byte for newlines and an 8 KiB `BufReader`, both
of which are wrong for binary payload.

Three things contend for stderr: log records, a [`tee`](../plugins/tee.md)
pointed there, and the [`--progress`](../progress.md) line. Logs and the
progress line are coordinated under one lock; a stderr dump is not, and tocat
warns about that combination.
