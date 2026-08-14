# The data path

Per connection, per direction, tocat owns one buffer and one loop: read into the
buffer, hand the bytes to that path's chain, write what comes out, repeat until
end of stream. The buffer is 256 KiB by default and is the largest chunk any
stage can see.

What actually runs depends on what was declared, and there are four
arrangements, chosen so that nobody pays for machinery they did not ask for.

## The whole-relay shortcuts

**`copy_bidirectional_with_sizes`.** Nothing declared on either path, no
progress meter, and both endpoints handed back a `Duplex` stream, which in
practice means TCP or Unix sockets on both sides. The relay hands the pair to
tokio and stays out of the way. This is the case worth protecting: it is what
most invocations are, and any per-chunk work tocat adds to it is pure overhead.
Turning on `--progress` costs the shortcut, because counting needs a per-chunk
hook that call does not offer, and that is documented rather than hidden.

**The synchronous path.** No plugins at all and both endpoints blocking-backed,
which means `file:`, `pipe:` and `stdio:`. Tokio's wrappers for those read into
their own buffer and copy into yours, so a relay between two of them would pay
two userspace copies of every byte. Instead each direction gets a
`spawn_blocking` task running a plain `read`/`write` loop over one page-aligned
buffer, which is structurally what socat does. Not `std::io::copy`: its
kernel-offload specialisations only fire for concrete types, and through a `dyn`
it falls back to an 8 KiB stack buffer.

A direction with no reader or no writer does not exist and is skipped, on this
path and on the pumped ones below. It matters more than it sounds. A `file:`
source paired with stdio would otherwise park on a stdin read whose bytes go to
a null sink, holding the relay open for an EOF nobody will send; paired with
`udp:` it would wait for one that cannot arrive at all, since a datagram socket
has no end of stream. Either way the direction that mattered has already
finished. Stages declared on a direction that does not exist are warned about
rather than silently skipped, since they were built and are about to do nothing.

## The pumps

Otherwise each direction is pumped independently, and a direction with nothing
declared on it still gets the cheap treatment:

| Chain on this path | Cost per chunk                                                      |
| ------------------ | ------------------------------------------------------------------- |
| empty              | `copy_buf`, or a plain loop on a datagram path. No plugin code runs |
| one segment        | N virtual calls, no copy if every stage observes                    |
| detached segments  | above, plus one copy and one wakeup per unit crossing the boundary  |
| process segment    | above, plus a pipe crossing each way and a child                    |

Datagrams cannot take the `copy_buf` shortcut even with an empty chain, because
it is free to coalesce reads and would merge two messages into one send. An
empty pipeline is run instead, which preserves the one-in-one-out mapping.

## Segments and detach

A path is a chain of stages, and by default the whole chain runs inline on the
reading task: one call per stage per chunk, no allocation, no wakeup. A stage
declared `Detached` starts a new segment, and so does the first stage after a
subprocess, since nothing can run inside one. Subsequent inline stages join the
segment that is open rather than spawning more.

Segments are joined by bounded channels two parcels deep, with a return path so
spent buffers are recycled rather than reallocated. That is what an OS pipe
between stages would give you, minus the two syscalls and the kernel round trip.
Crossing the boundary is the one place a copy is unavoidable: the downstream
task outlives the stack frame that produced the bytes, so it needs owned ones.

Segments are spawned back to front, so every segment's downstream exists before
it starts, and the head stays on the calling task, since spawning it would buy
nothing.

A downstream segment that has finished makes its upstream's send fail, which is
treated as end of stream rather than as an error: a `limit` that reached its cap
or a sink that closed is a legitimate reason for the segment above to stop, and
one that broke reports its own error from its own task.

## Backpressure

tocat does not queue. Nothing on the data path grows without bound, and slowness
propagates backwards rather than accumulating:

- A slow sink means the write does not complete, so the loop does not read
  again, so the receive buffer fills and the peer is slowed by the transport.
- `throttle` uses the same mechanism deliberately: it asks the host to wait
  before reading again rather than holding bytes back, which throttles the
  sender rather than tocat.
- A stage that buffers (`block`) is bounded by its own configuration rather than
  by demand.
- A detached link is two parcels deep, so a slow segment stalls the one above it
  almost immediately.

Kernel pipe buffers are the one fixed limit outside tocat that can cap
throughput, which is why the pipes tocat owns are enlarged to match the copy
buffer. See [Buffers](../guide/buffers.md).

## Ticks on the data path

A segment holding a stage that asked for ticks runs its read in a `select!`
against one timer, so the stage hears from the clock even while the stream is
idle. Reads are biased to win, because payload should not wait behind
bookkeeping, and a segment with nothing ticking awaits the read directly.

That arrangement depends on the read being cancel-safe. Every arm of it bottoms
out in `poll_read`, `UdpSocket::recv` or `mpsc::Receiver::recv`, none of which
consume anything when dropped mid-poll. Anything added there has to keep that
property or the select will quietly eat bytes.

When the timer fires, every stage that is due gets its turn, and what each emits
is written before the next runs, all measured against one instant so that two
stages due on the same wakeup do not disagree about the time.

## Ending

End of stream on a path is delivered to the top stage, which may emit, and then
cascades: each stage below sees that output and then its own end of stream. Once
the chain has drained, the emission is written, staged effects are applied, and
the sink is flushed and shut down. `limit` reaching its ceiling produces exactly
this sequence early, which is why it is a successful exit rather than an error.

The run ends when every direction that exists has ended, which is not the same
as when the interesting one has. Two duplex endpoints usually resolve this by
themselves, since closing the write half is what tells the peer to hang up, and
its close is the other direction's end of stream. A datagram sink sends no such
signal, so a genuinely bidirectional datagram relay ends when both paths do:
that is what a `timeout` stage on both directions is for.
