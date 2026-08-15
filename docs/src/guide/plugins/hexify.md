# `hexify` and `unhexify` - hex

Carries arbitrary bytes across a hop that only tolerates text, as
[`base64`](base64.md) does, and for twice the bytes. The two are paired, one per
path:

```console
$ tocat - hexify unhexify:reverse tcp:relay.internal:9000
```

`direction=both` would encode the replies too and hand the peer double-encoded
text.

The far end of the link runs the mirror image, and the two relays form a
hex-armored tunnel over a hop that would otherwise mangle binary.

| Option      | Plugin   | Description                                                                  |
| ----------- | -------- | ---------------------------------------------------------------------------- |
| `case=NAME` | `hexify` | `lowercase` (or `lower`) or `uppercase` (or `upper`). Default is `lowercase` |

Both are cheap enough per byte to run inline, and both preserve message
boundaries, so either may sit on a datagram path.

`unhexify` takes no options. It accepts either case regardless of how the far
end is set, so an option there would have nothing to do.

## One message per chunk

Each chunk is one complete message. Neither stage holds bytes back between
calls.

That is the datagram contract, and on a datagram path it comes for free: one
call per message, one message out. On a byte stream a chunk is an arbitrary
slice, and hex packs one byte into two characters, so a chunk cut on an odd
offset cannot be decoded on its own. Boundaries on a stream path have to come
from `frame` and `unframe`, which have not landed yet; until they do, use these
stages on datagram paths or on a peer that already sends one message per read.

A chunk with an odd number of digits is reported as a configuration error rather
than relayed:

```
message has an odd number of hex digits: unhexify decodes one complete message per call, so it needs an unframe stage ahead of it
```

The mistake that cannot be caught is a chunk cut on an even offset, which
decodes to a short payload. Hex has the shortest group of any codec here, so
that is also the likeliest: half of all cuts land on one, against a quarter for
base64. Framing is what rules it out.

## Case

`hexify` writes lowercase by default. `unhexify` accepts both cases and mixtures
of the two, whatever the encoding end is set to, so `case` never has to match
across a hop the way base64's `alphabet` does. Set it for the benefit of
whatever is reading the wire.

Whitespace is not stripped. A trailing newline is a frame delimiter, and
swallowing it here would paper over framing that is missing or misconfigured.
Nor is a `0x` prefix accepted, for the same reason: this is a wire codec, not a
parser for hex written for people.

## Size

Hex is a 2-to-1 expansion, exactly, with no padding: an encoded message is
always twice the payload. On a datagram path that is the difference between
fitting in a datagram and not, and a starker one than base64's: a 1400-byte
payload leaves as 2800 bytes, where base64 would send 1868.
