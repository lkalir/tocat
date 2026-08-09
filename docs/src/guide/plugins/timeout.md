# `timeout` - end an idle path

Ends the transfer when nothing has crossed this point for a while.

```console
$ tocat tcp-listen:9000,fork 'timeout,timeout=30s' tcp:backend:8080
$ tocat -f tcp-listen:9000,fork -t tcp:backend:8080 -p 'timeout:forward,wait=2m'
```

| Option         | Description                                                                                                                    |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `timeout=TIME` | How long the path may carry nothing before it is ended. Required, and greater than zero. Aliases: `wait`, `inactivity`, `idle` |

Reaching the timeout is upstream end of stream arriving early, not an error:
bytes already emitted are written, the stages below get their end of stream,
sinks are flushed and closed, and tocat exits successfully. The stop is logged
at info by the host:

```
timeout: no data for 30s
```

Like [`tee`](tee.md) and [`rate`](rate.md) it never touches the payload, so it
can go anywhere in a chain, including on a datagram path, and its position
decides only what counts as activity: before a [`compress`](compress.md) stage
it watches the payload, after it watches the wire.

## One clock per path

The window is per path and per connection, and by default that is the forward
path. `direction=both` gives each path its own clock, which is usually not what
an idle-connection reaper wants: a request/response protocol whose server thinks
before answering has a quiet reverse path by construction, and ending it closes
the write half back to the client. Pick the one path whose silence actually
means the connection is dead.

The clock starts when the path opens rather than at the first byte, so a
connection that is accepted and then says nothing is ended after one window.
That is the point of the stage, but it does mean the timeout has to be longer
than the peer's think time, not just longer than its gaps.

Ending one path does not by itself end the run: the other direction is still
being pumped. In practice the closed write half is what tells the peer to go
away, and the other path then reaches its own end of stream, but a peer that
ignores that keeps the connection open until it too times out. On a path that
never reaches end of stream anyway (a datagram source, a held
[`pipe`](../endpoints/pipe.md)) this stage is the only thing that will ever end
it, which is one good reason to reach for it there.

## Accuracy and cost

The halt lands within a quarter of the timeout of where it was asked for, and
never early. The stage asks the host for a tick four times per window and counts
consecutive idle ticks, rather than asking for one tick per window: the host's
timer runs at a fixed cadence, so a single-tick implementation would fire
anywhere between one and two windows after the last byte. Below 400ms it stops
subdividing, so a very short timeout is coarser (up to twice what was asked)
rather than waking every forked connection every few milliseconds.

That means four wakeups per window per direction per connection, which
multiplies under `fork`, plus one clock read per chunk to restart the window.
Both are cheap, but a one-second timeout across a thousand connections is four
thousand wakeups a second, and the answer there is a longer timeout rather than
a tighter one.
