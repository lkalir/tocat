//! relay.rs — connection lifecycle.
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
//! shutdown the listener stops accepting and a `TaskTracker` drains what is
//! still in flight; a second signal exits immediately.
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
use tocat_api::{Chain, PluginSpec, Registry};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{TcpListener, UnixListener},
    sync::Semaphore,
};
use tracing::{Instrument, debug, error, info, warn};

use crate::{
    endpoint::{
        Direction, EndpointSpec, EndpointStream, SyncRead, SyncWrite, UnixSocketGuard, bind_unix,
    },
    host::{ChannelPlan, Channels},
    pump::{BUF, pump},
    shutdown::Shutdown,
};

enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listener {
    /// Start listener.
    async fn bind(spec: &EndpointSpec) -> anyhow::Result<(Self, Option<UnixSocketGuard>)> {
        match spec {
            EndpointSpec::TcpListen { host, port, .. } => {
                let host = host.as_deref().unwrap_or("localhost");
                let port = port.unwrap_or(8000);
                let l = TcpListener::bind((host, port)).await?;
                info!(%host, port, "listening");
                Ok((Listener::Tcp(l), None))
            }
            EndpointSpec::UnixListen {
                path, unlink, mode, ..
            } => {
                let l = bind_unix(path, *unlink, *mode).await?;
                info!(path = %path.display(), "listening");
                Ok((Listener::Unix(l), Some(UnixSocketGuard(path.clone()))))
            }
            _ => anyhow::bail!("fork is only supported on listening endpoints"),
        }
    }

    /// Allow a peer to connect.
    async fn accept(&self) -> std::io::Result<(EndpointStream, String)> {
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

/// Fast-path for copying data between split endpoints.
async fn copy_split(src: EndpointStream, sink: EndpointStream) -> anyhow::Result<u64> {
    let (src_read, src_write) = src.into_split();
    let (sink_read, sink_write) = sink.into_split();

    // Readers are buffered because `copy_buf` needs `AsyncBufRead`. Writers are
    // not: `copy_buf` hands them a full 256 KiB slice already, so a `BufWriter`
    // would only copy the payload into a second buffer to write the same bytes.
    let mut src_read = BufReader::with_capacity(BUF, src_read);
    let mut sink_read = BufReader::with_capacity(BUF, sink_read);
    let mut src_write = src_write;
    let mut sink_write = sink_write;

    let forward = async {
        let n = tokio::io::copy_buf(&mut src_read, &mut sink_write).await?;
        sink_write.shutdown().await?;
        Ok::<_, std::io::Error>(n)
    };

    let reverse = async {
        let n = tokio::io::copy_buf(&mut sink_read, &mut src_write).await?;
        src_write.shutdown().await?;
        Ok::<_, std::io::Error>(n)
    };

    let (a, b) = tokio::try_join!(forward, reverse)?;
    Ok(a + b)
}

/// Fast-path for copying data between synchronous streams.
fn copy_sync(
    mut reader: SyncRead,
    mut writer: SyncWrite,
    shutdown: &Shutdown,
) -> anyhow::Result<u64> {
    // Deliberately not `std::io::copy`: its kernel-offload specialisations only
    // fire for concrete types, and through a `dyn` it falls back to an 8 KiB
    // stack buffer: 32x the syscalls for the same bytes.
    let mut buf = vec![0u8; BUF].into_boxed_slice();
    let mut total = 0u64;

    loop {
        // Checked per chunk because this task cannot be cancelled from outside.
        // A read that blocks indefinitely — a FIFO with no writer — still will
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
    }

    writer.flush()?;

    Ok(total)
}

/// The plugin-free fast-path.
async fn copy_fast(src_stream: EndpointStream, sink_stream: EndpointStream) -> anyhow::Result<u64> {
    // If both streams are duplex, we can use the fast bidirectional copy from tokio
    if src_stream.is_duplex() && sink_stream.is_duplex() {
        let (mut a, mut b) = (src_stream.into_duplex(), sink_stream.into_duplex());
        let (x, y) = tokio::io::copy_bidirectional_with_sizes(&mut a, &mut b, BUF, BUF).await?;
        return Ok(x + y);
    }

    copy_split(src_stream, sink_stream).await
}

/// Drive the relay.
///
/// Delegates to a fast-path if there are no plugins.
async fn relay_streams(
    src_stream: EndpointStream,
    sink_stream: EndpointStream,
    forward: Chain,
    reverse: Chain,
    channels: Arc<Channels>,
) -> anyhow::Result<()> {
    // Nothing declared on either path: keep the old fast path exactly.
    if forward.is_empty() && reverse.is_empty() {
        let total = copy_fast(src_stream, sink_stream).await?;
        info!(bytes = total, "relay finished");
        return Ok(());
    }

    // A chain on one path only still leaves the other on a plain copy: `pump`
    // dispatches per direction.
    let (src_read, src_write) = src_stream.into_split();
    let (sink_read, sink_write) = sink_stream.into_split();

    let (a, b) = tokio::try_join!(
        pump(src_read, sink_write, forward, channels.clone()),
        pump(sink_read, src_write, reverse, channels.clone()),
    )?;

    info!(bytes = a + b, "relay finished");
    channels.flush().await?;

    Ok(())
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

        plan.freeze();
        let channels = Channels::open(plan.targets()).await?;

        Ok(Self {
            source,
            sink,
            plugins,
            registry,
            plan,
            channels,
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
        } else {
            // The blocking path needs its own handle: dropping the future on
            // shutdown detaches the task rather than stopping it.
            let watcher = shutdown.clone();

            tokio::select! {
                res = self.run_once(watcher) => res,
                _ = shutdown.recv() => {
                    info!("interrupted");
                    Ok(())
                }
            }
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
        let source = self.source.connect_sync(Direction::Source)?;
        let sink = self.sink.connect_sync(Direction::Sink)?;

        // A direction with no reader or no writer does not exist. Skipping it
        // matters: a `file:` source paired with stdio would otherwise park a
        // thread on a stdin read whose bytes go straight to a null sink, and
        // hold the relay open waiting for an EOF nobody will send.
        let directions = [(source.reader, sink.writer), (sink.reader, source.writer)];

        let mut running = Vec::new();
        for (reader, writer) in directions {
            if let (Some(reader), Some(writer)) = (reader, writer) {
                let shutdown = shutdown.clone();

                running.push(tokio::task::spawn_blocking(move || {
                    copy_sync(reader, writer, &shutdown)
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

    async fn run_once(&self, shutdown: Shutdown) -> anyhow::Result<()> {
        if self.prefers_sync() {
            return self.run_sync(shutdown).await;
        }

        let (src_conn, sink_conn) = if self.source.is_listen() && self.sink.is_listen() {
            // This prevents clients connecting to the sink from needlessly being blocked on
            // waiting for clients to connect to the source first
            tokio::try_join!(
                self.source.connect(Direction::Source),
                self.sink.connect(Direction::Sink)
            )?
        } else {
            (
                self.source.connect(Direction::Source).await?,
                self.sink.connect(Direction::Sink).await?,
            )
        };

        let _guards = (src_conn.guard, sink_conn.guard);

        let (forward, reverse) = self.chains(&self.source.name(), &self.sink.name(), None)?;

        relay_streams(
            src_conn.stream,
            sink_conn.stream,
            forward,
            reverse,
            self.channels.clone(),
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

        let (listener, _socket_guard) = Listener::bind(listen).await?;
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

            // Let handler task go off and handle the connection
            tracker.spawn(
                async move {
                    let _permit = permit;
                    match this.handle_client(stream, &peer, peer_dir).await {
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
    ) -> anyhow::Result<()> {
        let listen = self.listening(peer_dir);
        let peer_spec = match peer_dir {
            Direction::Sink => &self.sink,
            Direction::Source => &self.source,
        };

        let dialled = peer_spec.connect(peer_dir).await?;
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

        relay_streams(
            src_stream,
            sink_stream,
            forward,
            reverse,
            self.channels.clone(),
        )
        .await
    }
}
