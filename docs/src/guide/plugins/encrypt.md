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

| Option           | Plugin    | Description                                                                 |
| ---------------- | --------- | --------------------------------------------------------------------------- |
| `cipher=NAME`    | both      | Which cipher. Required. See [Ciphers](#ciphers)                             |
| `key=TEXT`       | both      | The key, written in `key-format`                                            |
| `key-file=PATH`  | both      | File holding the key                                                        |
| `key-env=NAME`   | both      | Environment variable holding the key                                        |
| `key-format=FMT` | both      | `hex` (default), `base64` or `raw`                                          |
| `mode=MODE`      | both      | `record` or `stream`. Defaults by cipher. See [Two modes](#two-modes)       |
| `padding=P`      | both      | `pkcs7` (default) or `none`. Only for `ecb` and `cbc`                       |
| `rotate-after=N` | both      | Start a fresh session every N bytes of ciphertext. `stream` mode, not `ecb` |
| `on-fail=WHAT`   | `decrypt` | `error` (default), `drop` or `halt` for a record that will not open         |
| `random-key`     | `encrypt` | Generate a key here and report it                                           |
| `key-out=PATH`   | `encrypt` | Where a generated key is written. Defaults to stderr                        |

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

## Ciphers

| Cipher                                      | Family    | Key        | Per record            | Default mode |
| ------------------------------------------- | --------- | ---------- | --------------------- | ------------ |
| `aes-128-gcm`, `aes-192-gcm`, `aes-256-gcm` | AEAD      | 16, 24, 32 | 28 bytes              | `record`     |
| `aes-128-gcm-siv`, `aes-256-gcm-siv`        | AEAD      | 16, 32     | 28 bytes              | `record`     |
| `chacha20-poly1305`                         | AEAD      | 32         | 28 bytes              | `record`     |
| `xchacha20-poly1305`                        | AEAD      | 32         | 40 bytes              | `record`     |
| `ascon-128`                                 | AEAD      | 16         | 32 bytes              | `record`     |
| `aes-128-ctr`, `aes-192-ctr`, `aes-256-ctr` | keystream | 16, 24, 32 | 16 bytes              | `stream`     |
| `aes-128-ofb`, `aes-192-ofb`, `aes-256-ofb` | keystream | 16, 24, 32 | 16 bytes              | `stream`     |
| `aes-128-cbc`, `aes-192-cbc`, `aes-256-cbc` | block     | 16, 24, 32 | 16 bytes plus padding | `stream`     |
| `aes-128-ecb`, `aes-192-ecb`, `aes-256-ecb` | block     | 16, 24, 32 | padding only          | `stream`     |

Names are matched the way every identifier in tocat is, so `aes-256-gcm`,
`AES256GCM` and `aes_256_gcm` are the same cipher.

`aes-256-gcm` and `chacha20-poly1305` are the two to pick between without a
reason to do otherwise: pick ChaCha20-Poly1305 on hardware without AES
instructions, and AES-GCM everywhere else. `xchacha20-poly1305` is worth its
extra twelve bytes on a path carrying enormous numbers of records, since its
longer nonce is what makes a random one safe that far out. `ascon-128` is the
NIST lightweight selection, for talking to something small that already speaks
it.

The rest are unauthenticated. They keep the payload secret and do nothing at all
about tampering: anyone who can change bytes in transit can change the plaintext
that comes out, and `decrypt` will hand it downstream without complaint. ECB
goes further and encrypts identical blocks identically, so the shape of the
plaintext survives into the ciphertext. Use them to interoperate with something
that requires them.

Among those, CTR is the one that goes fast. CBC and OFB chain one block into the
next and are several times slower to encrypt for that reason; see [Cost](#cost).

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
attacker does deliberately. CTR and OFB cannot tell a good record from a bad one
at all.

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
`ctr` and `ofb`, where it bounds the keystream drawn under one nonce, and `cbc`,
where it bounds the chain. `ecb` has no nonce and is refused. So is `record`
mode, where every record already draws its own.

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
stay busy. CBC and OFB cannot: each block's encryption feeds the next, so the
work is a chain of latencies with nothing to overlap, and they run several times
slower than CTR under the same key. This is a property of the modes rather than
of this implementation, and it is asymmetric for CBC, whose decryption has no
such chain and runs at CTR-like speed. A CBC path is therefore much slower in
one direction than the other.

**How many rounds.** AES-256 is fourteen rounds against AES-128's ten, and pays
that difference on the encryption pass only. The authentication pass does not
care about key length, so the gap between 128 and 256 is wider for the
unauthenticated modes than for the AEAD ones.

**Whether the hardware has the instructions.** AES-GCM wants AES and carry-less
multiply instructions, which every x86-64 part worth relaying on and most ARM
ones have. ChaCha20-Poly1305 wants vector instructions instead, and is the one
to reach for where the AES instructions are missing. `ascon-128` is designed for
small constrained hardware rather than for throughput here, and is slower than
either on a machine of this kind.

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
