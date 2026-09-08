# `noise`

Noise is a layer, like [`tls`](tls.md): a TCP transport with a handshake stacked
over it, and a byte stream at the top. Unlike TLS it has no certificates and no
names. A Noise peer is a key, and everything here is about which keys the two
sides have and when.

Reach for it when TLS is more machinery than the job needs: two ends you
control, no public certificate authority in the picture, a shared secret or a
pair of keys you can distribute yourself.

## The two schemes

| Scheme         | What it is                     |
| -------------- | ------------------------------ |
| `noise`        | Dial. TCP with a `noise` layer |
| `noise-listen` | Accept                         |

```console
$ tocat - noise:gateway.internal:7000,pattern=nnpsk0,psk-file=/etc/tocat/psk
$ tocat 'noise-listen:7000,fork,pattern=nnpsk0,psk-file=/etc/tocat/psk' tcp:localhost:9000
```

Both take the
[socket options](../endpoints.md#socket-options-which-the-socket-schemes-share)
of the TCP transport underneath.

## Pick a pattern

`pattern=` is required, and it is the only decision that really matters. There
is no default, because the only pattern that works with no key material is the
one that proves nothing.

A Noise pattern is two letters. The first is what the dialling side does with a
static key, the second what the listening side does:

| Letter | Meaning                                                    |
| ------ | ---------------------------------------------------------- |
| `N`    | No static key                                              |
| `K`    | A static key the other side already has                    |
| `X`    | A static key sent during the handshake, encrypted          |
| `I`    | A static key sent in the first message. Dialling side only |

All twelve combinations work: `nn`, `nk`, `nx`, `kn`, `kk`, `kx`, `xn`, `xk`,
`xx`, `in`, `ik`, `ix`.

Two rules follow from the letters, and they are the whole of the configuration:

- **You need `key` when your own letter is `K`, `X` or `I`.**
- **You need `peer` when the other side's letter is `K`**, because a `K` key is
  never sent and so has to be there already.

Where the other side's letter is `X` or `I` its key arrives during the
handshake, so `peer` is optional there. It is a pin, and setting it is what
turns the handshake from opportunistic into authenticated. Without one, any peer
that answers is accepted.

### Which to choose

**`nnpsk0` if you have one secret to share.** Nothing else to distribute, both
sides authenticated. The smallest thing that is actually secure.

**`xx` if each machine should have its own key.** Both sides send their static
key during the handshake, and each names the other's with `peer=`. Nothing has
to be known in advance, which makes it the easiest of the key-based patterns to
deploy.

**`ik` when the dialling side already knows who it is calling.** One round trip
shorter than `xx`, and the dialling side's own key stays hidden from anyone
watching. The dialling side must set `peer=`.

**`nk` for a server with a published key and anonymous clients.** The client
proves the server and stays unidentified itself.

**`nn` proves nothing.** A listener cannot read the traffic; anything in the
path can read it, change it, and impersonate either end. tocat logs a warning
naming any run whose configuration authenticates nobody, which includes `nn` and
also `xx` or `ix` left without a `peer=`. It exists for testing and for talking
to something that offers nothing better.

## Mixing in a shared secret

Any pattern takes a pre-shared key modifier: append `psk0`, `psk1`, `psk2` or
`psk3` to its name, as in `nnpsk0`, `xxpsk3` or `ikpsk1`. The number is which
handshake message the secret is folded into, and both sides must use the same
one.

```console
$ tocat - noise:gateway:7000,pattern=xxpsk2,key-file=id,peer=$THEIRS,psk-file=secret
```

Lower numbers protect more of the handshake; `psk0` mixes the secret in before
anything else is sent. Most patterns have two messages, so `psk3` only works
with `xn`, `xk` and `xx`, which have three. Using it elsewhere is refused before
anything connects.

A psk is not a replacement for `peer=` in a pattern that transmits a static key,
but it does authenticate both sides on its own, which is why `nnpsk0` needs no
keys at all.

## Cipher and hash

| Option    | Values                                   | Default      |
| --------- | ---------------------------------------- | ------------ |
| `cipher=` | `chachapoly`, `aesgcm`                   | `chachapoly` |
| `hash=`   | `blake2s`, `blake2b`, `sha256`, `sha512` | `blake2s`    |

**Both ends must be set the same.** Noise negotiates nothing, so a mismatch is a
failed handshake, not a fallback. The defaults are what WireGuard uses and are a
good choice unless something you are talking to needs otherwise.

The Diffie-Hellman function is always 25519.

## Keys

Every key here is 32 bytes. Generate one with any tool that emits random bytes:

```console
$ head -c 32 /dev/urandom | xxd -p -c 32 > /etc/tocat/psk
$ chmod 600 /etc/tocat/psk
```

| Option              | Description                                    |
| ------------------- | ---------------------------------------------- |
| `psk=`              | The shared secret, for any `psk` pattern       |
| `psk-file=PATH`     | The same, from a file                          |
| `psk-env=NAME`      | The same, from an environment variable         |
| `key=`              | This side's static private key                 |
| `key-file=PATH`     | The same, from a file                          |
| `key-env=NAME`      | The same, from an environment variable         |
| `peer=`             | The peer's static public key. Alias `peer-key` |
| `key-format=FORMAT` | `hex` (the default), `base64` or `raw`         |

The three sources for one key are alternatives, not a list, and giving two is an
error rather than a silent preference.

**Prefer `psk-file` and `key-file` to `psk` and `key`.** A key on the command
line is visible to anything that can read the process list. Whitespace is
ignored when decoding, so a file with a trailing newline reads as what it looks
like.

For any pattern with a `K`, `X` or `I` you need the public half to give the
other side. tocat does not derive it for you today, so generate the pair with a
tool that prints both.

## Pinning

Where the other side's letter is `X` or `I`, `peer=` is what turns encryption
into authentication. Without it the handshake succeeds with whoever answers,
exactly like `tls` with `verify=none`. With it, a peer presenting any other key
is refused once the handshake completes and the connection dropped.

Where the other side's letter is `K`, `peer=` is required rather than optional,
because that key is never sent and the handshake cannot be built without it.

## Rotating the key mid-connection

`rekey=N` replaces each direction's cipher key after every `N` records, which
bounds how much data sits under any one key. Both sides count the same records,
so nothing is negotiated and nothing is sent; set the same number on both ends.

```console
$ tocat - noise:gateway:7000,pattern=nnpsk0,psk-file=/etc/tocat/psk,rekey=4096
```

Leave it unset unless you have a reason. A session key already changes on every
connection, and the nonce is a 64 bit counter that no transfer will exhaust.

## Binding a session to something else

`prologue=` mixes text into the handshake. Both ends must write the same thing
or the handshake fails, which is a cheap way to make a key usable only in the
context you meant:

```console
$ tocat - noise:gateway:7000,pattern=nnpsk0,psk-file=/etc/tocat/psk,prologue=metrics-v2
```

## What this is not

**It is not the [`encrypt`](../plugins/encrypt.md) plugin, and neither replaces
the other.** `encrypt` transforms bytes with a key you already hold and needs no
peer at all, which is why it can encrypt a file into another file or into a
pipe. `noise` needs a live peer running the same handshake, and in exchange
derives a fresh key for every connection and can prove who the peer is. Use
`encrypt` for data going somewhere to be read later, and `noise` for a
connection between two processes.

**It is TCP only.** There is no `noise` over unix sockets or a subprocess today,
the same as for `tls`.

**There is no close notification.** A peer that hangs up cleanly and a
connection someone cut look the same. A record cut in half is caught and
reported; a cut between records is not distinguishable from the peer finishing.
Anything that needs to know a transfer was complete has to say so itself, above
tocat.

**It is a byte stream, not messages.** A Noise record holds whatever happened to
be in the buffer, so an endpoint with a `noise` layer is a stream endpoint like
`tls`, not a message one like [`ws`](ws.md).
