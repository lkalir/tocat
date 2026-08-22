# `encrypt` and `decrypt` - symmetric encryption

Behind the `encrypt` cargo feature. Encryption is asymmetric, so the two are
paired, one per path:

```console
$ tocat - encrypt,cipher=aes-256-ctr,key-file=k.hex decrypt:reverse,cipher=aes-256-ctr,key-file=k.hex tcp:relay.internal:9000
```

`direction=both` would encrypt the replies too and hand the peer ciphertext it
would try to decrypt.

The far end of the link runs the mirror image, and the two relays form an
encrypted tunnel over a hop that would otherwise carry plaintext.

| Option           | Plugin    | Description                                                                     |
| ---------------- | --------- | ------------------------------------------------------------------------------- |
| `cipher=NAME`    | both      | Which cipher. Required. See [Ciphers](#ciphers)                                 |
| `key=TEXT`       | both      | The key, written in `key-format`                                                |
| `key-file=PATH`  | both      | File holding the key                                                            |
| `key-env=NAME`   | both      | Environment variable holding the key                                            |
| `key-format=FMT` | both      | `hex` (default), `base64` or `raw`                                              |
| `mode=MODE`      | both      | `record` or `stream`. Defaults by cipher. See [Two modes](#two-modes)           |
| `padding=P`      | both      | `pkcs7` (default) or `none`. Only for `ecb` and `cbc`                           |
| `rotate-after=N` | both      | Start a fresh session every N bytes of ciphertext. `stream` mode, needs a nonce |
| `nonstandard`    | both      | Allow a cipher no document specifies. See [Standards](#standards)               |
| `on-fail=WHAT`   | `decrypt` | `error` (default), `drop` or `halt` for a record that will not open             |
| `random-key`     | `encrypt` | Generate a key here and report it                                               |
| `key-out=PATH`   | `encrypt` | Where a generated key is written. Defaults to stderr                            |

Exactly one of `key`, `key-file`, `key-env` and `random-key` is required.

Every option except `on-fail`, `random-key` and `key-out` has to match at both
ends. Nothing is negotiated and nothing is described on the wire, so a
mismatched `cipher`, `mode`, `padding` or `rotate-after` is a stream neither end
can read.

Both default to `detach`, since they do enough work per byte to be worth their
own task. `detach=false` is accepted if you would rather have them inline.

## Two modes

`record` mode makes each chunk a self-contained unit:

```
nonce || ciphertext || tag
```

A fresh nonce is drawn for every record, so nothing is carried between them and
a lost or reordered message costs exactly that message. This is the mode for a
datagram path, and the only mode the authenticated ciphers have.

`stream` mode makes the whole path one session:

```
nonce || ciphertext ...
```

The nonce goes out once, ahead of the first byte, and the cipher's state
continues across every chunk after it. Nothing frames it, so no
[`frame`](frame.md) stage is needed, and it is the default for the ciphers that
allow it.

The choice is between where the overhead goes and what a lost byte costs. A
session pays for one nonce and then nothing; a record pays for a nonce and a tag
every time, and on a datagram path that is what makes each datagram readable on
its own. A session that loses a byte anywhere never recovers.

## Records need boundaries

A record has to arrive exactly as it left, so `encrypt` in `record` mode
requires that the units it emits survive downstream, and `decrypt` requires
whole records arriving. On a datagram path that comes for free. On a byte stream
it comes from [`frame` and `unframe`](frame.md), and without them tocat refuses
to start:

```
encrypt on source-to-sink needs the units it emits to survive, and the destination tcp://host:9000 is a
byte stream: put a frame below it, or use a stage that does not need them
```

The pair belongs immediately outside the encryption, so the forward path frames
what it encrypted and the reverse path unframes before decrypting:

```console
$ tocat - encrypt,cipher=aes-256-gcm,key-file=k.hex decrypt:reverse,cipher=aes-256-gcm,key-file=k.hex frame unframe:reverse tcp:relay.internal:9000
```

`frame,mode=length` is the one to reach for here: the payload is ciphertext, so
it can contain any byte, and a delimiter would have to be escaped.

## Which cipher to pick

If nothing else decides it, use `aes-256-gcm`, or `chacha20-poly1305` on
hardware without AES instructions. Both authenticate, both are `record` mode,
and both cost 28 bytes a record.

Everything else in the table below is there to talk to something that already
speaks it. The list is long because a mode and a block cipher combine freely,
not because a hundred and twenty three of them are worth choosing.

`xchacha20-poly1305` earns its extra twelve bytes on a path carrying enormous
numbers of records, since its 24-byte nonce is what makes a random one safe that
far out. `ascon-aead128` is the NIST lightweight selection, for talking to
something small. `aes-256-siv` is nonce-misuse resistant, which matters where
the same nonce may be drawn twice.

## Ciphers

A name is `primitive-keybits-mode`, so `aria-192-ctr` is ARIA with a 192-bit key
in counter mode. Names are matched the way every identifier in tocat is, so
`aes-256-gcm`, `AES256GCM` and `aes_256_gcm` are the same cipher.

Every combination below exists, at each key size listed:

| Primitive    | Block | `ecb`, `cbc`, `cfb`, `ctr`, `ofb` | `ccm`, `gcm`, `gcm-siv`, `ocb` | Specified by         |
| ------------ | ----- | --------------------------------- | ------------------------------ | -------------------- |
| `aes`        | 16    | 128, 192, 256                     | 128, 192, 256                  | FIPS 197             |
| `aria`       | 16    | 128, 192, 256                     | 128, 192, 256                  | RFC 5794             |
| `camellia`   | 16    | 128, 192, 256                     | 128, 192, 256                  | RFC 3713             |
| `sm4`        | 16    | 128                               | 128                            | GB/T 32907-2016      |
| `kuznyechik` | 16    | 256                               | 256, except `ccm`              | GOST R 34.12-2015    |
| `magma`      | 8     | 256                               |                                | GOST R 34.12-2015    |
| `des`        | 8     | 64                                |                                | FIPS 46-3, withdrawn |
| `des-ede3`   | 8     | 192                               |                                | NIST SP 800-67       |

Plus the ciphers that are not a mode over a block cipher:

| Cipher                       | Family    | Key    | Nonce  | Per record |
| ---------------------------- | --------- | ------ | ------ | ---------- |
| `chacha20-poly1305`          | AEAD      | 32     | 12     | 28 bytes   |
| `xchacha20-poly1305`         | AEAD      | 32     | 24     | 40 bytes   |
| `ascon-aead128`              | AEAD      | 16     | 16     | 32 bytes   |
| `aes-128-siv`, `aes-256-siv` | AEAD      | 32, 64 | 16     | 32 bytes   |
| `chacha20`, `xchacha20`      | keystream | 32     | 12, 24 | nonce only |
| `salsa20`, `xsalsa20`        | keystream | 32     | 8, 24  | nonce only |
| `rc4`                        | keystream | 32     | none   | nothing    |

The two SIV key sizes are the sum of the two keys RFC 5297 splits them into, so
`aes-128-siv` takes 32 bytes and `aes-256-siv` takes 64.

AES, ARIA and Camellia come in three key sizes, and leaving it out means the
largest: `aes-ctr` is `aes-256-ctr`, and a bare `aes` is `aes-256-gcm`. The
others have one size each and carry none in the name, so it is `sm4-ctr` and
`kuznyechik-gcm`; bare `sm4` is `sm4-gcm` and bare `magma` is `magma-ecb`.

The rest of the aliases are spellings: `3des-` and `tdes-` for the `des-ede3-`
modes, `grasshopper-` for the `kuznyechik-` ones, `chacha`, `xchacha`, `salsa`
and `xsalsa` for the bare keystreams, `chachapoly` and `xchachapoly` for the two
Poly1305 AEADs, and `ascon` for `ascon-aead128`.

### What each mode costs on the wire

| Mode                           | Family    | Per record                    | Default mode | Authenticated |
| ------------------------------ | --------- | ----------------------------- | ------------ | ------------- |
| `gcm`, `gcm-siv`, `ccm`, `ocb` | AEAD      | nonce plus a 16-byte tag      | `record`     | yes           |
| `ctr`, `ofb`, `cfb`            | keystream | one block of IV               | `stream`     | no            |
| `cbc`                          | block     | one block of IV, plus padding | `stream`     | no            |
| `ecb`                          | block     | padding only                  | `stream`     | no            |

The AEAD modes all take a 12-byte nonce here, so a record costs 28 bytes.

The IV a keystream mode transmits is the primitive's block, so it is 8 bytes for
`des`, `des-ede3` and `magma` and 16 elsewhere. GOST counter mode is the one
exception: `magma-ctr` puts 4 bytes on the wire and `kuznyechik-ctr` puts 8,
because GOST starts the counter at the transmitted half followed by zeroes.

`rc4` has no IV at all. That makes `mode=record` unsafe with it, since every
record would then be encrypted under the same keystream from the start, and it
is also why `rotate-after` is refused for it.

The unauthenticated modes keep the payload secret and do nothing at all about
tampering: anyone who can change bytes in transit can change the plaintext that
comes out, and `decrypt` will hand it downstream without complaint. ECB goes
further and encrypts identical blocks identically, so the shape of the plaintext
survives into the ciphertext.

Among them CTR is the one that goes fast. CBC, CFB and OFB chain one block into
the next and are several times slower to encrypt for that reason; see
[Cost](#cost).

`des` has a 56-bit effective key and is breakable by anyone who cares to. The
8-byte block ciphers, `des`, `des-ede3` and `magma`, also reach the birthday
bound after a few tens of gigabytes under one key, which on a relay is an
afternoon rather than a theoretical limit.

## Standards

Every cipher records the document that says what its bytes are, and a
combination that no document specifies is refused:

```
invalid configuration for plugin `decrypt`: camellia-256-gcm-siv is not specified
by any document, so nothing else is likely to read it: pass nonstandard to use it
anyway
```

These are combinations the crates behind them will happily compute, and which
round trip against tocat at both ends, but which no other implementation is
likely to agree with byte for byte. They are the `gcm-siv` rows other than
`aes-128-gcm-siv` and `aes-256-gcm-siv`, RFC 8452 having specified GCM-SIV for
AES at those two sizes and nothing else.

`nonstandard` opts in, and both ends need it, since both refuse without it:

```console
$ tocat - 'encrypt,cipher=sm4-gcm-siv,key-file=k.hex,nonstandard' ...
```

The flag is about whether a document defines the bytes, not about whether the
choice is wise. A withdrawn standard still counts as one, which is why `des-cbc`
needs no opt-in.

## Authentication and what happens without it

An AEAD cipher tags every record, and `decrypt` checks the tag before it emits
anything. A record that fails is never forwarded; `on-fail` decides what happens
instead.

| `on-fail` | What happens                                                            |
| --------- | ----------------------------------------------------------------------- |
| `error`   | The path fails and the relay reports why. The default                   |
| `drop`    | The record is discarded, a warning is logged, and the next one is tried |
| `halt`    | Reading upstream stops, as if the peer had closed                       |

`drop` is what a datagram path usually wants, where one bad message among many
is a thing to note rather than a reason to stop. `error` is the safer default
everywhere else: a record that fails is either a misconfiguration or someone
changing bytes in transit, and neither is a thing to carry on through quietly.

`on-fail` is a `record` mode option. Dropping part of a session desynchronises
everything after it, so a session that cannot be decoded ends the path.

The unauthenticated ciphers have no tag to check. CBC and ECB still reject
malformed padding, which catches a wrong key most of the time and nothing an
attacker does deliberately. The keystream modes, CTR, OFB, CFB and the bare
stream ciphers, cannot tell a good record from a bad one at all.

## Keys

The key is bytes, and every source below gives the same bytes, so a key written
as hex in one place is the same key as its base64 elsewhere. A key of the wrong
length for the cipher is rejected at startup rather than truncated or padded.

```console
$ tocat - encrypt,cipher=aes-256-gcm,key-file=/etc/tocat/relay.key ... 
$ tocat - encrypt,cipher=aes-256-gcm,key-env=RELAY_KEY ...
```

`key-format=raw` reads a file's bytes exactly, which is what
`head -c 32
/dev/urandom > relay.key` produces. `hex` and `base64` ignore
whitespace, so a file with a trailing newline reads as intended.

`key=` on the command line puts the key in `ps` output and in shell history. It
is there for a throwaway and for tests; `key-file` or `key-env` is what to reach
for otherwise.

`random-key` generates one and reports it on stderr, or to `key-out`:

```console
$ tocat file:secrets.tar 'encrypt,cipher=aes-256-gcm,random-key,key-out=key.txt' frame file:secrets.enc
```

Nothing else can hold that key, so the far end of a link cannot be configured to
match it. It is for encrypting something you will decrypt yourself later, with
the key the stage wrote down. The line is written once, when the path first
carries bytes, in the same shape `hash` uses:

```text
0001…1e1f  key (aes-256-gcm) [file:secrets.tar -> file:secrets.enc | encrypt]
```

stdout is refused, since on a stdio endpoint it carries payload.

## Rotation

`rotate-after=N` ends a session every N bytes and starts another, with a fresh
nonce on the wire between them. It applies wherever there is a nonce to replace:
the keystream modes, where it bounds the keystream drawn under one nonce, and
`cbc`, where it bounds the chain. `ecb` and `rc4` have no nonce and are refused.
So is `record` mode, where every record already draws its own.

```console
$ tocat - 'encrypt,cipher=aes-256-ctr,key-file=k.hex,rotate-after=1GiB' ...
```

N counts **ciphertext**, not payload, and it is what each closed session puts on
the wire after its nonce. For the keystream modes those are the same number. For
`cbc` they are not: a session ends with a padding block, so a 64-byte budget
carries three blocks of payload and one of padding, and how much payload rides
in that last block depends on where the sender's chunks fell. Counting
ciphertext is what lets the receiver find the next nonce, since it chunks
differently and has nothing else to go on.

For a block mode the budget must therefore be a whole number of blocks, and with
`pkcs7` at least two of them, since one would be all padding and no payload.
Both ends must be given the same `rotate-after`.

What rotation does not do is change the key. The amount of data one key can
safely carry is set by the key and the cipher's block size, and a fresh nonce
does not extend it. That matters most for a cipher with a small block, where the
bound arrives sooner than anyone expects. Rotation bounds what a single nonce
covers; it is not a way to keep one key in service for longer.

## Datagram paths

Use `record` mode. Each datagram is then one self-contained record: it is
readable on its own, a lost datagram costs nothing but itself, and reordering
does not matter.

`stream` mode on a datagram path is what the boundary warning is about:

```
stage may not preserve message boundaries; datagrams sent to this endpoint may be split, merged, or malformed
```

A session assumes every byte arrives once and in order, which is exactly what a
datagram path does not promise. One lost datagram desynchronises everything
after it, and the far end will carry on emitting plaintext that is not what was
sent.

## Cost

What a cipher costs depends on the machine and on how the binary was built, so
the useful thing to know is not a number but which of four things you are paying
for. Measure the rest yourself: put a [`rate`](rate.md) stage next to the
encryption and read it off.

```console
$ tocat file:/dev/zero limit,bytes=10GiB 'encrypt,cipher=aes-128-gcm,random-key' rate,interval=1s frame,mode=length file:/dev/null
```

**One pass or two.** The unauthenticated modes make one pass over the payload.
The authenticated ones make two, one to encrypt and one to authenticate, so they
cost roughly the cipher again. That is the price of the tag, and it is what you
are buying when you pick an AEAD cipher.

**Whether the blocks can overlap.** CTR and ECB encrypt every block
independently, so a modern CPU keeps several in flight at once and its AES units
stay busy. CBC, CFB and OFB cannot: each block's encryption feeds the next, so
the work is a chain of latencies with nothing to overlap, and they run several
times slower than CTR under the same key. This is a property of the modes rather
than of this implementation, and it is asymmetric for CBC, whose decryption has
no such chain and runs at CTR-like speed. A CBC path is therefore much slower in
one direction than the other.

**How many rounds.** AES-256 is fourteen rounds against AES-128's ten, and pays
that difference on the encryption pass only. The authentication pass does not
care about key length, so the gap between 128 and 256 is wider for the
unauthenticated modes than for the AEAD ones.

**Whether the hardware has the instructions.** AES-GCM wants AES and carry-less
multiply instructions, which every x86-64 part worth relaying on and most ARM
ones have. ChaCha20-Poly1305 wants vector instructions instead, and is the one
to reach for where the AES instructions are missing. The other primitives here
run in software whatever the machine has, so expect any of them to be slower
than AES by a wide margin. `ascon-aead128` is designed for small constrained
hardware rather than for throughput, and is slower than either on a machine of
this kind.

Beyond the cipher itself, encryption copies each chunk once, and decryption of a
block mode copies twice, since it accumulates whole blocks and holds the last
one back until it knows it is not the padded tail. Both stages detach by
default, so they run concurrently with the endpoints, and the slowest stage sets
the rate for the path.

Record overhead is per record, so it is set by how often the pipeline calls the
stage rather than by the size of the transfer. On a byte stream with a 256 KiB
buffer, AES-GCM's 28 bytes are nothing; on a datagram path carrying 100-byte
messages they are 28 percent, and `frame,mode=length` above them would add four
more. The same is true of the work done once per record rather than once per
byte: a fresh IV, and for the unauthenticated modes a fresh key schedule, both
invisible against a large buffer and worth counting against a small message.
