# `ws`

WebSocket is a layer, like [`tls`](tls.md), and the first one that changes what
an endpoint carries. TLS takes a byte stream and gives back a byte stream;
WebSocket takes one and gives back **messages**, so a `ws:` endpoint is a
datagram endpoint sitting on a TCP transport.

That is what makes it interesting beyond parity: it is the first message
oriented network transport tocat has, so `frame`, `unframe` and the other
boundary preserving stages become useful over a network rather than only over
unix sockets.

## The four schemes

| Scheme       | What it is                                        |
| ------------ | ------------------------------------------------- |
| `ws`         | Dial, plain. TCP with a `ws` layer                |
| `ws-listen`  | Accept, plain                                     |
| `wss`        | Dial, encrypted. TCP with `tls` then `ws` over it |
| `wss-listen` | Accept, encrypted                                 |

```console
$ tocat - ws:echo.example.com:80/socket
$ tocat 'wss-listen:8443,fork,cert=cert.pem,keyfile=key.pem' tcp:localhost:9000
```

The address is `host:port` as everywhere else in tocat, not a URL: write
`wss:echo.example.com:443`, not `wss://echo.example.com`. The port is always
required, since no scheme here defaults one.

| Option      | Description                                                        |
| ----------- | ------------------------------------------------------------------ |
| `path=PATH` | The resource. `ws:host:9000/socket` is sugar for this. Default `/` |
| `text`      | Send text frames instead of binary. See below                      |

`wss` and `wss-listen` also take every [`tls`](tls.md) option, and all four take
the
[socket options](../endpoints.md#socket-options-which-the-socket-schemes-share)
of the TCP transport underneath.

## The path is part of the endpoint

The same value on both sides: asked for when dialling, required when accepting.
Unset is `/` either way, so `ws-listen:9000` serves `/` and answers a client
asking for anything else with a 404 rather than upgrading it. The query string
is the client's business and is not compared.

A path is not authentication. It stops a misdirected client from being served,
not a determined one.

## Messages, not bytes

One message in is one message out. A `ws:` endpoint is under the same
[datagram rules](../plugins.md#datagrams) as `udp:` and `unix-seqpacket:`: a
stage that cannot carry boundaries is refused at build time rather than quietly
joining your messages together.

**A message larger than the copy buffer is an error.** The other message
endpoints truncate, because a datagram sender cannot know what the receiver will
accept. A WebSocket peer believes it sent a whole message, so delivering part of
one is worse than failing. Raise [`-b`](../buffers.md) if you meet this.

## Text and binary

Received text frames are passed through as their bytes; nothing is lost either
way. What is *sent* is binary, unless you set `text`.

```console
$ tocat - 'ws:api.example.com:80/socket,text'
```

A text frame promises the peer that its payload is valid UTF-8, and tocat cannot
know whether the bytes crossing it are text. `text` is you making that promise.
If a message turns out not to be valid UTF-8 the run fails rather than sending
something the peer is entitled to reject.

## What is not here yet

Subprotocol negotiation and `Origin` checking, both of which belong in the same
place as the path check. Compression is not offered.
