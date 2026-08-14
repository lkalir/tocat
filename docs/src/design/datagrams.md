# The datagram model

A byte endpoint carries a stream; a datagram endpoint carries messages. tocat
keeps that distinction rather than flattening everything to bytes, because the
boundaries in a message stream are data: a peer that sent two datagrams did not
send their concatenation.

The distinction is in the types. An open endpoint is a duplex stream, a split
pair of halves, or a datagram socket, and the datagram variant is deliberately
not `AsyncRead`/`AsyncWrite`: those traits describe a byte stream and have
nowhere to put a boundary. Everything downstream of an endpoint therefore has to
handle both shapes explicitly, which is what stops a datagram path quietly
acquiring stream semantics in a refactor.

## The rule

On a byte stream a chunk is an arbitrary slice, and both the host and a stage
may buffer, split or coalesce freely. On a datagram path one receive is one
message, one call to a stage is one message, and each unit a stage emits is sent
as exactly one message. A stage that holds bytes across calls, or emits two
messages' worth from one, silently corrupts the protocol.

[Units and boundaries](../api/units.md) are the mechanism that makes the good
case expressible: what a stage emits between boundaries becomes one datagram.

## Warn rather than refuse

Every stage declares whether it may carry datagrams, defaulting to false. When a
stage that may not sits on a path whose *destination* is a datagram endpoint,
the host names it, warns, and relays anyway.

Refusing would be wrong, because rewriting the message stream is sometimes
exactly the point. `block,size=1400` on a UDP path emits one datagram per block,
which is a reasonable thing to ask for and still not a preservation of what the
peer sent. The warning says what happened; the operator decides whether it is
what they meant, and may well know the peer tolerates it.

The test is against the destination, not the source. Sending datagrams *into* a
stream sink is unremarkable and draws no warning, since the sink has no
boundaries to corrupt.

```
udp-listen:9000 compress:forward tcp:collector:9000   # fine
udp-listen:9000 compress         udp:peer:9000        # warns
```

A subprocess stage always warns: its stdin and stdout are byte streams, so
boundaries are gone the moment bytes cross the pipe, whatever the child does.

## Consequences elsewhere

Four properties of datagram paths ripple through the rest of the system, and are
worth remembering when adding a feature.

**There is no end of stream, unless a stage makes one.** A datagram source runs
until interrupted, so anything that only happens at end of stream never happens:
a `rate` summary, a `compress` epilogue and ratio report, the final short
`block`. A feature that reports only at the end is a feature that does not exist
on such a path, which is why `rate` also reports on a cadence. Held FIFOs share
this property, so both cases need checking anywhere the code assumes end of
stream will arrive.

A `timeout` stage halting the path is the one thing that produces one, and its
halt is a real end of stream: `on_eof` runs and those reports do arrive. That
also makes it the only way a forked session can end, which is why the endpoint
grew no timer of its own.

**Splitting is the unsafe operation, not truncating.** This is why `limit` has
three behaviours for the crossing chunk: `drop` and `overshoot` preserve the
message and are safe, while `exact` splits it and is not. Half a datagram is a
corrupt message rather than a short read.

**The buffer is the message ceiling.** One receive fills one buffer, and a
longer message is truncated by the kernel. The copy buffer is therefore a
protocol-visible setting on a datagram path in a way it is not on a stream.

**An empty emission is not sent.** The pipeline is drained once more at end of
stream, and on a datagram sink that would put a spurious zero-length message on
the wire, which many peers read as a close. The cost is that a genuine
zero-length datagram is dropped rather than forwarded, which is the rarer case
and the lesser harm.

## Demultiplexing by sender

Without `fork`, `udp-listen` peers with the first sender and ignores everyone
else. With `fork` it keeps the socket unconnected and routes by source address:
one receive loop owns the socket, a map takes each datagram to that sender's
session, and a sender not in the map becomes a new session with its own dialled
peer and its own chain instances. Two senders are two connections, because stage
state is per path and per connection.

The alternative, and what socat does, is a fresh `SO_REUSEADDR` socket per peer
connected to that address, leaving the kernel to route by most specific match.
That is one fd per peer, it leans on delivery rules that differ across
platforms, `SO_REUSEPORT` on the same address load balances instead of
specialising, and datagrams arriving between the receive and the connect land on
the wrong socket anyway. One socket and a map has none of those problems and
costs a task hop and a copy per datagram, which a pipeline was going to pay at
its first detached boundary regardless.

**Nothing ends a session but a stage.** UDP has no close, so a session lasts as
long as its task, and the task lasts until both directions finish. The endpoint
deliberately has no idle option: `timeout` already ends a path that has gone
quiet, already declares itself datagram safe, and already halts in the way that
cascades `on_eof`, so a second timer here would have been the same feature under
a different name. It has to be declared on both paths (`timeout:both`), since a
forward-only halt leaves the reverse pump reading a sink that may never close,
and it is the task ending that releases the permit and the map entry.

Two more properties follow from the receive loop serving every sender at once.

**It never blocks on one session.** A session's queue is bounded, and a datagram
for a queue that is full is dropped rather than made to wait, since waiting
would stall every other peer. Drops are logged loudly once and at debug after,
so a flood does not bury the rest of the log.

**The ceiling is enforced there, not at the accept.** `max-connections` is
checked in the receive loop, because by the time a session reaches the accept
loop it already exists. Finished sessions are reaped when the ceiling is
reached, which is the only moment a dead entry is in anybody's way.
