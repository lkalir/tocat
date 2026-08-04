# `unix` and `unix-listen`

```console
$ tocat - unix:/run/app/app.sock
$ tocat unix-listen:/tmp/tocat.sock,fork,unlink,mode=660 tcp:localhost:8080
```

| Option                              | Description                                                                                                   |
|-------------------------------------|-----------------------------------------------------------------------------------------------------------------|
| `fork`, `max-connections=N`         | As [`tcp-listen`](tcp.md). `unix-listen` only                                                                 |
| `unlink`                            | Remove a stale socket before binding. `unix-listen` only                                                      |
| `mode=NNN`                          | Octal permissions applied after binding, explicitly, so umask does not mask them. `unix-listen` only          |
| `name=TEXT`                         | Label for logs and dumps. Default `unix://path`                                                               |

`unlink` is about the stale path rather than the fresh one. Binding fails on an existing path whether or not anything is listening on it, so with
`unlink` set tocat probes the path first: a refused connection means the owner is gone and the path can be removed, while a successful one means a live
server and is an error rather than something to unlink out from under.

A bound socket is removed again when the relay finishes.

In a config file the scheme may also be written `unix-connect`. On the command line only `unix:` is accepted.
