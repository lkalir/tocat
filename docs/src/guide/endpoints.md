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

## Socket options, which the socket schemes share

`tcp`, `tcp-listen`, `unix`, `unix-listen`, `udp` and `udp-listen` take the
options below, as far as their kind of socket has them. An option a scheme
cannot honour is an error rather than a no-op, so `unix:/tmp/s,nagle=false` is
refused: Nagle's algorithm is a TCP thing.

| Option                 | Where                   | Description                                                                    |
| ---------------------- | ----------------------- | ------------------------------------------------------------------------------ |
| `reuseaddr`            | tcp, udp                | Bind an address still held in `TIME_WAIT`                                      |
| `nagle=false`          | tcp                     | Send small writes immediately, trading throughput for latency                  |
| `keepalive[=DURATION]` | tcp                     | Probe an idle connection; bare uses the system's idle time, a duration sets it |
| `keepalive-interval=`  | tcp                     | Between probes once the idle time has passed                                   |
| `keepalive-probes=N`   | tcp                     | Unanswered probes before the connection is dead                                |
| `linger=DURATION`      | tcp, unix               | How long `close` waits for unsent data. `linger=0s` closes with an RST instead |
| `recv-buffer=SIZE`     | all                     | Kernel receive buffer for this socket                                          |
| `send-buffer=SIZE`     | all                     | Kernel send buffer for this socket                                             |
| `backlog=N`            | tcp-listen, unix-listen | How deep the kernel queues connections not yet accepted                        |

socat's spellings are accepted where they differ: `nodelay` (which is `nagle`
inverted), `rcvbuf`, `sndbuf`, `keepintvl`, `keepcnt`, `so-reuseaddr` and
`so-linger`.

```console
$ tocat 'tcp-listen:9000,fork,backlog=64,nagle=false' 'tcp:backend:80,keepalive=30s'
```

**`recv-buffer` is not `buffer-size`.** The kernel buffer is how much the
operating system will hold for this socket; [`buffer-size`](buffers.md) is how
much tocat copies at a time and, on a datagram endpoint, the largest message it
can carry. Sizing the wrong one is the usual mistake.

**On a listener the options belong to each accepted connection**, not to the
listening socket, so under `fork` every client gets them. `reuseaddr` and the
buffer sizes are the exceptions: they have to be set before the bind, so they
apply to the listening socket itself.

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
