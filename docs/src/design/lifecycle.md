# Lifecycle and shutdown

## Validation before I/O

`Relay::new` builds both chains once, before any endpoint is opened, purely to
discover which side channels the declarations ask for. The instances from that
pass are dropped: they are stateful and each connection needs its own. What
survives is the frozen channel plan, which is then opened, and which every
connection resolves its handles against without touching a lock.

So a misspelled plugin, an option no plugin declares, a `detach = false` on a
subprocess stage or an unwritable dump file fails at startup rather than on the
first byte of the first connection. The three warnings described in
[Architecture](architecture.md) are raised in the same pass.

## Forking

If either endpoint is a forking listener, that side binds and accepts in a loop
while the other is dialled per connection. Concurrency is bounded by a semaphore
of `max-connections` permits, acquired before the accept rather than after, so
the listener stops taking connections it cannot serve instead of accepting and
queueing them.

Each accepted connection gets its own chain pair, its own buffers and its own
tick schedules, and runs on its own task inside a tracing span carrying the
peer. Only the channel handles are shared, which is how several connections can
dump into one file. Accept errors that are per-connection (aborted, interrupted,
would-block) are logged and the loop continues; anything else ends the run.

`udp-listen` has no accept to run, so a forking one is served by a receive loop
that demultiplexes the socket by source address and hands each new sender to the
same accept loop as a session. Everything downstream of that is identical; what
differs is that a session has no close to end it, so a `timeout` stage on both
paths is what reclaims one, and that the connection ceiling is enforced in the
receive loop, since a session exists by the time the accept loop sees it. See [The datagram model](datagrams.md).

## Signals

The first SIGINT or SIGTERM flips a watch channel: the listener stops accepting,
every connection still in flight is handed the same watch, and a task tracker
waits for them to finish. A second signal exits immediately with status 130.

A connection observes the signal *as end of stream*, not as cancellation. The
handle goes down to the reads that touch an endpoint, which stop returning
chunks, and the ordinary end-of-stream path takes it from there: `on_eof` runs,
a stage holding buffered bytes gets to emit them, side channels are applied and
the writer is flushed and shut down. Racing the copy against the watch in a
`select!` would skip all of that, which is why only the two paths with no stages
do it: the plugin-free `copy_bidirectional` shortcut, and the synchronous copy.

Within a pipeline, only the head watches. A read from a segment boundary does
not: the head stopping closes its outlet, which is end of stream for the segment
below it, so `on_eof` cascades in order and parcels still in the channel are
drained rather than dropped. A `process` stage stops feeding and closes the
child's stdin, which is the only end of stream a subprocess has, and then keeps
draining its stdout so a filter holding a block still writes it out.

The synchronous copy path cannot do either, since a `spawn_blocking` task is not
cancellable, so it polls between chunks instead. That leaves one case it cannot
cover: a read that never returns, such as a FIFO with no writer. The runtime is
therefore shut down with a short grace period rather than being dropped, so
those tasks are left behind instead of making the process unkillable by signal.

## Cleanup

Two kinds of state outlive the copy loop and have to be released in the right
order.

**Paths.** A bound Unix socket, and a `pipe:` opened with `unlink`, hand back a
guard that removes the path when it drops. The guard has to outlive the
connection using it, or the socket is unlinked while still in use, or the FIFO
is removed out from under a producer still writing to it.

**Channels.** Side-channel writers are buffered, so they are flushed after the
relay finishes, whether it finished cleanly or not: on an error path that flush
may be the last chance to get the dump to disk. All channels are flushed
concurrently, so one failing sink does not cancel the others, and the first
failure is reported.

The progress display is taken down before any error is reported, so its summary
line lands above the diagnostics rather than wedged between them.

Children are handled differently at the two places they appear. An `exec:` or
`system:` endpoint is reaped in the background, so the relay ends when the pipes
close rather than when the process does, and a non-zero exit is a warning. A
`process` stage is waited on, and a non-zero exit fails that direction, because
there the child's output is mid-pipeline and being wrong matters. Both are
spawned with kill-on-drop, which is what ends a child blocked writing to a
stdout nobody is draining.

## Exit status

| Status | Means                                                               |
| ------ | ------------------------------------------------------------------- |
| 0      | The relay finished, or was interrupted and drained                  |
| 1      | Setup failed, or the relay failed mid-transfer. The error is logged |
| 130    | A second signal arrived while draining                              |

A stage stopping the transfer deliberately, which is what `limit` does, is a
success: it is upstream end of stream arriving early, not a fault.
