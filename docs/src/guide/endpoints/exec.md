# `exec` and `system`

Both run a child process with its stdin and stdout wired to the relay. They
differ only in how the command line is interpreted.

## `exec` - subprocesses

Runs a program with its stdin and stdout wired to the relay. The target is split
on whitespace and passed directly to the program. This is not a shell, so no
globbing, quoting, or metacharacters. The child's stderr is inherited, so its
diagnostics go to your terminal rather than into the relayed data.

```console
$ tocat tcp-listen:9000,fork "exec:/usr/bin/env cat"
```

## `system` - shell commands

Runs the given string through `$SHELL -c` (or `sh -c` when `SHELL` is unset), so
pipes, redirection, globbing, and variable expansion all work. Anything the
string contains runs with tocat's privileges. Don't use `system` with a command
built from untrusted input, or in a config file others can write.

```console
$ tocat tcp-listen:9000,fork "system:grep -v DEBUG | sort -u"
```

## Both

| Option      | Description                                                                                       |
| ----------- | ------------------------------------------------------------------------------------------------- |
| `name=TEXT` | Accepted, but the label stays `EXEC(argv)` or `SYSTEM(command)`: the command line is the identity |

The child is killed when the relay drops the connection, and is reaped in the
background: the relay ends when the pipes close rather than when the process
does, and a non-zero exit is logged as a warning. That is the opposite of the
[`process`](../plugins/process.md) plugin, where a bad exit fails the direction,
because there the child's output is mid-pipeline and being wrong matters.

The pipes to and from the child are enlarged to match the copy buffer where the
platform allows it, see [Buffers](../buffers.md).

To put a filter *between* the endpoints rather than at one end, use
[`process`](../plugins/process.md), which has the same two ways of naming a
child and adds control over its stderr.
