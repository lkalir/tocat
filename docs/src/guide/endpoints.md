# Endpoints

The fundamental unit of tocat is the endpoint. An endpoint is a scheme, a
target, and a set of options.

```
scheme:target,option,option=value
```

Bare options mean true, so `fork` and `fork=true` are equivalent. Schemes and
options may have various aliases. If an option is specified multiple times, the
last instance takes precedence. Options belong to the scheme that documents
them: anything else is an error rather than being ignored, so `tcp:80,append` is
rejected. Spelling is forgiving. Case is ignored and dashes and underscores are
removed for schemes and option keys. Values remain untouched.

## Schemes

| Scheme                                                 | Aliases on the command line  | Carries   | Shape   |
| ------------------------------------------------------ | ---------------------------- | --------- | ------- |
| [`tcp`](endpoints/tcp.md)                              | `tcp-connect`, `connect`     | bytes     | duplex  |
| [`tcp-listen`](endpoints/tcp.md)                       | `listen`                     | bytes     | duplex  |
| [`udp`](endpoints/udp.md)                              | `udp-connect`                | datagrams | duplex  |
| [`udp-listen`](endpoints/udp.md)                       |                              | datagrams | duplex  |
| [`unix`](endpoints/unix.md)                            | `uds`                        | bytes     | duplex  |
| [`unix-listen`](endpoints/unix.md)                     | `uds-listen`                 | bytes     | duplex  |
| [`unix-seqpacket`](endpoints/unix-seqpacket.md)        | `seqpacket`, `unix-seqpkt`   | datagrams | duplex  |
| [`unix-seqpacket-listen`](endpoints/unix-seqpacket.md) | `seqpacket-listen`           | datagrams | duplex  |
| [`unix-dgram`](endpoints/unix-dgram.md)                | `unix-datagram`, `uds-dgram` | datagrams | duplex  |
| [`unix-dgram-listen`](endpoints/unix-dgram.md)         | `unix-datagram-listen`       | datagrams | duplex  |
| [`file`](endpoints/file.md)                            | `open`                       | bytes     | one way |
| [`pipe`](endpoints/pipe.md)                            | `fifo`                       | bytes     | one way |
| [`exec`](endpoints/exec.md)                            |                              | bytes     | duplex  |
| [`system`](endpoints/exec.md)                          |                              | bytes     | duplex  |
| [`pty`](endpoints/pty.md)                              |                              | bytes     | duplex  |
| [`pty-exec`](endpoints/pty.md)                         |                              | bytes     | duplex  |
| [`tty`](endpoints/tty.md)                              | `serial`                     | bytes     | duplex  |
| [`stdio`](endpoints/stdio.md)                          | `-`                          | bytes     | duplex  |

Two properties in that table decide how the rest of tocat behaves around an
endpoint.

**Duplex or one way.** A duplex endpoint can be both read and written, so a run
with duplex endpoints on both sides relays in both directions. A one-way
endpoint is read when it is the source and written when it is the sink, and the
opposite path has nothing to carry.

That second half is what makes a one-way transfer end.
`tocat file:in.bin
tcp:host:9000` exits at end of file because the reverse
direction does not exist rather than because it finished: a direction with no
half to read is skipped outright, so nothing is left waiting on the peer. A
duplex endpoint has no such exit, which is why `file:` opens by role even where
the path underneath it could be opened both ways.

**Bytes or datagrams.** A byte endpoint carries a stream, and a chunk is an
arbitrary slice of it. A datagram endpoint carries messages, and the boundaries
are part of the data. That distinction is what the
[datagram rules](plugins.md#datagrams) in the pipeline are about. It is about
what the endpoint carries rather than how it connects:
[`unix-seqpacket`](endpoints/unix-seqpacket.md) accepts connections and has an
end of stream like `tcp-listen`, and is still a datagram endpoint, because a
message it delivers is one the peer sent whole.

## `name`, which every scheme takes

Every scheme accepts `name=TEXT`, which replaces the label the endpoint is known
by. Labels appear in log records, in `tee` headers, and in the
`upstream -> downstream` description a stage is given at build time.

```console
$ tocat 'tcp-listen:9000,fork,name=frontend' 'tcp:10.0.0.5:8080,name=backend'
```

Four schemes keep their own label even when `name` is given, because the target
is the identity: `file:` shows its path, and `exec:`, `system:` and `pty-exec:`
show the command line.

Default labels are `tcp://addr`, `udp://addr`, `unix://path`,
`unix-seqpacket://path`, `unix-dgram://path`, `pipe://path`, `file://path`,
`EXEC(argv)`, `SYSTEM(command)` and `STDIO`. Under `fork` the accepted peer is
appended to the listening side's label, so a dump or a log line says which
connection it belongs to.
