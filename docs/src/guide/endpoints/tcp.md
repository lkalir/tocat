# `tcp` and `tcp-listen`

## `tcp` - connect to a TCP socket

Aliases: `tcp-connect`, `connect`. The target is a `host:port` passed to the
resolver, so a name or a bracketed IPv6 literal both work.

```console
$ tocat - tcp:example.com:80
$ tocat - tcp:[::1]:9000
```

| Option      | Description                                    |
| ----------- | ---------------------------------------------- |
| `name=TEXT` | Label for logs and dumps. Default `tcp://addr` |

## `tcp-listen` - accept inbound TCP connections

Aliases: `tcplisten`, `listen`. The target is `host:port`, `host`, `port` or
nothing, and what is missing defaults to `127.0.0.1:8000`. Loopback rather than
the wildcard is deliberate: exposing a relay to the network should be something
you asked for.

```console
$ tocat tcp-listen:9000 -
$ tocat tcp-listen:0.0.0.0:9000,fork tcp:localhost:8080
```

| Option              | Description                                                                                             |
| ------------------- | ------------------------------------------------------------------------------------------------------- |
| `fork`              | Create a task for each client, without this option tocat serves a single connection and then terminates |
| `max-connections=N` | Concurrent connection ceiling. Alias `maxconn`. Default is 1024                                         |
| `name=TEXT`         | Label for logs and dumps. Default `tcp://host:port`                                                     |

Either endpoint may be the listening one:
`tocat tcp:backend:80 tcp-listen:9000,fork` forks on the sink and dials the
source for each accepted client.

Under `fork` everything stateful is per connection: a buffer per direction, a
fresh instance of every pipeline stage, and its own tick schedules. Only the
side channels a stage writes to (a `tee` file, a `rate` sample file) are shared.
That multiplies, so see the notes on cost in [`rate`](../plugins/rate.md) and
[Buffers](../buffers.md) before pairing a large buffer with a high ceiling.

The accepted peer's address is appended to the listening endpoint's label for
the life of that connection, which is what keeps one shared hex dump readable.
