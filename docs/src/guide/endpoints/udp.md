# `udp` and `udp-listen`

Messages rather than a byte stream. `udp:` fixes a peer to send to;
`udp-listen:` binds a port and either peers with the first sender or, with
`fork`, serves every sender separately. The target follows the same `host:port`
rules as `tcp-listen`, defaulting to `127.0.0.1:8000`.

```console
$ tocat udp-listen:9000 tcp:backend:80
$ tocat - udp:127.0.0.1:5353
$ tocat udp-listen:5353 'tee,format=hex' udp:8.8.8.8:53
$ tocat udp-listen:5353,fork 'timeout:both,timeout=30s' udp:8.8.8.8:53
```

| Option              | Description                                                                                                    |
| ------------------- | -------------------------------------------------------------------------------------------------------------- |
| `bind=ADDR`         | `udp:` only. Local address to bind. Defaults to an ephemeral port on the wildcard address in the peer's family |
| `fork`              | `udp-listen:` only. Serve each sending address separately, with its own dialled peer and plugin instances      |
| `max-connections=N` | Sessions served at once under `fork`. Default 1024. Datagrams from a new sender past the ceiling are dropped   |
| `name=TEXT`         | Label for logs and dumps. Default `udp://addr`                                                                 |

`udp:` resolves the peer before binding, so that the local socket lands in the
same address family. Without `fork`, `udp-listen:` peeks the first datagram to
learn who the peer is and then connects to it, leaving that datagram queued for
the relay rather than eating it.

## Forking

With `fork` the socket is left unconnected and datagrams are routed by source
address. Each new address becomes a session: its own dialled peer, its own
buffers, its own plugin instances, its own span in the log. Replies go back out
of the same socket to that address, so no extra port is opened per sender.

Nothing ends a session on its own, because UDP has no close to observe. Use the
[`timeout`](../plugins/timeout.md) plugin, on both directions:

```console
$ tocat udp-listen:9000,fork 'timeout:both,timeout=30s' tcp:backend:80
```

`:both` matters. A forward-only halt ends the path from the sender but leaves
the reverse pump reading a sink that may never close, and it is the session task
finishing that releases the connection permit and the map entry. Without it,
sessions accumulate until `max-connections` is reached and every new sender is
dropped from then on; the log says so the first time it happens.

A halt is a real end of stream, so a `hash` digest, a `compress` epilogue or a
`rate` summary do arrive when the session closes. A sender that goes quiet and
comes back gets a new session with fresh plugin state, the way a reconnecting
TCP client would.

Because one receive loop serves every sender, a session that cannot keep up has
its datagrams dropped rather than being allowed to stall the others. The first
drop is a warning and the rest are at debug, so `-v` is where to look if a peer
is losing messages.

Three consequences of the datagram shape are worth keeping in mind.

**No end of stream, unless a stage makes one.** A datagram source runs until it
is interrupted, so anything that only happens at end of stream never happens on
that path: a `rate` summary, a `compress` ratio report, the final short `block`.
A `timeout` stage halting the path is the one thing that produces one.

**The copy buffer is the message ceiling.** One receive is one datagram, and a
message longer than the buffer is truncated by the kernel. The default is 256
KiB, which is above any datagram that will survive a real network, but `-b` set
low enough will start cutting messages.

**Boundaries are data.** A stage that may not preserve them draws a warning when
the destination on its path is a datagram endpoint. See
[Datagrams](../plugins.md#datagrams).
