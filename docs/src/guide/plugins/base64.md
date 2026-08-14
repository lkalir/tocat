# `base64` and `unbase64` - base64

Behind the `base64` cargo feature. Carries arbitrary bytes across a hop that
only tolerates text. The two are paired, one per path, as `compress` and
`decompress` are:

```console
$ tocat - base64 unbase64:reverse tcp:relay.internal:9000
```

`direction=both` would encode the replies too and hand the peer double-encoded
text.

The far end of the link runs the mirror image, and the two relays form a
base64-armoured tunnel over a hop that would otherwise mangle binary.

| Option            | Plugin     | Description                                                                       |
| ----------------- | ---------- | --------------------------------------------------------------------------------- |
| `alphabet=NAME`   | both       | `standard` (`+`, `/`) or `url-safe` (`-`, `_`). Default is `standard`             |
| `accept-unpadded` | `unbase64` | Restore padding the peer omitted instead of rejecting the message. Off by default |

Both are cheap enough per byte to run inline, and both preserve message
boundaries, so either may sit on a datagram path.

## One message per chunk

Each chunk is one complete message. Neither stage holds bytes back between
calls, and `base64` pads every chunk it encodes.

That is the datagram contract, and on a datagram path it comes for free: one
call per message, one message out. On a byte stream a chunk is an arbitrary
slice, and base64 packs three bytes into four characters, so a chunk cut
anywhere other than a group boundary cannot be decoded on its own. Boundaries on
a stream path have to come from `frame` and `unframe`, which have not landed
yet; until they do, use these stages on datagram paths or on a peer that already
sends one message per read.

A chunk whose length is not a whole number of groups is reported as a
configuration error rather than relayed:

```
message is not a whole number of base64 groups: unbase64 decodes one complete message per call, so it needs an unframe stage ahead of it
```

The one mistake that cannot be caught is a chunk cut exactly on a group
boundary, which decodes to a short payload. Framing is what rules it out.

## Padding

`base64` always pads. `unbase64` requires padding, because under one message per
chunk a short final group is far more likely to be a framing bug than a peer
that omits `=`. `accept-unpadded` is for a peer that really does omit it, and
costs that diagnostic: a message truncated by one or two characters then decodes
to a short payload instead of erroring. A length remainder of 1 is rejected
either way, since no padding rule makes it valid.

Whitespace is not stripped. A trailing newline is a frame delimiter, and
swallowing it here would paper over framing that is missing or misconfigured.

## Size

Base64 is a 4-to-3 expansion, so an encoded message is a third larger than the
payload, plus padding. On a datagram path that is the difference between fitting
in a datagram and not: a 1400-byte payload leaves as 1868 bytes.
