//! pump.rs - moving bytes along one direction.
//!
//! Three paths, chosen per direction so you only pay for what you declared:
//!
//! | chain              | cost per chunk                                   |
//! | ------------------ | ------------------------------------------------ |
//! | empty              | `copy_buf`; no plugin code runs at all           |
//! | one segment        | N virtual calls, no copy if every stage observes |
//! | detached segments  | above, plus one copy and one wakeup per boundary |
//! | process segment    | above, plus a pipe crossing each way and a child |
//!
//! The detached path is what an OS pipe between stages would give you, minus
//! the two syscalls and the kernel round-trip: a bounded in-memory channel with
//! buffer recycling. It exists for stages that are expensive per byte and want
//! to run concurrently with the reader.
//!
//! # Ticks
//!
//! A segment holding a stage that asked for ticks runs its read in a `select!`
//! against a timer, so the stage hears from the clock even while the stream is
//! idle. That relies on `Source::next` being cancel-safe, which it is: every
//! arm of it bottoms out in `poll_read`, `UdpSocket::recv` or
//! `mpsc::Receiver::recv`, and none of those consume anything when dropped
//! mid-poll. Anything added there has to keep that property or the select will
//! quietly eat bytes.
//!
//! Reads win the race (due to `biased`) because payload should not wait behind
//! bookkeeping. A pipeline with nothing ticking builds no timer and awaits the
//! read directly, exactly as before.
//!
//! # Framing
//!
//! A segment emits one unit per chunk unless a stage in it asked otherwise
//! (see [`tocat_api::Ctx::boundary`]). Where units are sent one at a time they
//! cost something, so each destination pays only where the split is
//! observable: a byte stream takes the whole emission in one write, because a
//! peer cannot tell the difference and one syscall is cheaper than several,
//! while a datagram sink sends one message per unit and a detached boundary
//! sends one parcel per unit. That last one is the expensive case: a stage
//! cutting a chunk into many small units turns one task hop into one per unit,
//! which is a reason not to detach a stage sitting under one.
//!
//! # Shutdown
//!
//! A signal is upstream end of stream arriving early, not cancellation. Only
//! the reads that touch an endpoint watch for it: they stop returning chunks,
//! and everything after that is the ordinary end-of-stream path, so `on_eof`
//! runs, whatever a stage was holding is emitted, side channels are applied
//! and the writer is flushed and shut down. Dropping these futures instead
//! would skip all of that and lose a `hash` digest, a compressor's epilogue,
//! or a `block` stage's tail.
//!
//! Reads from a segment boundary deliberately do *not* watch for it. The head
//! stopping closes its outlet, which is end of stream for the segment below,
//! which drains what is still in the channel before running its own `on_eof`.
//! Breaking that cascade at every boundary at once would throw away parcels
//! still in flight, and the bytes the head just emitted from `finish` first.

use std::{sync::Arc, time::Instant};

use anyhow::{Context, bail};
use tocat_api::{Chain, Emitted, ExternalStage, Pipeline, Segment};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
    time::{MissedTickBehavior, sleep},
};
use tracing::{info, warn};

use crate::{
    buffer::Buffer,
    child,
    endpoint::{BoxRead, BoxWrite, DatagramSocket, ReadHalf, WriteHalf},
    host::{Channels, Effects},
    progress::Counter,
    shutdown::Shutdown,
};

/// Chunks in flight per detached boundary. Deep enough to keep both sides busy,
/// shallow enough that backpressure still reaches the reader promptly.
const LINK_DEPTH: usize = 2;

/// Run one direction to completion, returning the bytes read from upstream.
///
/// `counter` is the progress display's, and is `Some` only for a datagram
/// upstream: a stream is counted by the wrapper around its read half, which is
/// why nothing here double counts. See [`crate::progress::count`].
pub async fn pump(
    reader: ReadHalf,
    writer: WriteHalf,
    chain: Chain,
    channels: Arc<Channels>,
    buffer: usize,
    counter: Option<Counter>,
    shutdown: Shutdown,
) -> anyhow::Result<u64> {
    let meta = chain.meta().clone();
    let mut segments = chain.into_segments();

    match segments.len() {
        // Datagrams cannot take the byte-stream shortcut: a coalescing copy would merge two
        // messages into one send. An empty pipeline preserves the one-in-one-out mapping instead
        0 if matches!(reader, ReadHalf::Stream(_)) && matches!(writer, WriteHalf::Stream(_)) => {
            copy_direct(reader, writer, buffer, shutdown).await
        }
        0 => {
            run_pipeline(
                Source::new(Upstream::Stream(reader), buffer, counter, shutdown),
                Downstream::Stream(writer).into(),
                Pipeline::new(meta, Vec::new()),
                channels,
            )
            .await
        }
        1 => {
            run_segment(
                segments.pop().expect("one segment"),
                Upstream::Stream(reader),
                Downstream::Stream(writer),
                channels,
                buffer,
                counter,
                shutdown,
            )
            .await
        }
        _ => {
            run_segmented(
                reader, writer, segments, channels, buffer, counter, shutdown,
            )
            .await
        }
    }
}

/// Where a segment reads from: the endpoint, or the segment before it.
///
/// Concrete rather than generic. Only the head and tail ever hold a stream, so
/// a type parameter would be a phantom on every other segment, and one the
/// compiler could not infer at the call site.
enum Upstream {
    Stream(ReadHalf),
    Link(Inlet),
}

/// Where a segment writes to: the endpoint, or the segment after it.
enum Downstream {
    Stream(WriteHalf),
    Link(Outlet),
}

/// Run one segment, whichever ends it happens to have.
///
/// Returns the bytes it read, which is only meaningful for the head.
async fn run_segment(
    segment: Segment,
    input: Upstream,
    output: Downstream,
    channels: Arc<Channels>,
    buffer: usize,
    counter: Option<Counter>,
    shutdown: Shutdown,
) -> anyhow::Result<u64> {
    match segment {
        Segment::Inline(pipeline) => {
            run_pipeline(
                Source::new(input, buffer, counter, shutdown),
                output.into(),
                pipeline,
                channels,
            )
            .await
        }
        Segment::Process(external) => {
            run_process(external, input, output, buffer, counter, shutdown).await
        }
    }
}

/// No stages: runs straight through.
///
/// Hand rolled rather than `copy_buf` so a signal is observed between chunks.
/// Cancelling a copy future mid-flight would drop bytes it had read and not
/// yet written, and there is no kernel offload to give up: this is the same
/// read-then-write loop, minus one layer of buffering.
async fn copy_direct(
    reader: ReadHalf,
    writer: WriteHalf,
    buffer: usize,
    mut shutdown: Shutdown,
) -> anyhow::Result<u64> {
    let (ReadHalf::Stream(mut reader), WriteHalf::Stream(mut writer)) = (reader, writer) else {
        unreachable!("checked by the caller");
    };

    let mut buf = Buffer::new(buffer);
    let mut total = 0u64;

    loop {
        let Some(n) = read_or_stop(&mut reader, &mut buf, &mut shutdown).await? else {
            break;
        };

        if n == 0 {
            break;
        }

        writer.write_all(&buf[..n]).await?;
        total += n as u64;
    }

    writer.flush().await?;
    let _ = writer.shutdown().await;

    Ok(total)
}

/// One read, unless a signal arrived first.
///
/// `None` means stop: either the signal was already pending, or it landed
/// while the read was parked. The read arm is biased ahead of the signal so
/// bytes already waiting are never dropped in favour of the shutdown, and the
/// cheap check up front is what keeps a saturated stream, whose read arm wins
/// every race, from never noticing the signal at all.
///
/// Cancel safe: the only future dropped here is a `poll_read`, which consumes
/// nothing when it does not complete.
async fn read_or_stop(
    reader: &mut BoxRead,
    buf: &mut Buffer,
    shutdown: &mut Shutdown,
) -> anyhow::Result<Option<usize>> {
    if shutdown.is_triggered() {
        return Ok(None);
    }

    tokio::select! {
        biased;
        n = reader.read(&mut buf[..]) => Ok(Some(n?)),
        () = shutdown.recv() => Ok(None),
    }
}

/// Two or more segments, joined by bounded channels.
async fn run_segmented(
    reader: ReadHalf,
    writer: WriteHalf,
    mut segments: Vec<Segment>,
    channels: Arc<Channels>,
    buffer: usize,
    counter: Option<Counter>,
    shutdown: Shutdown,
) -> anyhow::Result<u64> {
    let n = segments.len();

    let mut inlets: Vec<Option<Inlet>> = (0..n).map(|_| None).collect();
    let mut outlets: Vec<Option<Outlet>> = (0..n).map(|_| None).collect();

    for i in 0..n - 1 {
        let (outlet, inlet) = link();
        outlets[i] = Some(outlet);
        inlets[i + 1] = Some(inlet);
    }

    let mut writer = Some(writer);
    let mut spawned = Vec::with_capacity(n - 1);

    // Back to front, so every segment's downstream exists before it starts.
    for i in (1..n).rev() {
        let segment = segments.pop().expect("index is in range");
        let inlet = inlets[i]
            .take()
            .expect("every non-head segment has an inlet");

        let output = if i == n - 1 {
            Downstream::Stream(writer.take().expect("the tail owns the writer"))
        } else {
            Downstream::Link(outlets[i].take().expect("a middle segment has an outlet"))
        };

        let channels = channels.clone();
        let shutdown = shutdown.clone();

        spawned.push(tokio::spawn(async move {
            // No counter: only the head reads from the endpoint. The handle is
            // inert here for the same reason: a segment reading from a link
            // stops when the link closes, never on the signal itself.
            run_segment(
                segment,
                Upstream::Link(inlet),
                output,
                channels,
                buffer,
                None,
                shutdown,
            )
            .await
            .map(|_| ())
        }));
    }

    // The head stays on this task: spawning it would buy nothing.
    let head = segments.pop().expect("at least one segment");
    let output = match outlets[0].take() {
        Some(outlet) => Downstream::Link(outlet),
        None => Downstream::Stream(writer.take().expect("a lone segment owns the writer")),
    };

    let total = run_segment(
        head,
        Upstream::Stream(reader),
        output,
        channels,
        buffer,
        counter,
        shutdown,
    )
    .await?;

    for handle in spawned {
        handle.await.context("segment task panicked")??;
    }

    Ok(total)
}

/// Feed a subprocess and take its stdout back.
///
/// The feeding and draining halves run **concurrently**, not in sequence. A
/// loop that wrote a chunk and then read the reply would deadlock against any
/// filter that buffers (e.g. `sort`, `tac`, `gzip`) with a block pending: the
/// child blocks writing stdout because nobody is draining it, and we block
/// writing stdin because its pipe is full. Neither side moves.
///
/// A signal reaches the child the only way it can: the feeding half stops
/// reading and closes stdin, which is the child's end of stream. Draining is
/// then left alone until stdout closes, so a filter holding a block still gets
/// to write it out. A child that ignores its stdin closing keeps the drain
/// parked, which is what the second signal is for.
async fn run_process(
    external: ExternalStage,
    input: Upstream,
    output: Downstream,
    buffer: usize,
    counter: Option<Counter>,
    mut shutdown: Shutdown,
) -> anyhow::Result<u64> {
    let (program, args) = external
        .argv
        .split_first()
        .expect("the factory rejects an empty argv");

    let mut parts = child::spawn(program, args, external.shell, external.stderr, buffer)?;
    let name = external.name.clone();

    let feed = async {
        let mut stdin = parts.stdin;

        let total = match input {
            Upstream::Stream(ReadHalf::Stream(mut reader)) => {
                let mut buf = Buffer::new(buffer);
                let mut total = 0u64;

                while let Some(n) = read_or_stop(&mut reader, &mut buf, &mut shutdown).await? {
                    if n == 0 {
                        break;
                    }

                    stdin.write_all(&buf[..n]).await?;
                    total += n as u64;
                }

                total
            }
            Upstream::Stream(ReadHalf::Datagram(socket)) => {
                // A socket of our own never reaches end of stream, so stdin is
                // closed only when the relay stops or, on a forked
                // `udp-listen:` session, when its peer falls silent. Until
                // then this needs a filter that streams its output rather than
                // one that waits for end of stream.
                let mut buf = Buffer::new(buffer);
                let mut total = 0u64;

                loop {
                    if shutdown.is_triggered() {
                        break;
                    }

                    let received = tokio::select! {
                        biased;
                        received = socket.recv(&mut buf) => received?,
                        () = shutdown.recv() => break,
                    };

                    let Some(n) = received else {
                        break;
                    };

                    if let Some(counter) = &counter {
                        counter.add(n as u64);
                    }

                    stdin.write_all(&buf[..n]).await?;
                    stdin.flush().await?;
                    total += n as u64;
                }

                total
            }
            Upstream::Link(mut inlet) => {
                let mut total = 0u64;

                while let Some(parcel) = inlet.recv().await {
                    total += parcel.len() as u64;
                    stdin.write_all(&parcel).await?;
                    inlet.release(parcel);
                }
                total
            }
        };

        stdin.flush().await?;

        // Closing stdin is the only way the child sees EOF and flushes.
        drop(stdin);

        Ok::<u64, anyhow::Error>(total)
    };

    let drain = async {
        let mut stdout = parts.stdout;

        match output {
            Downstream::Stream(WriteHalf::Datagram(socket)) => {
                // One read becomes one datagram. The child's output has no
                // boundaries in it, so these are invented from wherever the
                // reads land.
                let mut buf = Buffer::new(buffer);

                loop {
                    let n = stdout.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    socket.send(&buf[..n]).await?;
                }

                // The child's stdout closing is this path's end of stream, and
                // a connected message sink has one to pass on. Same call the
                // stream arm below makes for the same reason.
                socket.finish();
            }
            Downstream::Stream(WriteHalf::Stream(mut writer)) => {
                let mut stdout = BufReader::with_capacity(buffer, stdout);
                tokio::io::copy_buf(&mut stdout, &mut writer).await?;
                writer.flush().await?;
                let _ = writer.shutdown().await;
            }
            Downstream::Link(mut outlet) => {
                let mut buf = Buffer::new(buffer);

                loop {
                    let n = stdout.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    outlet.send(&buf[..n]).await?;
                }

                drop(outlet);
            }
        }

        Ok::<(), anyhow::Error>(())
    };

    let diagnostics = async {
        if let Some(stderr) = parts.stderr.take() {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(line) = lines.next_line().await? {
                warn!(stage = %name, "{line}");
            }
        }

        Ok::<(), anyhow::Error>(())
    };

    let (total, (), ()) = tokio::try_join!(feed, drain, diagnostics)?;

    let status = parts.child.wait().await.context("waiting on child")?;
    if !status.success() {
        bail!(
            "stage `{}` exited {status}; its output is incomplete",
            external.name
        );
    }

    Ok(total)
}

/// Where a segment's bytes come from.
enum Source {
    Stream {
        reader: BoxRead,
        buf: Buffer,
        /// Watched between reads: a signal ends this source the same way the
        /// peer closing does, so the stages above still see end of stream.
        shutdown: Shutdown,
    },
    /// One `next` call is one datagram.
    Datagram {
        socket: DatagramSocket,
        buf: Buffer,
        /// A datagram socket has no reader to wrap, so the progress display's
        /// counter is applied here instead.
        counter: Option<Counter>,
        /// The only thing that ever ends a datagram source.
        shutdown: Shutdown,
    },
    Link {
        inlet: Inlet,
        spent: Option<Vec<u8>>,
    },
}

impl Source {
    fn new(
        upstream: Upstream,
        buffer: usize,
        counter: Option<Counter>,
        shutdown: Shutdown,
    ) -> Self {
        match upstream {
            Upstream::Stream(ReadHalf::Stream(reader)) => Source::Stream {
                reader,
                buf: Buffer::new(buffer),
                shutdown,
            },
            Upstream::Stream(ReadHalf::Datagram(socket)) => Source::Datagram {
                socket,
                buf: Buffer::new(buffer),
                counter,
                shutdown,
            },
            // Deliberately dropped: this segment ends when the one above it
            // closes the link, which is how `on_eof` cascades in order.
            Upstream::Link(inlet) => Source::Link { inlet, spent: None },
        }
    }
}

impl Source {
    /// The next chunk, or `None` at end of stream.
    ///
    /// Returns a borrow, which is why a spent parcel is recycled *here* rather
    /// than at the end of the caller's loop body: releasing it there would
    /// need `&mut self` while the borrow is still live.
    async fn next(&mut self) -> anyhow::Result<Option<&[u8]>> {
        match self {
            Source::Stream {
                reader,
                buf,
                shutdown,
            } => {
                let Some(n) = read_or_stop(reader, buf, shutdown).await? else {
                    return Ok(None);
                };

                Ok((n > 0).then(|| &buf[..n]))
            }
            Source::Datagram {
                socket,
                buf,
                counter,
                shutdown,
            } => {
                // A socket of our own has no EOF: it stops when the relay does.
                // A forked `udp-listen:` session does end, when its peer falls
                // silent, and reports that the same way a closed stream would.
                // A zero-length datagram is legal and distinct from "no more",
                // which is why the end is `None` and never an empty message.
                if shutdown.is_triggered() {
                    return Ok(None);
                }

                let received = tokio::select! {
                    biased;
                    received = socket.recv(&mut buf[..]) => received?,
                    () = shutdown.recv() => return Ok(None),
                };

                let Some(n) = received else {
                    return Ok(None);
                };

                if let Some(counter) = counter {
                    counter.add(n as u64);
                }

                Ok(Some(&buf[..n]))
            }
            Source::Link { inlet, spent } => {
                if let Some(parcel) = spent.take() {
                    inlet.release(parcel);
                }

                match inlet.recv().await {
                    Some(parcel) => Ok(Some(&spent.insert(parcel)[..])),
                    None => Ok(None),
                }
            }
        }
    }
}

/// Where a segment's bytes go.
enum Dest {
    Stream(BoxWrite),
    /// One unit is one datagram. Nothing declared any framing for the
    /// overwhelming majority of chunks, which is one unit, so this is the
    /// one-in-one-out mapping a datagram path expects.
    Datagram(DatagramSocket),
    Link(Outlet),
}

impl From<Downstream> for Dest {
    fn from(downstream: Downstream) -> Self {
        match downstream {
            Downstream::Stream(WriteHalf::Stream(writer)) => Dest::Stream(writer),
            Downstream::Stream(WriteHalf::Datagram(socket)) => Dest::Datagram(socket),
            Downstream::Link(outlet) => Dest::Link(outlet),
        }
    }
}

impl Dest {
    /// False when there is nowhere left to send. See [`Outlet::send`].
    async fn send(&mut self, emitted: Emitted<'_>) -> anyhow::Result<bool> {
        match self {
            // A byte stream has no framing of its own, so units are written
            // together: the peer cannot tell one call from several, and one
            // is cheaper.
            Dest::Stream(writer) => {
                if !emitted.is_empty() {
                    writer.write_all(emitted.bytes()).await?;
                }
            }
            Dest::Datagram(socket) => {
                // One unit, one message.
                //
                // Nothing is sent for an empty emission. `run_pipeline` calls
                // this once more at end of stream with whatever `finish`
                // produced, which is usually nothing. On a datagram sink that
                // would put a spurious zero-length message on the wire, and
                // many peers read that as end of stream and close.
                //
                // The cost is that a genuine zero-length datagram is dropped
                // rather than forwarded. That is the rarer case, and a silent
                // extra message is worse than a missing empty one.
                for unit in emitted.units() {
                    if !unit.is_empty() {
                        socket.send(unit).await?;
                    }
                }
            }
            // One unit, one parcel, so the segment downstream is called once
            // per unit and the framing survives the hop.
            Dest::Link(outlet) => {
                for unit in emitted.units() {
                    if !outlet.send(unit).await? {
                        return Ok(false);
                    }
                }
            }
        }

        Ok(true)
    }

    /// End of stream: flush a writer, or close the channel so the segment
    /// downstream sees EOF.
    ///
    /// A datagram destination has nothing to flush, and only a connected one
    /// has an end of stream to pass on, which is
    /// [`DatagramSocket::finish`]'s business rather than this one's.
    async fn finish(self) -> anyhow::Result<()> {
        match self {
            Dest::Stream(mut writer) => {
                writer.flush().await?;
                let _ = writer.shutdown().await;
            }
            Dest::Datagram(socket) => socket.finish(),
            Dest::Link(outlet) => drop(outlet),
        }

        Ok(())
    }
}

/// Pump one inline segment, whichever ends it has.
///
/// Returns the bytes read, which is only meaningful when the input is a stream.
async fn run_pipeline(
    mut input: Source,
    mut output: Dest,
    mut pipeline: Pipeline,
    channels: Arc<Channels>,
) -> anyhow::Result<u64> {
    let mut effects = Effects::new(&channels);
    let mut total = 0u64;

    // One timer for the whole segment, at the shortest period any stage in it
    // asked for. `None` for the overwhelming majority of pipelines.
    let mut ticker = pipeline.tick_interval().map(|period| {
        let mut ticker = tokio::time::interval(period);

        // A segment that fell behind should not then fire a burst of catch-up
        // ticks at a stage that only wanted to know the time.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker
    });

    loop {
        let arrived = match ticker.as_mut() {
            Some(ticker) => {
                tokio::select! {
                    biased;
                    chunk = input.next() => chunk?,
                    _ = ticker.tick() => {
                        let alive =
                            drive_ticks(&mut pipeline, &mut output, &channels, &mut effects)
                                .await?;

                        if !alive || !flow(&mut effects).await {
                            break;
                        }

                        continue;
                    }
                }
            }
            None => input.next().await?,
        };

        let Some(chunk) = arrived else {
            break;
        };

        total += chunk.len() as u64;

        // Borrows the read buffer when every stage passed through, so a chain
        // of observers hands the original bytes straight to the destination.
        let emitted = pipeline.process(chunk, &mut effects)?;

        // Nothing staged (no tee, or a chunk it swallowed): skip the join and
        // the apply future rather than polling one that would do nothing.
        let alive = if effects.is_empty() {
            output.send(emitted).await?
        } else {
            tokio::try_join!(output.send(emitted), channels.apply(&mut effects))?.0
        };

        // After the write, never before it: a stage that asked to stop or to
        // slow down still gets the bytes it emitted delivered first.
        if !alive || !flow(&mut effects).await {
            break;
        }
    }

    // Stages holding buffered bytes get one last chance to emit them.
    let emitted = pipeline.finish(&mut effects)?;
    output.send(emitted).await?;
    channels.apply(&mut effects).await?;
    output.finish().await?;

    Ok(total)
}

/// Apply the effects that act on this pump rather than on a side channel.
///
/// Returns false when a stage asked to stop reading, which the caller treats
/// exactly as upstream end of stream: emitted bytes are already out, `on_eof`
/// still cascades, and the path closes down normally. Stopping this way is a
/// decision, not a failure, so it is reported at info and the relay exits
/// successfully.
///
/// A pace request is honoured by simply not reading for that long. That is the
/// entire throttling mechanism: nothing is buffered here, and on a socket the
/// stalled read closes the receive window and slows the peer down at source.
async fn flow(effects: &mut Effects) -> bool {
    if let Some(reason) = effects.take_halt() {
        info!("{reason}");
        return false;
    }

    let pace = effects.take_pace();

    if !pace.is_zero() {
        sleep(pace).await;
    }

    true
}

/// Hand a turn to every stage whose schedule came due, writing out whatever
/// each one emitted before the next runs.
///
/// One `now` for the whole sweep, so two stages due on the same wakeup measure
/// against the same instant rather than against how long the first one took.
///
/// Returns false when the destination went away, which the caller treats as
/// end of stream exactly as it does on the data path. A ticking stage above a
/// segment that has finished would otherwise keep waking up and writing into a
/// closed channel for as long as the source stayed open.
async fn drive_ticks(
    pipeline: &mut Pipeline,
    output: &mut Dest,
    channels: &Channels,
    effects: &mut Effects,
) -> anyhow::Result<bool> {
    let now = Instant::now();
    let mut alive = true;

    while let Some(emitted) = pipeline.tick(now, &mut *effects)? {
        alive &= output.send(emitted).await?;
    }

    if !effects.is_empty() {
        channels.apply(effects).await?;
    }

    Ok(alive)
}

/// Sending half of a segment boundary, with a return path for spent buffers.
struct Outlet {
    data: mpsc::Sender<Vec<u8>>,
    back: mpsc::Receiver<Vec<u8>>,
}

/// Receiving half.
struct Inlet {
    data: mpsc::Receiver<Vec<u8>>,
    back: mpsc::Sender<Vec<u8>>,
}

fn link() -> (Outlet, Inlet) {
    let (data_tx, data_rx) = mpsc::channel(LINK_DEPTH);

    // One deeper so returning a buffer never blocks the consumer.
    let (back_tx, back_rx) = mpsc::channel(LINK_DEPTH + 1);

    (
        Outlet {
            data: data_tx,
            back: back_rx,
        },
        Inlet {
            data: data_rx,
            back: back_tx,
        },
    )
}

impl Outlet {
    /// Crossing a segment boundary is the one place a copy is unavoidable: the
    /// downstream task outlives this stack frame, so it needs owned bytes.
    ///
    /// Returns false when the segment downstream has finished and there is
    /// nobody left to send to. That is not a failure: a downstream that
    /// stopped on purpose (a `limit` stage reaching its cap, a sink that
    /// closed) is end of stream for this segment too, and a downstream that
    /// stopped because it broke reports its own error from its own task.
    async fn send(&mut self, bytes: &[u8]) -> anyhow::Result<bool> {
        if bytes.is_empty() {
            return Ok(true);
        }

        let mut buf = self
            .back
            .try_recv()
            .ok()
            .unwrap_or_else(|| Vec::with_capacity(bytes.len()));

        buf.clear();
        buf.extend_from_slice(bytes);

        Ok(self.data.send(buf).await.is_ok())
    }
}

impl Inlet {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        self.data.recv().await
    }

    /// Hand the buffer back upstream for reuse. Dropping it is fine too.
    fn release(&self, buf: Vec<u8>) {
        let _ = self.back.try_send(buf);
    }
}
