//! relay.rs: connection lifecycle.
//!
//! [`Relay`] owns the two endpoints, the plugin declarations, and the side
//! channels those declarations resolved to. Construction is where validation
//! happens: every plugin is built once to discover which channels it wants, the
//! channels are opened, and the plan is then frozen. This way a misspelled
//! plugin or an unwritable dump file fails at startup, before either endpoint
//! is touched.
//!
//! Under `fork` the listening side accepts in a loop, bounded by a semaphore of
//! `max_connections` permits, and each connection gets **its own plugin
//! instances**: stages are stateful (byte offsets, codec state) and sharing
//! them across connections would interleave nonsense. Only the channel handles
//! are shared, which is how several connections can dump into one file. On
//! shutdown the listener stops accepting, every connection still in flight is
//! handed the same signal so it stops reading and drains its stages, and a
//! `TaskTracker` waits for them; a second signal exits immediately.
//!
//! A signal is not cancellation anywhere a plugin can be holding bytes. The
//! handle is passed down to `pump` rather than raced against it in a `select!`,
//! because dropping the copy future would skip `on_eof` and lose whatever a
//! stage had buffered. The exceptions are the two paths with no stages at all:
//! `copy_bidirectional_with_sizes` below, and the blocking copy, which cannot
//! be cancelled and so is abandoned instead.
//!
//! `relay_streams` picks the transport. With nothing declared on either path it
//! goes straight to `copy_bidirectional_with_sizes`, exactly as before plugins
//! existed; otherwise each direction is handed to `pump`, which has its own
//! fast path for a direction that happens to be empty.

use std::{
    io::{Read as _, Write as _},
    sync::Arc,
};

use anyhow::Context;
use tocat_api::{Chain, ChannelTarget, Direction as Flow, PluginSpec, Registry, Side};
use tokio::{
    net::{TcpListener, UnixListener},
    sync::Semaphore,
};
use tokio_seqpacket::UnixSeqpacketListener;
use tracing::{Instrument, debug, error, info, warn};

use crate::{
    buffer::Buffer,
    endpoint::{
        Demux, Direction, EndpointSpec, EndpointStream, PathGuard, ReadHalf, SyncRead, SyncWrite,
        WriteHalf,
    },
    host::{ChannelPlan, Channels},
    progress::{self, Counter, Meter},
    pump::pump,
    shutdown::Shutdown,
};

enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
    /// Connection oriented like the others, message oriented like the one
    /// below: an accepted seqpacket socket is a datagram endpoint.
    Seqpacket(UnixSeqpacketListener),
    /// A connectionless socket has no accept: a sender is discovered by
    /// receiving from it. The socket is demultiplexed by source address
    /// instead, and each new address arrives here as a session to serve.
    Datagram(Demux),
}

impl Listener {
    /// Start listener.
    async fn bind(
        spec: &EndpointSpec,
        buffer: usize,
        shutdown: Shutdown,
    ) -> anyhow::Result<(Self, Option<PathGuard>)> {
        match spec {
            EndpointSpec::TcpListen(e) => {
                let l = e.bind().await?;
                info!(local = %l.local_addr()?, "listening");
                Ok((Listener::Tcp(l), None))
            }
            EndpointSpec::UnixListen(e) => {
                let l = e.bind().await?;
                info!(path = %e.path, "listening");
                Ok((Listener::Unix(l), e.path.guard()))
            }
            EndpointSpec::UnixSeqpacketListen(e) => {
                let l = e.bind().await?;
                info!(path = %e.path, "listening");
                Ok((Listener::Seqpacket(l), e.path.guard()))
            }
            // The receive loops need the copy buffer for the same reason the
            // pump does: one receive is one message, and anything longer than
            // the buffer is truncated by the kernel.
            EndpointSpec::UnixDgramListen(e) => Ok((
                Listener::Datagram(e.demux(buffer, shutdown).await?),
                e.path.guard(),
            )),
            EndpointSpec::UdpListen(e) => {
                Ok((Listener::Datagram(e.demux(buffer, shutdown).await?), None))
            }
            _ => anyhow::bail!("fork is only supported on listening endpoints"),
        }
    }

    /// Allow a peer to connect.
    async fn accept(&mut self) -> std::io::Result<(EndpointStream, String)> {
        match self {
            Listener::Tcp(l) => {
                let (s, peer) = l.accept().await?;
                Ok((EndpointStream::tcp(s), peer.to_string()))
            }
            Listener::Unix(l) => {
                let (s, peer) = l.accept().await?;
                let label = peer
                    .as_pathname()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "unnamed".to_string());
                Ok((EndpointStream::unix(s), label))
            }
            Listener::Seqpacket(l) => {
                let s = l.accept().await?;

                // A connected unix socket is anonymous, so there is nothing to
                // name the peer with. The stream form says the same thing
                // whenever its client did not bind.
                Ok((EndpointStream::seqpacket(s), "unnamed".to_string()))
            }
            Listener::Datagram(d) => d.accept().await,
        }
    }
}

/// Set of errors that accept can return that should be considered fatal.
fn is_fatal_accept(e: &std::io::Error) -> bool {
    !matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
    )
}

/// Fast-path for copying data between synchronous streams.
fn copy_sync(
    mut reader: SyncRead,
    mut writer: SyncWrite,
    shutdown: &Shutdown,
    buffer: usize,
    counter: Option<Counter>,
) -> anyhow::Result<u64> {
    // Deliberately not `std::io::copy`: its kernel-offload specialisations only
    // fire for concrete types, and through a `dyn` it falls back to an 8 KiB
    // stack buffer: 32x the syscalls for the same bytes.
    let mut buf = Buffer::new(buffer);
    let mut total = 0u64;

    loop {
        // Checked per chunk because this task cannot be cancelled from outside.
        // A read that blocks indefinitely (a FIFO with no writer) still will
        // not notice; the runtime's shutdown timeout covers that.
        if shutdown.is_triggered() {
            info!(bytes = total, "interrupted");
            break;
        }

        let n = reader.read(&mut buf)?;

        if n == 0 {
            break;
        }

        writer.write_all(&buf[..n])?;
        total += n as u64;

        if let Some(counter) = &counter {
            counter.add(n as u64);
        }
    }

    writer.flush()?;

    Ok(total)
}

/// A configured relay: two endpoints, a plugin declaration list, and the side
/// channels those plugins resolved to.
pub struct Relay {
    source: EndpointSpec,
    sink: EndpointSpec,
    plugins: Vec<PluginSpec>,
    registry: Registry,
    /// Frozen after construction; cloned per connection to resolve handles.
    plan: ChannelPlan,
    channels: Arc<Channels>,
    buffer: usize,
    /// Shared with the progress display, when one is running.
    progress: Option<Arc<Meter>>,
}

impl Relay {
    /// Validate the plugin list and open every side channel it asks for.
    ///
    /// Chains are built once here purely to discover channels: the instances
    /// are dropped, since each connection needs its own. A bad declaration or
    /// an unopenable dump file therefore fails at startup, not on first byte.
    pub async fn new(
        source: EndpointSpec,
        sink: EndpointSpec,
        plugins: Vec<PluginSpec>,
        registry: Registry,
        buffer: usize,
        progress: Option<Arc<Meter>>,
    ) -> anyhow::Result<Self> {
        let mut plan = ChannelPlan::new();

        let (forward, reverse) =
            registry.build_pair(&plugins, &source.name(), &sink.name(), None, &mut plan)?;

        debug!(
            forward = ?forward.stage_names(),
            reverse = ?reverse.stage_names(),
            forward_segments = forward.segments().len(),
            reverse_segments = reverse.segments().len(),
            channels = plan.targets().len(),
            "plugin chains resolved",
        );

        // Two checks over the same declarations, and the difference between
        // them is the whole point. A stage that reshapes bytes cannot preserve
        // message boundaries, so a datagram *sink* may receive well-formed
        // messages containing nonsense: warn, because the operator may know the
        // peer tolerates it, and `block` at the MTU is a reasonable ask.
        //
        // A stage that *needs* boundaries is not in that position. It cannot do
        // its job where it was put, whatever the peer tolerates, so an unmet
        // requirement fails the build.
        //
        // Both are checked per direction, against that direction's own ends. A
        // datagram source feeding a stream sink loses nothing.
        let mut faults = Vec::new();

        for (chain, upstream, downstream, direction) in [
            (&forward, &source, &sink, "source-to-sink"),
            (&reverse, &sink, &source, "sink-to-source"),
        ] {
            if downstream.is_datagram()
                && let Some(stage) = chain.datagram_hazard()
            {
                warn!(
                    stage, direction, endpoint = %downstream.name(),
                    "stage may not preserve message boundaries; datagrams sent to this endpoint \
                    may be split, merged, or malformed",
                );
            }

            for fault in chain.boundary_faults(upstream.is_datagram(), downstream.is_datagram()) {
                let (want, remedy, end) = match fault.side {
                    Side::Upstream => ("arriving", "an unframe above it", upstream),
                    Side::Downstream => ("to survive", "a frame below it", downstream),
                };

                // Naming what broke it is the difference between a message an
                // operator can act on and one that only says no.
                let cause = match fault.cause {
                    Some(stage) => format!("{stage} does not carry them"),
                    None => format!(
                        "the {} {} is a byte stream",
                        fault.side.endpoint_role(),
                        end.name(),
                    ),
                };

                faults.push(format!(
                    "{} on {direction} needs message boundaries {want}, and {cause}: put {remedy}, \
                     or use a stage that does not need them",
                    fault.stage,
                ));
            }
        }

        if !faults.is_empty() {
            anyhow::bail!("{}", faults.join("\n"));
        }

        plan.freeze();

        // Both would be writing to the same terminal, and only one of them
        // knows about the progress line. The dump wins the collision, since it
        // is the payload.
        if progress.is_some()
            && plan
                .targets()
                .iter()
                .any(|target| matches!(target, ChannelTarget::Stderr))
        {
            warn!(
                "a plugin is dumping to stderr while the progress line is drawn there; send the \
                 dump to a file, or drop --progress",
            );
        }

        let channels = Channels::open(plan.targets()).await?;

        // One buffer per direction per connection: worth saying out loud before
        // someone pairs a large buffer with a high connection ceiling.
        let peak = buffer.saturating_mul(source.max_connections().get().max(1)) * 2;
        if peak > 1024 * 1024 * 1024 {
            warn!(
                buffer,
                "buffer size and connection ceiling allow over 1 GiB of copy buffers"
            );
        }

        Ok(Self {
            source,
            sink,
            plugins,
            registry,
            plan,
            channels,
            buffer,
            progress,
        })
    }

    pub async fn run(self, shutdown: Shutdown) -> anyhow::Result<()> {
        let this = Arc::new(self);
        let channels = this.channels.clone();

        let result = this.dispatch(shutdown).await;

        // Buffered channel writers must not lose their tail on exit.
        if let Err(e) = channels.flush().await {
            warn!(error = %e, "flushing plugin channels failed");
        }

        result
    }

    async fn dispatch(self: Arc<Self>, mut shutdown: Shutdown) -> anyhow::Result<()> {
        if self.source.is_fork() {
            self.serve(Direction::Sink, shutdown).await
        } else if self.sink.is_fork() {
            self.serve(Direction::Source, shutdown).await
        } else if self.prefers_sync() {
            // The one path that has to be abandoned rather than drained: a
            // `spawn_blocking` task cannot be cancelled, and a read that never
            // returns would hold the runtime open forever. Nothing is owed an
            // `on_eof` here, since this path is only taken with no plugins.
            let watcher = shutdown.clone();

            tokio::select! {
                res = self.run_sync(watcher) => res,
                () = shutdown.recv() => {
                    info!("interrupted");
                    Ok(())
                }
            }
        } else {
            // Not a `select!`: the signal is delivered *into* the copy, which
            // stops reading and then runs the stages' end-of-stream path.
            // Dropping this future would skip that.
            self.run_once(shutdown).await
        }
    }

    /// Build a fresh chain pair. Instances are stateful and per-connection;
    /// only the channel handles are shared.
    fn chains(
        &self,
        src_name: &str,
        sink_name: &str,
        peer: Option<&str>,
    ) -> anyhow::Result<(Chain, Chain)> {
        let mut plan = self.plan.clone();
        Ok(self
            .registry
            .build_pair(&self.plugins, src_name, sink_name, peer, &mut plan)?)
    }

    /// Both ends blocking-backed and nothing declared: tokio buys us nothing
    /// here and costs two userspace copies of every byte.
    fn prefers_sync(&self) -> bool {
        self.plugins.is_empty()
            && self.source.is_blocking_backed()
            && self.sink.is_blocking_backed()
    }

    /// A plain `read`/`write` loop on the blocking pool: one buffer, no
    /// intermediate copies. Structurally what socat does.
    ///
    /// Note this cannot be interrupted mid-transfer: a blocking read is not
    /// cancellable, so a shutdown signal takes effect when the current read
    /// returns. Sockets, where that would matter, never take this path.
    async fn run_sync(&self, shutdown: Shutdown) -> anyhow::Result<()> {
        let mut source = self.source.connect_sync(Direction::Source, self.buffer)?;
        let mut sink = self.sink.connect_sync(Direction::Sink, self.buffer)?;

        // Held until every copy has finished: dropping these unlinks a `pipe:`
        // opened with `unlink`, and doing that early would remove the path out
        // from under a producer still writing to it.
        let _guards = (source.guard.take(), sink.guard.take());

        // A direction with no reader or no writer does not exist. Skipping it
        // matters: a `file:` source paired with stdio would otherwise park a
        // thread on a stdin read whose bytes go straight to a null sink, and
        // hold the relay open waiting for an EOF nobody will send.
        let directions = [
            (source.reader, sink.writer, Flow::SourceToSink),
            (sink.reader, source.writer, Flow::SinkToSource),
        ];

        let mut running = Vec::new();
        for (reader, writer, path) in directions {
            if let (Some(reader), Some(writer)) = (reader, writer) {
                let shutdown = shutdown.clone();
                let buffer = self.buffer;
                let counter = self.progress.as_ref().map(|meter| meter.counter(path));

                running.push(tokio::task::spawn_blocking(move || {
                    copy_sync(reader, writer, &shutdown, buffer, counter)
                }));
            }
        }

        let mut total = 0u64;
        for task in running {
            total += task.await.context("blocking copy task panicked")??;
        }

        info!(bytes = total, "relay finished");

        Ok(())
    }

    /// One direction, if it exists at all.
    ///
    /// A direction with no reader or no writer does not exist, which is not the
    /// same as one that is empty. A `file:` source has nothing to be written
    /// back to, so the reverse direction is a read from the sink whose bytes
    /// have nowhere to go, and on a sink that never reaches end of stream, a
    /// datagram socket or a held FIFO, that read never returns: the direction
    /// that matters finishes and the relay then waits forever on the one that
    /// could not have carried anything. [`Self::run_sync`] has always skipped
    /// these; this is the async path doing the same.
    async fn pump_direction(
        &self,
        reader: Option<ReadHalf>,
        writer: Option<WriteHalf>,
        chain: Chain,
        flow: Flow,
        shutdown: Shutdown,
    ) -> anyhow::Result<u64> {
        let (Some(reader), Some(writer)) = (reader, writer) else {
            if chain.is_empty() {
                debug!(?flow, "one way, skipping the direction that does not exist");
            } else {
                // Worth saying out loud: the stages were declared, built and
                // are about to do nothing at all.
                warn!(?flow, "no such direction, so its stages will not run");
            }

            return Ok(0);
        };

        // Counting happens at the endpoint, before any stage sees the bytes.
        let (reader, counter) = progress::count(self.progress.as_ref(), reader, flow);

        pump(
            reader,
            writer,
            chain,
            self.channels.clone(),
            self.buffer,
            counter,
            shutdown,
        )
        .await
    }

    /// Drive one connection.
    ///
    /// Delegates to a fast-path if there are no plugins. A method rather than a
    /// free function because the buffer size, the channels and the meter are
    /// all on `self` and both callers had to hand them over one at a time.
    async fn relay_streams(
        &self,
        src_stream: EndpointStream,
        sink_stream: EndpointStream,
        forward: Chain,
        reverse: Chain,
        mut shutdown: Shutdown,
    ) -> anyhow::Result<()> {
        let buffer = self.buffer;

        // Nothing declared and both ends duplex byte streams: hand the whole thing to
        // tokio and stay out of the way.
        //
        // Not while a meter is running: `copy_bidirectional` offers nowhere to
        // count from, and the split path below is where a read half can be
        // wrapped. Measuring costs the shortcut.
        let (src_stream, sink_stream) =
            if forward.is_empty() && reverse.is_empty() && self.progress.is_none() {
                match (src_stream, sink_stream) {
                    (EndpointStream::Duplex(mut a), EndpointStream::Duplex(mut b)) => {
                        // Dropping the copy is only acceptable because nothing
                        // was declared: no stage is holding bytes and no
                        // `on_eof` is owed to anyone. Every path that has one
                        // goes through `pump`, which drains instead.
                        tokio::select! {
                            copied = tokio::io::copy_bidirectional_with_sizes(
                                &mut a, &mut b, buffer, buffer,
                            ) => {
                                let (to_sink, to_source) = copied?;
                                info!(bytes = to_sink + to_source, "relay finished");
                            }
                            () = shutdown.recv() => info!("interrupted"),
                        }

                        return Ok(());
                    }
                    pair => pair,
                }
            } else {
                (src_stream, sink_stream)
            };

        // A chain on one path only still leaves the other on a plain copy: `pump`
        // dispatches per direction.
        let (src_read, src_write) = src_stream.into_halves();
        let (sink_read, sink_write) = sink_stream.into_halves();

        let (a, b) = tokio::try_join!(
            self.pump_direction(
                src_read,
                sink_write,
                forward,
                Flow::SourceToSink,
                shutdown.clone(),
            ),
            self.pump_direction(
                sink_read,
                src_write,
                reverse,
                Flow::SinkToSource,
                shutdown.clone(),
            ),
        )?;

        info!(bytes = a + b, "relay finished");
        self.channels.flush().await?;

        Ok(())
    }

    /// One connection, on the async path. The sync path is chosen by
    /// [`Self::dispatch`], which has to treat a signal differently.
    async fn run_once(&self, shutdown: Shutdown) -> anyhow::Result<()> {
        let (src_conn, sink_conn) = if self.source.is_listen() && self.sink.is_listen() {
            // This prevents clients connecting to the sink from needlessly being blocked on
            // waiting for clients to connect to the source first
            tokio::try_join!(
                self.source.connect(Direction::Source, self.buffer),
                self.sink.connect(Direction::Sink, self.buffer)
            )?
        } else {
            (
                self.source.connect(Direction::Source, self.buffer).await?,
                self.sink.connect(Direction::Sink, self.buffer).await?,
            )
        };

        let _guards = (src_conn.guard, sink_conn.guard);

        let (forward, reverse) = self.chains(&self.source.name(), &self.sink.name(), None)?;

        self.relay_streams(
            src_conn.stream,
            sink_conn.stream,
            forward,
            reverse,
            shutdown,
        )
        .await
    }

    fn listening(&self, peer_dir: Direction) -> &EndpointSpec {
        match peer_dir {
            Direction::Sink => &self.source,
            Direction::Source => &self.sink,
        }
    }

    /// `peer_dir` is the role of the *dialled* endpoint; the other one listens.
    async fn serve(
        self: Arc<Self>,
        peer_dir: Direction,
        mut shutdown: Shutdown,
    ) -> anyhow::Result<()> {
        let listen = self.listening(peer_dir);
        let max = listen.max_connections();

        let (mut listener, _socket_guard) =
            Listener::bind(listen, self.buffer, shutdown.clone()).await?;
        info!(max = max.get(), "accepting connections");

        let permits = Arc::new(Semaphore::new(max.get()));
        let tracker = tokio_util::task::TaskTracker::new();

        loop {
            // Acquire semaphore (or get canceled)
            let permit = tokio::select! {
                biased;
                _ = shutdown.recv() => break,
                p = permits.clone().acquire_owned() => p?,
            };

            // Accept peer connection (or get canceled)
            let (stream, peer) = tokio::select! {
                biased;
                _ = shutdown.recv() => break,
                conn = listener.accept() => match conn {
                    Ok(conn) => conn,
                    Err(e) if is_fatal_accept(&e) => return Err(e).context("accept"),
                    Err(e) => {
                        warn!("Accept error: {e}");
                        continue;
                    }
                },
            };

            let this = Arc::clone(&self);
            let span = tracing::info_span!("conn", %peer);

            // Its own handle, so a connection already in flight stops reading
            // on the signal and drains rather than running until its peer
            // happens to hang up. Without this the tracker below waits on
            // connections that were never told anything had changed.
            let shutdown = shutdown.clone();

            // Let handler task go off and handle the connection
            tracker.spawn(
                async move {
                    let _permit = permit;
                    match this.handle_client(stream, &peer, peer_dir, shutdown).await {
                        Ok(()) => info!("closed cleanly"),
                        Err(err) => error!(error = ?err, "terminated with error"),
                    }
                }
                .instrument(span),
            );
        }

        tracker.close();
        info!(active = tracker.len(), "waiting for connections to drain");
        tracker.wait().await;
        info!("drained");

        Ok(())
    }

    async fn handle_client(
        &self,
        accepted: EndpointStream,
        peer: &str,
        peer_dir: Direction,
        shutdown: Shutdown,
    ) -> anyhow::Result<()> {
        // Counted for the display's connection gauge until this returns.
        let _connection = self.progress.as_ref().map(|meter| meter.connected());

        let listen = self.listening(peer_dir);
        let peer_spec = match peer_dir {
            Direction::Sink => &self.sink,
            Direction::Source => &self.source,
        };

        let dialled = peer_spec.connect(peer_dir, self.buffer).await?;
        let _guard = dialled.guard;

        let (src_stream, sink_stream, src_spec, sink_spec) = match peer_dir {
            Direction::Source => (dialled.stream, accepted, peer_spec, listen),
            Direction::Sink => (accepted, dialled.stream, listen, peer_spec),
        };

        let src_name = if peer_dir == Direction::Sink {
            format!("{}_{}", src_spec.name(), peer)
        } else {
            src_spec.name()
        };

        let sink_name = if peer_dir == Direction::Source {
            format!("{}_{}", sink_spec.name(), peer)
        } else {
            sink_spec.name()
        };

        let (forward, reverse) = self.chains(&src_name, &sink_name, Some(peer))?;

        self.relay_streams(src_stream, sink_stream, forward, reverse, shutdown)
            .await
    }
}
