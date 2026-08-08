# `udp` and `udp-listen`

Messages rather than a byte stream. `udp:` fixes a peer to send to;
`udp-listen:` binds a port and peers with the first sender. The target follows
the same `host:port` rules as `tcp-listen`, defaulting to `127.0.0.1:8000`.

```console
$ tocat udp-listen:9000 tcp:backend:80
$ tocat - udp:127.0.0.1:5353
$ tocat udp-listen:5353 'tee,format=hex' udp:8.8.8.8:53
```

| Option      | Description                                                                                                    |
| ----------- | -------------------------------------------------------------------------------------------------------------- |
| `bind=ADDR` | `udp:` only. Local address to bind. Defaults to an ephemeral port on the wildcard address in the peer's family |
| `name=TEXT` | Label for logs and dumps. Default `udp://addr`                                                                 |

`udp:` resolves the peer before binding, so that the local socket lands in the
same address family. `udp-listen:` peeks the first datagram to learn who the
peer is and then connects to it, leaving that datagram queued for the relay
rather than eating it.

There is no per-sender demultiplexing yet, so `udp-listen` peers with whoever
sends first and ignores everyone else; `fork` does not apply.

Three consequences of the datagram shape are worth keeping in mind.

**No end of stream.** A datagram source runs until it is interrupted, so
anything that only happens at end of stream never happens on that path: a `rate`
summary, a `compress` ratio report, the final short `block`.

**The copy buffer is the message ceiling.** One receive is one datagram, and a
message longer than the buffer is truncated by the kernel. The default is 256
KiB, which is above any datagram that will survive a real network, but `-b` set
low enough will start cutting messages.

**Boundaries are data.** A stage that may not preserve them draws a warning when
the destination on its path is a datagram endpoint. See
[Datagrams](../plugins.md#datagrams).
