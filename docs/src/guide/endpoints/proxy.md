# `proxy` and `socks5`

Two ways to reach a peer through something else. Both are layers, both dial the
proxy as their transport and carry the real destination themselves, and both are
transparent once the tunnel is open: after the handshake the bytes are the
target's.

```console
$ tocat - proxy:proxy.corp:8080:api.example.com:443
$ tocat - socks5:127.0.0.1:1080:api.example.com:443
```

The address is `proxyhost:port:targethost:port`. The first two components are
the transport, which is what a scheme's address has always meant here, and
everything after is the target.

| Option                 | Description                                                      |
| ---------------------- | ---------------------------------------------------------------- |
| `target=HOST:PORT`     | The destination, if you would rather not write it in the address |
| `proxy-auth=USER:PASS` | Credentials, inline                                              |
| `proxy-auth-file=PATH` | Credentials from a file, first line                              |
| `proxy-auth-env=NAME`  | Credentials from an environment variable                         |
| `resolve-locally`      | `socks5` only. Resolve the target here instead of at the proxy   |

Naming more than one credential source is an error rather than a precedence
rule. Both layers are client only: a listening endpoint has nobody to ask for a
tunnel, and saying so is a parse error rather than a surprise at connect time.

## Which one

`proxy` is HTTP CONNECT: a request, a status line, and then the connection is
yours. It works through anything that fronts an HTTP proxy, which in a corporate
network is usually what exists.

`socks5` sends the target as a **name** unless you set `resolve-locally`, so the
proxy does the lookup. That is the reason to prefer it where both are available:
a host only the proxy's network can resolve still works, and no lookup for it
happens on your side. It also carries the reply codes, so a refusal arrives as
"not allowed by ruleset" rather than as a dropped connection.

SOCKS4a is not supported. Everything that speaks it speaks SOCKS5.

## Stacking TLS over a tunnel

This is the arrangement the layer model exists for, and the one that has to get
the name right: above the tunnel you are talking to the target, so the
certificate is checked against the target and not against the proxy.

There is no single scheme for it, because sugar spells one layer. Write the
stack:

```toml
[sink]
type = "tcp"
addr = "127.0.0.1:1080"

[[sink.layers]]
type = "socks5"
target = "api.example.com:443"

[[sink.layers]]
type = "tls"
```

Layers are bottom first, so this reads as: dial the proxy, ask it for a route to
`api.example.com:443`, then handshake TLS with `api.example.com` through it.

## Two things that look like bugs

**`curl -x` will not work against a tocat relay.** `-x` tells curl the address
is an HTTP proxy, so it sends `CONNECT` and waits for a `200`. A relay answers
nothing: it carries bytes to an endpoint that is already decided. If your sink
is `api.example.com:443`, point curl at the relay directly and let it speak the
protocol the far side expects.

**A name based virtual host needs the name in the request.** A relay does not
rewrite anything, so a request arriving with `Host: 127.0.0.1:12345` reaches the
target with that header and gets whatever the target does with an unknown host,
often a 404. Send the name you mean:

```console
$ curl -H 'Host: api.example.com' http://127.0.0.1:12345
```

## What is not here yet

`proxy` offers only basic authentication; digest and NTLM are not supported.
`socks5` does not do UDP association or BIND, so it tunnels outbound TCP and
nothing else. Neither layer can accept a connection, so tocat cannot be a proxy,
only use one.
