# `pty` and `pty-exec`

Both allocate a pseudo-terminal and relay its master side. They differ in what
ends up on the other end: `pty-exec` spawns a child there, `pty` publishes it as
a device path and waits for something else to open it.

## `pty-exec` - a command on a terminal

Like [`exec`](exec.md), but the child gets a controlling terminal instead of a
pair of pipes.

```console
$ tocat tcp-listen:9000,fork pty-exec:bash
```

Three things change when a program runs on a terminal, and one of them is
usually why you are here:

- libc line-buffers stdout instead of block-buffering it, so output arrives a
  line at a time rather than in 4KiB blocks. If you have piped a program through
  `exec` and watched nothing come out until it exited, this is the fix.
- The kernel supplies a line discipline, so line editing, `^C` and job control
  work.
- Programs that check for a terminal and refuse, or fall back to a degraded
  mode, run properly.

The pty is the child's stdin, stdout **and** stderr. There is no second
descriptor to inherit, so unlike `exec` the child's diagnostics are relayed
along with everything else. On a terminal that is what a terminal shows.

With `shell`, the target is handed to `$SHELL -c` whole rather than split on
whitespace, which is `pty-exec` playing the part [`system`](exec.md) plays for
`exec`:

```console
$ tocat tcp-listen:9000 "pty-exec:tail -f /var/log/syslog | grep -i oom,shell"
```

| Option      | Description                                                                      |
| ----------- | -------------------------------------------------------------------------------- |
| `shell`     | Run the target through `$SHELL -c` rather than splitting it on whitespace        |
| `term=TEXT` | What to set `TERM` to for the child. Unset leaves the relay's own value          |
| `name=TEXT` | Accepted, but the label stays `PTY-EXEC(argv)`: the command line is the identity |

## `pty` - a device for something else to open

Allocates a pty, relays the master, and spawns nothing. The target is a symlink
to create pointing at the slave device, which is how another program finds it:

```console
$ tocat pty:/tmp/ttyfake tcp:10.0.0.5:4001
```

Anything that wants a serial port can now be pointed at `/tmp/ttyfake`, and its
traffic goes over TCP. The symlink is removed when the relay exits, and a
*dangling* one left by a previous run is replaced. A link whose target still
exists is not, since that would steal a path something else is using.

The target is optional. Without it the allocated device is logged and nothing is
linked, which is fine at a prompt and useless from a script.

| Option      | Description                                                     |
| ----------- | --------------------------------------------------------------- |
| `link=PATH` | The same symlink the target sets, for the config file's benefit |
| `name=TEXT` | Replaces the label, which is otherwise `pty://link`             |

## Terminal settings, which both take

| Option           | Default | Description                             |
| ---------------- | ------- | --------------------------------------- |
| `raw`            | on      | Pass bytes through untouched            |
| `echo`           | off     | Echo input back at the writer           |
| `size=ROWSxCOLS` | unset   | The window size reported to the program |

**`raw` is on by default and you should usually leave it.** A pty in cooked mode
is not a transparent pipe: it translates carriage returns, turns `^C` into a
signal rather than a byte, and refuses a line longer than 4096 bytes. Relaying
anything but text through that is corruption. Turn it off with `raw=false` when
you want the line discipline, which mostly means when a human is typing at the
other end and nothing else is providing line editing.

**`echo` is off** because a relay that echoes writes back at its own reader is a
loop rather than a feature. Turn it on when the far end is a person who expects
to see what they type and no local terminal is doing it for them.

**`size` is unset** because there is no correct value to guess. A fresh pty
reports 0x0, which most full-screen programs read as "no idea" and a few read as
a terminal one column wide. Set it if you are running something full-screen:

```console
$ tocat tcp-listen:9000,fork "pty-exec:htop,size=40x120,term=xterm-256color"
```

The size is fixed for the life of the connection. Nothing relays a terminal
resize, because there is no channel in a byte stream to carry one.

## Which to reach for

To open a terminal that already exists rather than allocating one, use
[`tty`](tty.md).

Use [`exec`](exec.md) when you want a filter: it is cheaper, and a pty's line
discipline is a liability when nothing needs it. Use `pty-exec` when the program
behaves differently on a terminal, which is most interactive ones. Use `pty`
when the thing you are connecting expects to open a device rather than be
spawned.

To put a program *between* the endpoints rather than at one end, use the
[`process`](../plugins/process.md) plugin. It has no pty option: a stage sits
mid-pipeline where a line discipline rewriting the payload would be a bug.
