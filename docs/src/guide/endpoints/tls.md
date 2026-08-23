# `tls`

TLS is a layer rather than a plugin. A plugin stage is synchronous and pure and
has no way to send bytes back upstream, and a handshake is a conversation with
the peer, so it belongs to the endpoint. `tls:example.com:443` is a TCP
transport with one TLS layer over it, and `--dump-config` prints that composed
form.

## `tls` - dial a TLS server

Aliases: `ssl`, `openssl`. The target is the same `host:port` that
[`tcp`](tcp.md) takes, and the transport options work as they do there.

```console
$ tocat - tls:example.com:443
$ tocat tcp-listen:8080,fork 'tls:backend.internal:443,cafile=/etc/ca.pem'
```

| Option            | Description                                                                         |
| ----------------- | ----------------------------------------------------------------------------------- |
| `cafile=PATH`     | Trust anchors from a PEM file instead of the platform's store. Alias `ca`           |
| `pin=FINGERPRINT` | Accept the certificate with this SHA-256, ignoring the chain                        |
| `verify=none`     | Accept any certificate. See below                                                   |
| `servername=NAME` | The name to send in SNI and check against. Default is the host dialled. Alias `sni` |
| `alpn=PROTOCOL`   | A protocol to offer. Repeat for more, in preference order                           |
| `cert=PATH`       | A certificate to present, PEM. The client half of mutual TLS, with `keyfile`        |
| `keyfile=PATH`    | The private key matching `cert`. Alias `key`                                        |

Plus the
[socket options](../endpoints.md#socket-options-which-the-socket-schemes-share)
and the
[resilience options](../endpoints.md#resilience-on-the-schemes-that-can-be-reopened)
of the TCP transport underneath, except `reconnect=keep`, which is refused under
a layer.

## `tls-listen` - accept TLS connections

```console
$ tocat 'tls-listen:8443,fork,cert=/etc/cert.pem,keyfile=/etc/key.pem' tcp:localhost:8080
```

| Option             | Description                                              |
| ------------------ | -------------------------------------------------------- |
| `cert=PATH`        | The certificate chain to present, PEM. Required          |
| `keyfile=PATH`     | The private key, PEM. Required. Alias `key`              |
| `client-auth=MODE` | Ask clients for a certificate. See below                 |
| `cafile=PATH`      | Who may issue client certificates. Needed with the above |
| `alpn=PROTOCOL`    | A protocol to accept. Repeat for more                    |

Plus everything [`tcp-listen`](tcp.md) takes, including `fork`.

## Mutual TLS

Both sides can present a certificate and both can check one. The same four keys
do it, read from whichever side they are on:

- **`cert=` and `keyfile=`** are the identity this side presents. Required on
  `tls-listen`, optional on `tls`, where supplying them is the client half.
- **`cafile=`** is who this side trusts. On `tls` it replaces the platform store
  as who may vouch for the server. On `tls-listen` it names who may issue a
  client certificate.

```console
$ tocat 'tls-listen:8443,fork,cert=server.pem,keyfile=server-key.pem,cafile=ca.pem,client-auth=required' \
    tcp:localhost:8080

$ tocat - 'tls:api.internal:8443,cafile=ca.pem,cert=client.pem,keyfile=client-key.pem'
```

`client-auth` has three settings. `required` refuses a client that presents no
certificate. `optional` asks for one and checks it if it arrives, but accepts a
client that presents none, so it authenticates the clients that have a
certificate without restricting who may connect. `none` is the default and asks
for nothing.

Two combinations are refused rather than accepted quietly:

- **`client-auth` without `cafile=`.** Falling back to the platform store would
  mean every public authority could vouch for anyone connecting, which is not a
  restriction anybody wants. Who may issue a client certificate has no default
  worth guessing.
- **`cafile=` on a listener with `client-auth=none`.** Nothing would consult it,
  and an option that sits there looking configured is worse than an error.

To try it with [mkcert](https://github.com/FiloSottile/mkcert):

```console
$ mkcert localhost
$ mkcert -client localhost
$ CA="$(mkcert -CAROOT)/rootCA.pem"

$ tocat "tls-listen:8443,fork,cert=./localhost.pem,keyfile=./localhost-key.pem,cafile=$CA,client-auth=required" \
    tcp:localhost:8080
```

## Talking to something with a self-signed certificate

The usual reason to reach for `verify=none` is an appliance that presents a
certificate no public authority signed. There are two better answers, and both
are real checks rather than the absence of one.

If you have the issuing certificate, name it:

```console
$ tocat - 'tls:appliance.local:443,cafile=/etc/appliance-ca.pem'
```

If you do not, pin the certificate itself. Its fingerprint is stable until it is
replaced, so this authenticates the peer even though nothing signed it:

```console
$ openssl s_client -connect appliance.local:443 </dev/null 2>/dev/null \
    | openssl x509 -noout -fingerprint -sha256
SHA256 Fingerprint=2F:8A:...

$ tocat - 'tls:appliance.local:443,pin=sha256:2F:8A:...'
```

The colons are optional and the case does not matter. `pin=` checks the leaf
certificate and nothing else: no chain, no expiry, no name. That is the trade,
and it is a reasonable one against a peer whose certificate you have seen.

`pin=` and `verify=none` together are refused, because they contradict each
other.

## `verify=none`

```console
$ tocat - 'tls:appliance.local:443,verify=none'
```

The connection is encrypted and the peer is unauthenticated. Anything able to
sit between you and the peer can present its own certificate, and TLS will
accept it, so this protects against a passive listener and not against an active
one.

It is spelled as a word rather than as a number so that a config file carrying
it reads as an admission, and every run that uses it logs a warning naming what
it gave up. That is deliberate: the failure mode of a verification bypass is
that it is invisible afterwards, and the one thing that can be done about it is
to make it noisy in the place it ended up.

The signature checks in the handshake still run, which proves the peer holds the
key in the certificate it sent. That is worth nothing on its own, since the
certificate is unverified, but it costs nothing and keeps the handshake honest.

## Boundaries

TLS declares `Fuse`. A record is not a message: one write by the peer may arrive
as several records or share one with the next write, so an endpoint with a TLS
layer carries a byte stream whatever is underneath it. A stage that needs
message boundaries is refused over TLS, for the same reason it is refused on
`tcp:`.

## The composed form

The command line spells the common stack as a single scheme. A config file can
write either, and `--dump-config` prints the composed one:

```toml
[source]
type = "tcp"
addr = "backend.internal:443"

[[source.layers]]
type = "tls"
cafile = "/etc/ca.pem"
alpn = ["h2"]
```

The two are the same endpoint. The composed form is what makes a deeper stack
expressible once there is more than one layer to stack.

## What is not here yet

DTLS is out of scope, so a layer over `udp:` is refused rather than attempted.
And `reconnect=keep` is refused under a layer, because reopening would have to
redo the handshake underneath a pipeline that is mid-stream; use
`reconnect=restart`.
