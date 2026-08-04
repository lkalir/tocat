# The datagram model

A byte endpoint carries a stream; a datagram endpoint carries messages. tocat keeps that distinction rather than flattening everything to bytes,
because the boundaries in a message stream are data: a peer that sent two datagrams did not send their concatenation.

The distinction is in the types. An open endpoint is a duplex stream, a split pair of halves, or a datagram socket, and the datagram variant is
deliberately not `AsyncRead`/`AsyncWrite`: those traits describe a byte stream and have nowhere to put a boundary. Everything downstream of an endpoint
therefore has to handle both shapes explicitly, which is what stops a datagram path quietly acquiring stream semantics in a refactor.

## The rule

On a byte stream a chunk is an arbitrary slice, and both the host and a stage may buffer, split or coalesce freely. On a datagram path one receive is
one message, one call to a stage is one message, and each unit a stage emits is sent as exactly one message. A stage that holds bytes across calls, or
emits two messages' worth from one, silently corrupts the protocol.

[Units and boundaries](../api/units.md) are the mechanism that makes the good case expressible: what a stage emits between boundaries becomes one
datagram.

## Warn rather than refuse

Every stage declares whether it may carry datagrams, defaulting to false. When a stage that may not sits on a path whose *destination* is a datagram
endpoint, the host names it, warns, and relays anyway.

Refusing would be wrong, because rewriting the message stream is sometimes exactly the point. `block,size=1400` on a UDP path emits one datagram per
block, which is a reasonable thing to ask for and still not a preservation of what the peer sent. The warning says what happened; the operator decides
whether it is what they meant, and may well know the peer tolerates it.

The test is against the destination, not the source. Sending datagrams *into* a stream sink is unremarkable and draws no warning, since the sink has no
boundaries to corrupt.

```
udp-listen:9000 compress:forward tcp:collector:9000   # fine
udp-listen:9000 compress         udp:peer:9000        # warns
```

A subprocess stage always warns: its stdin and stdout are byte streams, so boundaries are gone the moment bytes cross the pipe, whatever the child does.

## Consequences elsewhere

Four properties of datagram paths ripple through the rest of the system, and are worth remembering when adding a feature.

**There is no end of stream.** A datagram source runs until interrupted, so anything that only happens at end of stream never happens: a `rate`
summary, a `compress` epilogue and ratio report, the final short `block`. A feature that reports only at the end is a feature that does not exist on
such a path, which is why `rate` also reports on a cadence. Held FIFOs share this property, so both cases need checking anywhere the code assumes end
of stream will arrive.

**Splitting is the unsafe operation, not truncating.** This is why `limit` has three behaviours for the crossing chunk: `drop` and `overshoot` preserve
the message and are safe, while `exact` splits it and is not. Half a datagram is a corrupt message rather than a short read.

**The buffer is the message ceiling.** One receive fills one buffer, and a longer message is truncated by the kernel. The copy buffer is therefore a
protocol-visible setting on a datagram path in a way it is not on a stream.

**An empty emission is not sent.** The pipeline is drained once more at end of stream, and on a datagram sink that would put a spurious zero-length
message on the wire, which many peers read as a close. The cost is that a genuine zero-length datagram is dropped rather than forwarded, which is the
rarer case and the lesser harm.

## What is not there yet

`udp-listen` peers with the first sender and ignores everyone else: there is no per-sender demultiplexing, and `fork` does not apply to it. Adding it
means a peer table and per-peer chain instances, since stage state is per path and per connection and two senders are two connections.
