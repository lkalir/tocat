# `hash` - digest the stream

Computes a digest of everything crossing this point on the path, and writes it
where you point it.

```console
$ tocat file:big.iso hash tcp:host:9000
$ tocat -f tcp-listen:9000,fork -t file:capture.bin -p 'hash,algo=blake3,file=digests.txt'
```

| Option          | Description                                                                          |
| --------------- | ------------------------------------------------------------------------------------ |
| `algo=NAME`     | Which digest. Default `sha256`. Aliases: `algorithm`, `alg`, `hasher`                |
| `summary=false` | Suppress the end-of-stream line, which is otherwise the whole point                  |
| `chunks`        | One line per chunk, each the digest of that chunk alone                              |
| `file=PATH`     | Where to write. Omitted, `-`, `stderr`, `/dev/stderr` or `/dev/fd/2` all mean stderr |
| `append=false`  | Truncate an existing file rather than appending to it                                |

`md5`, `sha1`, `sha224`, `sha256`, `sha384`, `sha512`, `sha3-224`, `sha3-256`,
`sha3-384`, `sha3-512`, `blake2` and `blake3` are available. Spelling is
forgiving, so `SHA-256` and `sha_256` are the same thing, and `sha2` and `blake`
name the common member of their family. `md5` and `sha1` are there for checking
against a legacy tool's output rather than for deciding whether two things are
the same.

Lines follow the `sha256sum` shape, with the algorithm and the hop appended:

```text
ba78…15ad  stream (sha256) [tcp://example.com:80_10.0.0.4:52134 -> STDIO | hash]
```

The trailing bracket is what makes a shared file readable. Entries naming the
same file share one writer, and under `fork` with the default `direction=both`
there are two instances per connection, so without the hop and the stage name
the lines could not be told apart. `as=` names an instance everywhere, including
here.

Like [`tee`](tee.md) and [`rate`](rate.md) it never touches the payload, so it
can go anywhere in a chain, including on a datagram path, and its position
decides only what it sees: before a [`compress`](compress.md) stage it digests
the payload, after it digests the wire. Two instances give you both.

```console
$ tocat file:big.iso 'hash,as=plain' compress 'hash,as=wire' tcp:relay:9000
```

Direction works the same way. The default `direction=both` builds one instance
per path, each with its own hasher, so a duplex connection produces two digests
rather than one over the concatenation, which would be a number with no meaning.

## Chunks

`chunks` reports the digest of each chunk *alone*, not a running value, which is
what makes it useful for locating where two captures diverge: the first line
that differs is the first chunk that differed. It costs a finalisation and a
write per chunk, so it is off by default.

A chunk is an arbitrary slice of a byte stream, decided by the copy buffer and
by what the peer sent, so the same bytes relayed twice do not have to produce
the same chunk lines. On a datagram path they do, since there a chunk is a
message. [`block`](block.md) above a `hash` makes the splits deterministic, at
the price of the framing it imposes.

## There is not always an end of stream

`summary` needs one, and two paths never reach it: a datagram source, and a
[`pipe`](../endpoints/pipe.md) held open across producers. On those, `chunks` is
the only thing that will ever report, which is the same reason `rate` reports on
a timer as well as at the end.

Turning both off is rejected at startup rather than accepted as a stage that
hashes nothing.
