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

use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use tocat_api::{Chain, ExternalStage, Pipeline, Segment};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use tracing::warn;

use crate::{
    buffer::Buffer,
    child,
    endpoint::{BoxRead, BoxWrite, DatagramSocket, ReadHalf, WriteHalf},
    host::{Channels, Effects},
};

/// Chunks in flight per detached boundary. Deep enough to keep both sides busy,
/// shallow enough that backpressure still reaches the reader promptly.
const LINK_DEPTH: usize = 2;

/// Run one direction to completion, returning the bytes read from upstream.
pub async fn pump(
    reader: ReadHalf,
    writer: WriteHalf,
    chain: Chain,
    channels: Arc<Channels>,
    buffer: usize,
) -> anyhow::Result<u64> {
    let meta = chain.meta().clone();
    let mut segments = chain.into_segments();

    match segments.len() {
        // Datagrams cannot take the byte-stream shortcut: `copy_buf` is free to coalesce reads,
        // which would merge two messages into one sned. An empty pipeline preserves the
        // on-in-one-out mapping instead
        0 if matches!(reader, ReadHalf::Stream(_)) && matches!(writer, WriteHalf::Stream(_)) => {
            copy_direct(reader, writer, buffer).await
        }
        0 => {
            run_pipeline(
                Source::new(Upstream::Stream(reader), buffer),
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
            )
            .await
        }
        _ => run_segmented(reader, writer, segments, channels, buffer).await,
    }
}

/// Where a segment reads from: the endpoint, or the segment before it.
///
/// Concrete rather than generic. Only the head and tail ever hold a stream, so
/// a type parameter would be a phantom on every other segment — and one the
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
) -> anyhow::Result<u64> {
    match segment {
        Segment::Inline(pipeline) => {
            run_pipeline(
                Source::new(input, buffer),
                output.into(),
                pipeline,
                channels,
            )
            .await
        }
        Segment::Process(external) => run_process(external, input, output, buffer).await,
    }
}

/// No stages: runs straight through.
async fn copy_direct(reader: ReadHalf, writer: WriteHalf, buffer: usize) -> anyhow::Result<u64> {
    let (ReadHalf::Stream(reader), WriteHalf::Stream(mut writer)) = (reader, writer) else {
        unreachable!("checked by the caller");
    };

    let mut reader = BufReader::with_capacity(buffer, reader);
    let total = tokio::io::copy_buf(&mut reader, &mut writer).await?;

    writer.flush().await?;
    let _ = writer.shutdown().await;

    Ok(total)
}

/// Two or more segments, joined by bounded channels.
async fn run_segmented(
    reader: ReadHalf,
    writer: WriteHalf,
    mut segments: Vec<Segment>,
    channels: Arc<Channels>,
    buffer: usize,
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

        spawned.push(tokio::spawn(async move {
            run_segment(segment, Upstream::Link(inlet), output, channels, buffer)
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

    let total = run_segment(head, Upstream::Stream(reader), output, channels, buffer).await?;

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
async fn run_process(
    external: ExternalStage,
    input: Upstream,
    output: Downstream,
    buffer: usize,
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
            Upstream::Stream(ReadHalf::Stream(reader)) => {
                let mut reader = BufReader::with_capacity(buffer, reader);
                tokio::io::copy_buf(&mut reader, &mut stdin).await?
            }
            Upstream::Stream(ReadHalf::Datagram(socket)) => {
                // No EOF on a datagram source, so stdin is never closed and the
                // child never flushes on its own: this needs a filter that
                // streams its output. It ends when the relay does.
                let mut buf = Buffer::new(buffer);

                // Diverges: the loop has no break, so it types as `!` and the
                // byte count below is never reached.
                loop {
                    let n = socket.recv(&mut buf).await?;
                    stdin.write_all(&buf[..n]).await?;
                    stdin.flush().await?;
                }
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
    },
    /// One `next` call is one datagram.
    Datagram {
        socket: DatagramSocket,
        buf: Buffer,
    },
    Link {
        inlet: Inlet,
        spent: Option<Vec<u8>>,
    },
}

impl Source {
    fn new(upstream: Upstream, buffer: usize) -> Self {
        match upstream {
            Upstream::Stream(ReadHalf::Stream(reader)) => Source::Stream {
                reader,
                buf: Buffer::new(buffer),
            },
            Upstream::Stream(ReadHalf::Datagram(socket)) => Source::Datagram {
                socket,
                buf: Buffer::new(buffer),
            },
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
            Source::Stream { reader, buf } => {
                let n = reader.read(&mut buf[..]).await?;
                Ok((n > 0).then(|| &buf[..n]))
            }
            Source::Datagram { socket, buf } => {
                // A datagram socket has no EOF: it stops when the relay does. A
                // zero-length datagram is legal and distinct from "no more".
                let n = socket.recv(&mut buf[..]).await?;
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
    /// Whatever a stage emitted is sent as exactly one datagram.
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
    async fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        match self {
            Dest::Stream(writer) => {
                if !bytes.is_empty() {
                    writer.write_all(bytes).await?;
                }
            }
            Dest::Datagram(socket) => {
                // Not sent when empty. `run_pipeline` calls this once more at end of streamwith
                // whatever `finish` produced, which is usually nothing. On a
                // datagram sink that would put a spurious zero-length message on the wire. Many
                // peers read that as end of stream and close.
                //
                // The cost is that a genuine zero-length datagram is dropped rather than
                // forwarded. That is the rarer case, and a silent extra message
                // is worse than a missing empty one
                if !bytes.is_empty() {
                    socket.send(bytes).await?;
                }
            }
            Dest::Link(outlet) => outlet.send(bytes).await?,
        }

        Ok(())
    }

    /// End of stream: flush a writer, or close the channel so the segment
    /// downstream sees EOF.
    async fn finish(self) -> anyhow::Result<()> {
        match self {
            Dest::Stream(mut writer) => {
                writer.flush().await?;
                let _ = writer.shutdown().await;
            }
            Dest::Datagram(_) => {}
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

    while let Some(chunk) = input.next().await? {
        total += chunk.len() as u64;

        // Borrows the read buffer when every stage passed through, so a chain
        // of observers hands the original bytes straight to the destination.
        let bytes = pipeline.process(chunk, &mut effects)?;

        // Nothing staged (no tee, or a chunk it swallowed): skip the join and
        // the apply future rather than polling one that would do nothing.
        if effects.is_empty() {
            output.send(bytes).await?;
        } else {
            tokio::try_join!(output.send(bytes), channels.apply(&mut effects))?;
        }
    }

    // Stages holding buffered bytes get one last chance to emit them.
    let bytes = pipeline.finish(&mut effects)?;
    output.send(bytes).await?;
    channels.apply(&mut effects).await?;
    output.finish().await?;

    Ok(total)
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
    async fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        let mut buf = self
            .back
            .try_recv()
            .ok()
            .unwrap_or_else(|| Vec::with_capacity(bytes.len()));

        buf.clear();
        buf.extend_from_slice(bytes);

        self.data
            .send(buf)
            .await
            .map_err(|_| anyhow!("downstream segment stopped"))
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
