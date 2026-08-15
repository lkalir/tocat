# `frame` and `unframe` - message boundaries

Puts message boundaries on a byte stream and takes them off again. The two are
paired, one per path:

```console
$ tocat - frame unframe:reverse tcp:relay.internal:9000
```

`direction=both` would frame the replies too, which is only right if the peer
speaks the same framing back.

## What they are for

On a datagram path a chunk is a message and there is nothing to do. On a byte
stream a chunk is an arbitrary slice, so a stage that needs whole messages
([`unbase64`](base64.md), anything with a per-message header) has no way to find
them. `unframe` is where that knowledge lives: it accumulates until it has a
whole message, then emits it as one unit, and the pipeline turns each unit into
one call at every stage below it.

`frame` is the inverse, and it takes the boundaries it is given: one unit in,
one framed unit out. That makes it meaningful on a datagram path, or after a
stage that declared boundaries of its own such as [`block`](block.md), and
pointless anywhere else, where a chunk is whatever the last read happened to
return.

| Option            | Plugin    | Description                                                                                                |
| ----------------- | --------- | ---------------------------------------------------------------------------------------------------------- |
| `mode=NAME`       | both      | `delimiter`, `cobs`, `slip`, `length` or `netstring`. Default is `delimiter`. Must match at both ends      |
| `delimiter=BYTES` | both      | Terminator in `delimiter` mode, with `\n`, `\r`, `\t`, `\0`, `\\` and `\xNN` escapes. Default is a newline |
| `length-bytes=N`  | both      | Header width in `length` mode: 1, 2, 4 or 8. Default is 4                                                  |
| `endian=NAME`     | both      | Header byte order in `length` mode: `big` or `little`. Default is `big`                                    |
| `check`           | `frame`   | Reject a message that would frame as two, in `delimiter` mode. On by default                               |
| `max-message=N`   | `unframe` | Largest message to accept. Default is 1MiB; `0` removes the limit                                          |
| `at-eof=NAME`     | `unframe` | `emit`, `error` or `drop` for a trailing partial message. Default depends on the mode                      |

An option a mode ignores is an error rather than a no-op, so
`mode=cobs,delimiter=\n` is refused instead of quietly going on using a zero
byte.

`frame` preserves the message stream it is handed, so it may sit on a datagram
path. `unframe` replaces that stream with the sender's framing, so it warns
there, and there is nothing for it to do on a path that already has messages.

## Modes

Five, in two families. The terminator family scans for a byte string and pays
for a payload that could contain it. The counted family reads a header and pays
nothing for the payload at all.

| Mode        | Framing                           | Overhead per message     | Payload                         |
| ----------- | --------------------------------- | ------------------------ | ------------------------------- |
| `delimiter` | a byte string, newline by default | the delimiter            | must not contain the delimiter  |
| `cobs`      | zero byte, payload stuffed        | 1 byte + 1 per 254       | any                             |
| `slip`      | `0xc0`, payload escaped           | 1 byte + 1 per `c0`/`db` | any                             |
| `length`    | fixed-width prefix                | 1, 2, 4 or 8 bytes       | any, up to what the width holds |
| `netstring` | `LEN:payload,`                    | 3 bytes or so            | any                             |

### `delimiter`

Cheap, greppable, and only correct when the payload cannot contain the
delimiter. That pairs naturally with [`base64`](base64.md), whose output is text
by construction:

```console
$ tocat - base64 unbase64:reverse frame unframe:reverse tcp:relay.internal:9000
```

Encode then frame going out, unframe then decode coming back: the reverse path
walks the entry list backwards, so writing each pair in forward order makes the
two nest correctly.

### `cobs` and `slip`

Both escape the payload so the terminator cannot appear in it, which makes them
exact for arbitrary binary.

[COBS][cobs] costs one byte per frame plus one per 254 bytes of payload, and a
receiver that joins a stream mid-flight resynchronises at the next zero byte.

[SLIP][slip] escapes `0xc0` and `0xdb`, so its overhead depends on the payload
and doubles in the worst case. It is here because serial hardware and embedded
stacks already speak it: reach for it to talk to something that requires it, and
for COBS otherwise. An empty SLIP frame is a flush rather than a message, since
RFC 1055 has senders lead with a terminator to clear line noise, so `unframe`
skips it. An escape followed by anything but `0xdc` or `0xdd` is undefined in
the RFC and is an error here, rather than passing a peer's bug through as
payload.

[cobs]: https://doi.org/10.1109/90.769765
[slip]: https://www.rfc-editor.org/rfc/rfc1055

### `length` and `netstring`

A header says how long the message is, so the payload is written and read
untouched and the overhead does not depend on it. These are also the only modes
that know how big a message is before reading it, which is what lets
`max-message` reject an oversized one from its header instead of after buffering
it.

`length` writes a fixed-width big-endian prefix, which is what most binary
protocols on a TCP hop already speak. Set `endian=little` for a peer that got
that wrong. A message too large for the width is refused by `frame` rather than
truncated into a header the far end could never resynchronise from.

`netstring` is [the djb format][netstring], `length:payload,` with a decimal
length. Same properties, with a self-describing header you can read in a hex
dump, and the trailing comma turns a desynchronised stream into an error on the
very next message instead of never. The length must be canonical: no sign, no
spaces, and no leading zero to pad the header out with.

[netstring]: https://cr.yp.to/proto/netstrings.txt

Neither counted mode can resynchronise. A receiver that joins mid-stream, or a
sender that gets one length wrong, is lost until the connection is remade, where
the terminator modes recover at the next terminator.

## The delimiter has to be absent from the payload

Framing a message that contains the delimiter puts two messages on the wire, and
nothing downstream can tell that apart from two messages the sender meant.
`frame` scans for it rather than trusting the peer:

```
message would frame as two: it contains the delimiter, or ends with a prefix of one
```

The second half of that catches a subtler case, and only bites for a delimiter
that overlaps itself. A message ending in `a`, terminated by `aa`, puts `aaa` on
the wire, and the far end reads the boundary one byte early even though the
message contains no delimiter at all.

`check=false` turns the scan off for a peer whose parser is known to tolerate
the ambiguity. Any other mode makes the question go away.

## Bounded memory

`unframe` holds a partial message, so a peer whose framing does not match is a
peer asking the relay to buffer without limit. `max-message` caps the message
and defaults to 1MiB. Exceeding it is reported as a protocol error rather than
accommodated:

```
no complete message in 1048577 bytes (max-message is 1MiB): this stage's framing does not match the peer's
```

A counted mode does better, and refuses the message from its header before a
byte of the payload has been buffered:

```
a header declares 1073741824 bytes, over the max-message of 1MiB
```

The cap is on one message, not on the stream: a stream of small messages never
approaches it however long it runs. Framing bytes do not count against it, so a
message of exactly `max-message` still passes. `max-message=0` removes the cap,
which is worth doing only for a peer you control.

## What framing survives, and what it does not

Framing puts the boundary into the payload, so it outlives a stage below that
loses message boundaries: `block`, `compress`, `process`. That is what makes

```
tocat file:in.bin 'frame,mode=cobs' 'compress' tcp:collector:9000
```

meaningful where the same two stages without `frame` would leave the far end
unable to tell one message from the next.

What survives is the *bytes*, though, not the framing as such, and that puts two
conditions on the stage below.

The first is that the bytes have to come out the other end. A stage that copies
them (`block`) or transforms them reversibly (`compress`, or a `process` running
`gzip`) is fine, because the framing bytes are still in there. A stage that
drops or rewrites them is not: `process` with `argv = ["grep", "ERROR"]` throws
away whole frames and half of a COBS escape sequence with them, and the far end
sees a corrupt stream rather than fewer messages. tocat cannot tell the two
apart, and does not try: what a subprocess does to its stdin is not something
the relay can inspect.

The second is that the inverse has to run on the far side, above the `unframe`
rather than below it. The framing bytes are inside the compressed stream, so
they only reappear once it has been decompressed:

```
send:    ... -> frame -> compress -> tcp
receive: tcp -> decompress -> unframe -> ...
```

Reversing that nesting is also valid and means something else. `compress` above
`frame` frames whatever chunks the compressor emits rather than the messages it
was given, and the far end recovers those chunks, not your messages.

Two smaller things about a subprocess in the middle. Pick a binary-safe mode:
anything that emits arbitrary bytes rules out `delimiter` unless something
upstream guarantees the payload cannot contain it, which is what `cobs` is for.
And check the child flushes. libc block-buffers stdout when it is a pipe, so a
framed message can sit in the child waiting for 4KiB of company that never
arrives, which on a relay is a stall rather than a slowdown. `stdbuf -o0`, or
whatever the program's own flag is.

## The end of the stream

A stream can end part way through a message. `at-eof` decides what happens to
those bytes: `emit` sends them as a final message, `error` fails the path, and
`drop` discards them.

The default depends on the mode, because the same bytes mean different things. A
text stream whose last line has no newline is routine, so `delimiter` mode
emits. Everywhere else a partial frame was cut off in transit, so the default is
`error`. The counted modes refuse `at-eof=emit` outright: what they hold is a
header and part of a payload, and there is no message in it to emit.

## Empty messages

A zero-length message describes nothing a pipeline can carry: an empty unit is
not emitted, and a datagram sink drops an empty message rather than putting a
spurious zero-length datagram on the wire. Every mode can express one on the
wire, and none of them will deliver one. In `delimiter` mode a blank line is
therefore swallowed. In `cobs` mode two terminators in a row cannot come from a
conforming sender at all, so they are reported as a corrupt frame.
