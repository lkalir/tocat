# `unix-dgram` and `unix-dgram-listen`

Local messages without a connection. `unix-dgram:` fixes a peer to send to;
`unix-dgram-listen:` binds an address and receives. This is the shape `/dev/log`
and most local logging protocols use.

```console
$ tocat unix-dgram-listen:/tmp/log.sock,unlink file:/var/log/collected.log
$ tocat - unix-dgram:/dev/log
$ tocat unix-dgram-listen:/tmp/log.sock,unlink 'tee,format=hex' udp:collector:514
$ tocat unix-dgram-listen:@tocat,fork 'timeout:both,timeout=30s' tcp:backend:80
```

| Option                      | Description                                                                                                     |
| --------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `bind=ADDR`                 | `unix-dgram:` only. Local address to bind, so replies have somewhere to go. Defaults to a temporary path        |
| `fork`, `max-connections=N` | `unix-dgram-listen:` only. Serve each sending address separately. See [Forking](#forking)                       |
| `unlink`                    | Remove a stale address before binding. On `unix-dgram:` it needs `bind=`, since the generated one is always new |
| `mode=NNN`                  | Octal permissions applied after binding, explicitly, so umask does not mask them                                |
| `name=TEXT`                 | Label for logs and dumps. Default `unix-dgram://path`                                                           |

The scheme is also spelled `unix-datagram` and `uds-dgram`, with `-listen` on
the end for the listening form.

## Replies need an address

A unix datagram socket that never bound has no address, so nothing sent to it
can be answered: the kernel has nowhere to deliver a reply to. This is the one
place unix datagrams differ sharply from UDP, where an unbound sender is given a
port automatically and can always be replied to.

`unix-dgram:` therefore binds a local address before it connects, so the reverse
direction works by default. Without one, a relay whose sink is duplex would sit
waiting for replies the kernel could not deliver, which looks like a hang rather
than a misconfiguration.

The default is a path in the temporary directory, created with mode 600 and
removed when the relay ends. `bind=` names one yourself, which is what you want
when the peer authorises callers by path:

```console
$ tocat - unix-dgram:/run/app/app.sock,bind=/run/app/client.sock,mode=660
```

On the listening side the same rule decides who can be answered. The first
sender becomes the peer, and if it never bound, tocat says so and the path
becomes receive-only:

```console
WARN the first sender has no address of its own, so this path can receive but
not send; a peer that expects replies has to bind before it sends
```

That is the ordinary case for logging, where nothing is expected back and the
sink has nothing to send anyway.

## Listening

Without `fork`, `unix-dgram-listen:` receives one message to learn who the
sender is, connects to it, and hands that first message to the pipeline as the
first thing it carries. Nothing is lost, and once connected the kernel filters
to that sender, so later messages from anyone else are dropped. A sender with no
address of its own leaves the socket unconnected, and then messages from
everyone are received.

## Forking

With `fork` the socket stays unconnected and messages are routed by sending
address. Each new address becomes a session: its own dialled peer, its own
buffers, its own plugin instances, its own span in the log. Replies go back out
of the same socket, so no extra socket is created per sender.

A session needs a sender it can both recognise and answer, and that is exactly
the address that also carries replies. Senders that bound a path are served.
Senders that did not are dropped, because two of them cannot be told apart and
neither can be answered:

```console
WARN dropping datagram; further drops are logged at debug
  peer=unnamed reason=the sender has no address to reply to
```

Senders in the abstract namespace are dropped for a narrower reason: replies
here go out by path, and an abstract name is not one. Bind the senders to paths
if they need to be forked to.

If your senders are anonymous and you want a session each, they need a
connection rather than an address: [`unix-seqpacket-listen`](unix-seqpacket.md)
is the message transport that accepts.

Nothing ends a session on its own, because a datagram socket has no close to
observe. Use the [`timeout`](../plugins/timeout.md) plugin on both directions,
exactly as on [`udp-listen`](udp.md#forking); without it, sessions accumulate
until `max-connections` is reached and new senders are dropped from then on.

## Consequences of the datagram shape

**No end of stream, unless a stage makes one.** Anything that only happens at
end of stream never happens on that path: a `rate` summary, a `compress` ratio
report, the final short `block`. A `timeout` stage halting the path is the one
thing that produces one. [`unix-seqpacket`](unix-seqpacket.md) is the local
message transport that does have an end of stream.

**The copy buffer is the message ceiling.** One receive is one message, and a
longer one is truncated by the kernel, silently. The default is 256 KiB.

**Boundaries are data.** A stage that may not preserve them draws a warning when
the destination on its path is a message endpoint. See
[Datagrams](../plugins.md#datagrams).

## Addresses

Paths and abstract names both work, and the rules for `unlink`, `mode` and
cleanup are the same as for [`unix`](unix.md#addresses).
