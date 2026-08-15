# `unix` and `unix-listen`

```console
$ tocat - unix:/run/app/app.sock
$ tocat unix-listen:/tmp/tocat.sock,fork,unlink,mode=660 tcp:localhost:8080
```

| Option                      | Description                                                                                          |
| --------------------------- | ---------------------------------------------------------------------------------------------------- |
| `fork`, `max-connections=N` | As [`tcp-listen`](tcp.md). `unix-listen` only                                                        |
| `unlink`                    | Remove a stale socket before binding. `unix-listen` only                                             |
| `mode=NNN`                  | Octal permissions applied after binding, explicitly, so umask does not mask them. `unix-listen` only |
| `name=TEXT`                 | Label for logs and dumps. Default `unix://path`                                                      |

`unlink` is about the stale path rather than the fresh one. Binding fails on an
existing path whether or not anything is listening on it, so with `unlink` set
tocat probes the path first: a refused connection means the owner is gone and
the path can be removed, while a successful one means a live server and is an
error rather than something to unlink out from under.

A bound socket is removed again when the relay finishes.

In a config file the scheme may also be written `unix-connect`. On the command
line only `unix:` is accepted.

These two carry a byte stream. For local sockets that carry messages instead,
see [`unix-seqpacket`](unix-seqpacket.md), which is connected and preserves
boundaries, and [`unix-dgram`](unix-dgram.md), which is connectionless.

## Addresses

Every unix scheme takes its address the same way, on the command line and in a
config file alike.

A plain address is a path. An address beginning with `@` names the Linux
abstract namespace, which lives outside the filesystem:

```console
$ tocat unix-listen:@tocat,fork tcp:localhost:8080
$ tocat - unix:@tocat
```

```toml
source = { type = "unix-listen", path = "@tocat", fork = true }
```

An abstract address has no directory entry, which changes three things.

- **Nothing is created and nothing is cleaned up.** The kernel releases the name
  when the last socket holding it closes, so there is no stale address for
  `unlink` to clear and nothing left behind if tocat is killed.
- **`mode` is rejected rather than ignored.** There is no file to change the
  permissions of, and no permission check at all: anything in the network
  namespace can connect. tocat fails the run instead of letting `mode=600` look
  like it did something.
- **Reaching one needs the same namespace.** Containers, and anything else with
  a network namespace of its own, cannot see each other's abstract addresses
  even when they share a filesystem. The reverse is also true, which is the
  usual reason to prefer one.

To use a file that really is called `@name`, write it as `./@name` or give the
absolute path.
