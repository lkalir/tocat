//! reconnect.rs: what happens when a connection that was working goes away.
//!
//! The distinction under test is failure against end of stream. A peer that
//! closes has said it is finished and the run ends; a peer whose connection
//! breaks has said nothing, and `reconnect=restart` reopens the pair. Getting
//! that backwards makes an ordinary relay impossible to end, so both halves are
//! asserted here rather than only the interesting one.
//!
//! The source listens on a unix socket rather than a port: a restart rebinds
//! it immediately, and a TCP listener in that position needs `reuseaddr` to
//! avoid a bind refused by a connection still in TIME_WAIT. That is a real
//! caveat for users and a source of flakes for tests.
//!
//! The helpers here overlap with loopbacks.rs. When a third file needs them
//! they should move to tests/common/mod.rs; two is not yet worth the module.

use std::{future::Future, path::Path, time::Duration};

use tocat::{
    endpoint::EndpointSpec,
    relay::Relay,
    shutdown::{self, Trigger},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, UnixStream},
    task::JoinHandle,
    time::{sleep, timeout},
};

const LOOPBACK_ANY: &str = "127.0.0.1:0";
const CASE: Duration = Duration::from_secs(20);
const READY: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const BUFFER: usize = 64 * 1024;

async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

/// Build a relay without starting it, so a case can assert on the построение
/// failing.
async fn build(source: &str, sink: &str) -> anyhow::Result<Relay> {
    let source: EndpointSpec = source.parse().expect("source spec");
    let sink: EndpointSpec = sink.parse().expect("sink spec");

    Relay::new(
        source,
        sink,
        Vec::new(),
        tocat_plugins::native_registry(),
        BUFFER,
        None,
    )
    .await
}

async fn start(source: &str, sink: &str) -> (JoinHandle<anyhow::Result<()>>, Trigger) {
    let relay = build(source, sink).await.expect("relay construction");
    let (trigger, shutdown) = shutdown::channel();

    (tokio::spawn(relay.run(shutdown)), trigger)
}

async fn connect_unix(path: &Path) -> UnixStream {
    timeout(READY, async {
        loop {
            match UnixStream::connect(path).await {
                Ok(stream) => return stream,
                Err(_) => sleep(POLL).await,
            }
        }
    })
    .await
    .expect("the relay never listened")
}

/// A far side that breaks its first connection and echoes on its second.
///
/// `linger=0` is what makes the drop send an RST rather than a FIN, which is
/// the difference between the relay seeing an error and seeing end of stream.
/// Returns what the second connection received, so a case can prove the relay
/// came back rather than merely that it did not exit.
fn spawn_tcp_breaker(listener: TcpListener) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("far accept");

        let mut buf = [0u8; 4096];
        let _ = first.read(&mut buf).await;

        first.set_zero_linger().expect("far linger");
        drop(first);

        let (mut second, _) = listener.accept().await.expect("far accept");

        let mut seen = Vec::new();

        loop {
            let n = second.read(&mut buf).await.expect("far read");
            if n == 0 {
                break;
            }

            seen.extend_from_slice(&buf[..n]);
            second.write_all(&buf[..n]).await.expect("far write");
        }

        second.shutdown().await.expect("far half close");

        seen
    })
}

/// A broken connection reopens the pair; the second client is served by a
/// relay that would have exited without `reconnect`.
#[tokio::test]
async fn a_broken_connection_restarts_the_run() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("relay.sock");

        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let far = spawn_tcp_breaker(far);

        let (relay, _trigger) = start(
            &format!("unix-listen:{}", path.display()),
            &format!("tcp:{far_addr},reconnect=restart"),
        )
        .await;

        // The first client's connection dies with the run it belongs to: a
        // restart reopens both ends, so this one is dropped rather than
        // reattached to the new sink.
        let mut first = connect_unix(&path).await;
        first.write_all(b"lost").await.expect("client write");
        let _ = first.read(&mut [0u8; 16]).await;
        drop(first);

        let mut second = connect_unix(&path).await;
        second.write_all(PAYLOAD).await.expect("client write");
        second.shutdown().await.expect("client half close");

        let mut echoed = Vec::new();
        second
            .read_to_end(&mut echoed)
            .await
            .expect("client read back");

        assert_eq!(echoed, PAYLOAD, "the reopened pair altered the bytes");
        assert_eq!(
            far.await.expect("far task"),
            PAYLOAD,
            "the second connection did not carry the payload",
        );

        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

const PAYLOAD: &[u8] = b"after the restart";
