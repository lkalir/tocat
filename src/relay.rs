use std::sync::Arc;

use anyhow::Context;
use futures_util::TryFutureExt;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    net::{TcpListener, UnixListener},
    sync::Semaphore,
};
use tracing::{Instrument, error, info, warn};

use crate::{
    endpoint::{Direction, DumpConfig, EndpointSpec, EndpointStream, UnixSocketGuard, bind_unix},
    logging::DumpLogger,
    shutdown::Shutdown,
};

enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listener {
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

pub async fn copy_with_logging<R, W>(
    mut reader: R,
    mut writer: W,
    mut logger: DumpLogger,
) -> anyhow::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 256 * 1024].into_boxed_slice();
    let mut total_bytes = 0u64;

    loop {
        let n = reader.read(&mut buf).await?;

        if n == 0 {
            break;
        }

        let payload = &buf[..n];
        total_bytes += n as u64;

        tokio::try_join!(
            writer.write_all(payload).map_err(anyhow::Error::from),
            logger.log_bytes(payload)
        )?;
    }

    writer.flush().await?;
    let _ = writer.shutdown().await;
    logger.flush().await?;

    Ok(total_bytes)
}

fn is_fatal_accept(e: &std::io::Error) -> bool {
    !matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
    )
}

#[derive(Debug)]
struct PeerEndpoint {
    spec: EndpointSpec,
    dir: Direction,
}

async fn run_relay_listen(
    listen: EndpointSpec,
    peer_spec: PeerEndpoint,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let max = listen.max_connections();

    let (listener, _socket_guard) = Listener::bind(&listen).await?;
    info!(max = max.get(), "accepting connections");

    let permits = Arc::new(Semaphore::new(max.get()));
    let listen = Arc::new(listen);
    let peer_spec = Arc::new(peer_spec);
    let tracker = tokio_util::task::TaskTracker::new();

    loop {
        let permit = tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            p = permits.clone().acquire_owned() => p?,
        };

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

        let listen = listen.clone();
        let peer_spec = peer_spec.clone();
        let span = tracing::info_span!("conn", %peer);
        let value = peer.clone();

        tracker.spawn(
            async move {
                let _permit = permit;
                match handle_client(stream, &value, &listen, &peer_spec).await {
                    Ok(_) => info!("closed cleanly"),
                    Err(err) => error!(error =?err, "terminated with error"),
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

async fn copy_split(src: EndpointStream, sink: EndpointStream) -> anyhow::Result<u64> {
    let (src_read, src_write) = src.into_split();
    let (sink_read, sink_write) = sink.into_split();

    let mut src_read = BufReader::with_capacity(256 * 1024, src_read);
    let mut sink_read = BufReader::with_capacity(256 * 1024, sink_read);
    let mut src_write = BufWriter::with_capacity(256 * 1024, src_write);
    let mut sink_write = BufWriter::with_capacity(256 * 1024, sink_write);

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

async fn copy_fast(src_stream: EndpointStream, sink_stream: EndpointStream) -> anyhow::Result<u64> {
    if src_stream.is_duplex() && sink_stream.is_duplex() {
        let (mut a, mut b) = (src_stream.into_duplex(), sink_stream.into_duplex());
        let (x, y) =
            tokio::io::copy_bidirectional_with_sizes(&mut a, &mut b, 256 * 1024, 256 * 1024)
                .await?;
        return Ok(x + y);
    }

    copy_split(src_stream, sink_stream).await
}

async fn handle_client(
    accepted: EndpointStream,
    peer: &str,
    listen: &EndpointSpec,
    peer_spec: &PeerEndpoint,
) -> anyhow::Result<()> {
    let dialled = peer_spec.spec.connect(peer_spec.dir).await?;
    let _guard = dialled.guard;

    let (src_stream, sink_stream, src_spec, sink_spec) = match peer_spec.dir {
        Direction::Source => (dialled.stream, accepted, &peer_spec.spec, listen),
        Direction::Sink => (accepted, dialled.stream, listen, &peer_spec.spec),
    };

    let src_name = if peer_spec.dir == Direction::Sink {
        format!("{}_{}", src_spec.name(), peer)
    } else {
        src_spec.name()
    };

    let sink_name = if peer_spec.dir == Direction::Source {
        format!("{}_{}", sink_spec.name(), peer)
    } else {
        sink_spec.name()
    };

    relay_streams(
        src_stream,
        sink_stream,
        src_spec.dump_config().cloned(),
        sink_spec.dump_config().cloned(),
        &src_name,
        &sink_name,
    )
    .await
}

pub async fn run(
    source: EndpointSpec,
    sink: EndpointSpec,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    if source.is_fork() {
        let peer = PeerEndpoint {
            spec: sink,
            dir: Direction::Sink,
        };
        run_relay_listen(source, peer, shutdown).await
    } else if sink.is_fork() {
        let peer = PeerEndpoint {
            spec: source,
            dir: Direction::Source,
        };
        run_relay_listen(sink, peer, shutdown).await
    } else {
        tokio::select! {
            res = run_relay(&source, &sink) => res,
            _ = shutdown.recv() => {
                info!("interrupted");
                Ok(())
            }
        }
    }
}

async fn relay_streams(
    src_stream: EndpointStream,
    sink_stream: EndpointStream,
    src_cfg: Option<DumpConfig>,
    sink_cfg: Option<DumpConfig>,
    src_name: &str,
    sink_name: &str,
) -> anyhow::Result<()> {
    if src_cfg.is_none() && sink_cfg.is_none() {
        let total = copy_fast(src_stream, sink_stream).await?;
        info!(bytes = total, "relay finished");
        return Ok(());
    }

    let (src_read, src_write) = src_stream.into_split();
    let (sink_read, sink_write) = sink_stream.into_split();

    let (src_logger, shared) =
        DumpLogger::new_shared(format!("{src_name} -> {sink_name}"), src_cfg.clone(), None).await?;

    let same_file = match (&src_cfg, &sink_cfg) {
        (Some(a), Some(b)) => a.file.is_some() && a.file == b.file,
        _ => false,
    };
    let sink_file = if same_file { shared } else { None };
    let (sink_logger, _) =
        DumpLogger::new_shared(format!("{sink_name} -> {src_name}"), sink_cfg, sink_file).await?;

    let forward = copy_with_logging(src_read, sink_write, src_logger);
    let reverse = copy_with_logging(sink_read, src_write, sink_logger);

    let (a, b) = tokio::try_join!(forward, reverse)?;
    info!(bytes = a + b, "relay finished");

    Ok(())
}

pub async fn run_relay(source: &EndpointSpec, sink: &EndpointSpec) -> anyhow::Result<()> {
    let (src_conn, sink_conn) = if source.is_listen() && sink.is_listen() {
        tokio::try_join!(
            source.connect(Direction::Source),
            sink.connect(Direction::Sink)
        )?
    } else {
        (
            source.connect(Direction::Source).await?,
            sink.connect(Direction::Sink).await?,
        )
    };

    let _guards = (src_conn.guard, sink_conn.guard);

    relay_streams(
        src_conn.stream,
        sink_conn.stream,
        source.dump_config().cloned(),
        sink.dump_config().cloned(),
        &source.name(),
        &sink.name(),
    )
    .await
}
