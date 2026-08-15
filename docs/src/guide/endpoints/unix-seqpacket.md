# `unix-seqpacket` and `unix-seqpacket-listen`

A local socket that keeps message boundaries and still has a connection and an
end of stream. What the peer sent as three messages arrives as three messages,
in order, and closing the connection is visible on the other side.

```console
$ tocat - unix-seqpacket:/run/app/app.sock
$ tocat unix-seqpacket-listen:/tmp/tocat.sock,fork,unlink,mode=660 tcp:localhost:8080
$ tocat unix-seqpacket-listen:@tocat 'frame,format=lv32' tcp:collector:9000
```

| Option                      | Description                                                                                                    |
| --------------------------- | -------------------------------------------------------------------------------------------------------------- |
| `fork`, `max-connections=N` | As [`tcp-listen`](tcp.md). `unix-seqpacket-listen` only                                                        |
| `unlink`                    | Remove a stale socket before binding. `unix-seqpacket-listen` only                                             |
| `mode=NNN`                  | Octal permissions applied after binding, explicitly, so umask does not mask them. `unix-seqpacket-listen` only |
| `name=TEXT`                 | Label for logs and dumps. Default `unix-seqpacket://path`                                                      |

The scheme is also spelled `unix-seqpkt`, `uds-seqpacket` and `seqpacket`, with
`-listen` on the end for the listening form.

## Choosing it over `unix`

Both are local sockets between processes on one machine, and the difference is
what a read gives you back.

`unix` is a byte stream: what arrives is whatever the kernel had ready, so a
protocol carried over it needs framing of its own, and tocat's
[`frame`](../plugins/frame.md) plugin exists for that. `unix-seqpacket` carries
the boundaries for you. If the peer speaks a message protocol, this is the
endpoint that stops you inventing a framing layer to recover what the sender
already knew.

That also changes what a pipeline may do. A seqpacket endpoint is a message
endpoint, so the same rules apply to it as to [`udp`](udp.md): a stage that does
not preserve boundaries draws a warning when the destination on its path is one
of these, and a stage that needs boundaries arriving is satisfied by one. See
[Datagrams](../plugins.md#datagrams).

## The buffer is the message ceiling

One receive is one message, and a message longer than the copy buffer is
truncated, with the rest lost. Unlike UDP the kernel says so, and tocat warns
once per connection:

```console
$ tocat -b 1KiB unix-seqpacket:/run/app/app.sock -
WARN a message did not fit the buffer and the rest of it is lost; raise -b past
the largest message the peer sends
```

Further truncations on the same connection are logged at debug, so a flood does
not bury the rest of the log. The default buffer is 256 KiB.

## Ending

A seqpacket connection has a real end of stream, which is what separates it from
[`unix-dgram`](unix-dgram.md) and from UDP. When tocat has nothing further to
send it half closes the connection, so a peer waiting for that before it replies
gets what it is waiting for, and one-way transfers finish on their own:

```console
$ tocat file:messages.bin unix-seqpacket:/run/app/app.sock
```

The reverse arrives as an ordinary end of stream too, so `hash` digests, `rate`
summaries and `compress` epilogues are all produced.

One consequence worth knowing: a zero-length message and a peer shutdown look
the same at the receiving end, so sending an empty message ends the path.

## Peers under `fork`

A connected unix socket has no address, so accepted connections are all labelled
`unnamed`, the same as a `unix-listen` client that never bound. Each one still
gets its own plugin instances and its own dialled peer.

## Addresses

Paths and abstract names both work, and the rules for `unlink`, `mode` and
cleanup are the same as for [`unix`](unix.md#addresses).
