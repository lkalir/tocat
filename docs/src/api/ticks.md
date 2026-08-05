# Ticks and timers

A stage that needs time rather than traffic to drive it implements `tick_interval` and `on_tick`. The host asks once, at the end of construction, how
often the stage wants calling, and then calls it on that schedule for as long as the pipeline lives, whether or not bytes are moving. Without it a
stalled stream and a finished one are indistinguishable from inside a plugin.

`ctx.input()` is empty during a tick, so there is nothing to pass through: anything emitted is forwarded, and it continues downstream through the
stages *below* this one, the same way `on_eof` output cascades. The stages above are upstream of a chunk that did not come from them. Emitting nothing
is the common case and costs nothing. Whatever is emitted is one unit unless `ctx.boundary()` says otherwise, exactly as in `on_bytes`.

`tick_interval` is read exactly once, so it must not depend on anything that changes later. A stage whose interval is configurable reads its config in
`build` and answers from that. A zero period reads as "no ticks", which is the same answer as `None` and is what a stage configured with an interval of
zero means by it.

## Cadence, not delay

The host owns the clock, because a guest cannot reach one. What a stage gets is therefore a cadence rather than a delay: a tick that came due while
bytes were flowing fires at the next opportunity, which can be immediately after the bytes it is about arrived.

A stage that means "an interval after I started waiting" says so with `ctx.rearm()`, which restarts its own schedule from now. It is cheap and harmless
to call often: it sets a flag the host reads once at the end of the call, and it is ignored for a stage that asked for no ticks. A tick may rearm too,
which is how a stage that has just given up waiting says so.

That is how `block` turns `flush` into a latency bound it can actually promise: it rearms when a block starts filling, so the interval is measured from
the first byte held rather than from wherever a shared cadence happened to be.

| Stage      | Wants                                                        | Uses     |
|------------|--------------------------------------------------------------|----------|
| `rate`     | Samples on a fixed cadence, so a stalled stream is reported  | the tick |
| `block`    | A bound on how long a byte waits, measured from that byte    | `rearm`  |
| `timeout`  | A deadline measured from the last byte, to a known accuracy  | both     |

## What it costs

The pipeline owns the schedules, one deadline per ticking stage, and the host owns a single timer that asks whether any of them are due. The timer runs
at the shortest period any stage in that segment asked for; a stage that wanted a longer one is simply not due on most wakeups, which is cheaper than a
timer each. A segment with nothing ticking builds no timer at all and awaits its read directly.

So the cost is one timer per segment per direction per connection, which multiplies under `fork`: a stage asking for milliseconds is asking every
forked connection to wake up that often. A stage should return `None` when its options do not require ticking, which is what `rate,interval=0` does.

A stage that needs a deadline rather than a cadence needs both halves: `rearm` so the count starts from the last byte, and a period some fraction of the
deadline so that the host's cadence rounds the answer by that fraction rather than by the whole thing. `timeout` asks for a quarter of its window and
halts on the fourth consecutive idle tick, which is what bounds its error at a quarter rather than at one whole window.


Two details of the host's timing are worth relying on. Reads win the race against the timer, so payload never waits behind bookkeeping. And a segment
that fell behind does not then fire a burst of catch-up ticks: a missed schedule resumes from now, and a stage that wants to know how long it was away
measures that itself.
