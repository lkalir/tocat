//! These drive [`Relay`] in process rather than shelling out to the binary.
//! The subject is `relay`, `pump` and the endpoint modules over real kernel
//! objects; a subprocess would add argument parsing and a second runtime
//! without covering anything the unit tests do not already reach.
//!
//! Every case is wrapped in a timeout: the failure mode of all of them is a
//! hang rather than a wrong answer.

use std::{future::Future, path::Path, time::Duration};

use tocat_core::{
    endpoint::EndpointSpec,
    relay::Relay,
    shutdown::{self, Trigger},
    spec::parse_plugin_spec,
};
#[cfg(feature = "block")]
use tokio::sync::mpsc;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::{TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};

/// An ephemeral port on loopback, for a listener the test owns.
const LOOPBACK_ANY: &str = "127.0.0.1:0";

/// Long enough to be uninteresting on a loaded runner, short enough that a
/// deadlock is reported as one rather than by the harness.
const CASE: Duration = Duration::from_secs(20);

/// How long a peer waits for the relay to reach its listen call.
const READY: Duration = Duration::from_secs(5);

/// Between attempts while waiting for that.
const POLL: Duration = Duration::from_millis(10);

const BUFFER: usize = 64 * 1024;

const PAYLOAD: &[u8] = b"the quick brown fox jumps over the lazy dog";

/// Three lengths, none of them equal and none of them zero: a zero length
/// seqpacket message is the peer's shutdown, not a message.
const MESSAGES: [&[u8]; 3] = [b"one", b"two-two", b"three-three-three"];

/// Bound every case, since each one fails by hanging.
async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

/// Build a relay out of the strings a user would type, and start it.
///
/// The [`Trigger`] comes back because it has to outlive the relay: a dropped
/// trigger closes the watch channel, which every `Shutdown` reads as a signal.
/// Bind it even where the case never drains.
async fn start(
    source: &str,
    sink: &str,
    plugins: &[&str],
) -> (JoinHandle<anyhow::Result<()>>, Trigger) {
    let source: EndpointSpec = source.parse().expect("source spec");
    let sink: EndpointSpec = sink.parse().expect("sink spec");

    let plugins = plugins
        .iter()
        .map(|raw| parse_plugin_spec(raw).expect("plugin spec"))
        .collect();

    let (trigger, shutdown) = shutdown::channel();

    let relay = Relay::new(
        source,
        sink,
        plugins,
        tocat_plugins::native_registry(),
        BUFFER,
        None,
    )
    .await
    .expect("relay construction");

    (tokio::spawn(relay.run(shutdown)), trigger)
}

/// A port nothing is listening on.
///
/// A listening endpoint is named before it is bound and the relay does not
/// report the address it chose, so the port has to be picked in advance. The
/// listener is dropped rather than handed over, which leaves a window where
/// something else could take the port; every other listener in this file binds
/// an ephemeral port and keeps it.
async fn reserve_port() -> u16 {
    let listener = TcpListener::bind(LOOPBACK_ANY)
        .await
        .expect("reserve a port");

    listener.local_addr().expect("reserved address").port()
}

/// The same, in the other port namespace. A reserved TCP port says nothing
/// about the UDP port of the same number.
async fn reserve_udp_port() -> u16 {
    let socket = UdpSocket::bind(LOOPBACK_ANY)
        .await
        .expect("reserve a udp port");

    socket.local_addr().expect("reserved address").port()
}

/// The relay binds inside its own task, so a connect can arrive first.
async fn connect_tcp(port: u16) -> TcpStream {
    timeout(READY, async {
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => return stream,
                Err(_) => sleep(POLL).await,
            }
        }
    })
    .await
    .expect("the relay never listened")
}

/// The same wait, for a stream socket that also has to appear on the
/// filesystem.
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

/// The same wait, for a socket that also has to appear on the filesystem.
async fn connect_seqpacket(path: &Path) -> UnixSeqpacket {
    timeout(READY, async {
        loop {
            match UnixSeqpacket::connect(path).await {
                Ok(socket) => return socket,
                Err(_) => sleep(POLL).await,
            }
        }
    })
    .await
    .expect("the relay never listened")
}

/// The far side of the relay: writes every chunk back and returns everything it
/// saw, so a case can assert on both directions.
///
/// Generic over the stream because the transport under it is what several of
/// these cases vary, and the far side behaves the same either way.
async fn echo<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S) -> Vec<u8> {
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        let n = stream.read(&mut buf).await.expect("far read");
        if n == 0 {
            break;
        }

        seen.extend_from_slice(&buf[..n]);
        stream.write_all(&buf[..n]).await.expect("far write");
    }

    // Without this the client's read to end never returns.
    stream.shutdown().await.expect("far half close");

    seen
}

fn spawn_tcp_echo(listener: TcpListener) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("far accept");
        echo(stream).await
    })
}

fn spawn_unix_echo(listener: UnixListener) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("far accept");
        echo(stream).await
    })
}

/// The far side of a forked relay: one echo per accepted connection, for as
/// long as the case runs. Nothing awaits this; it is dropped with the runtime.
fn spawn_tcp_echo_each(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("far accept");

            tokio::spawn(async move {
                echo(stream).await;
            });
        }
    })
}

/// The connectionless far side. Every datagram goes back where it came from,
/// which for a relay's sink is the relay rather than the original client.
fn spawn_udp_echo(socket: UdpSocket) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];

        loop {
            let (n, peer) = socket.recv_from(&mut buf).await.expect("far recv");
            socket.send_to(&buf[..n], peer).await.expect("far send");
        }
    })
}

/// A far side that reports each chunk as it lands, for a case that needs to
/// assert something has *not* arrived yet.
#[cfg(feature = "block")]
fn spawn_tcp_collector(listener: TcpListener) -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("far accept");
        let mut buf = [0u8; 4096];

        loop {
            let n = stream.read(&mut buf).await.expect("far read");
            if n == 0 {
                break;
            }

            let _ = tx.send(buf[..n].to_vec());
        }
    });

    rx
}

/// The message oriented echo. One receive is one message, and it stays one on
/// the way back.
fn spawn_seqpacket_echo(mut listener: UnixSeqpacketListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let peer = listener.accept().await.expect("far accept");
        let mut buf = [0u8; 4096];

        loop {
            let n = peer.recv(&mut buf).await.expect("far recv");
            if n.bytes_read() == 0 {
                break;
            }

            peer.send(&buf[..n.bytes_read()]).await.expect("far send");
        }
    })
}

#[tokio::test]
async fn tcp_round_trip() {
    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_tcp_echo(far);

        let port = reserve_port().await;
        let (relay, _trigger) = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("tcp:{far_addr}"),
            &[],
        )
        .await;

        let mut client = connect_tcp(port).await;
        client.write_all(PAYLOAD).await.expect("client write");

        // Half close rather than drop: the reverse path has to stay open long
        // enough to carry the echo back.
        client.shutdown().await.expect("client half close");

        let mut echoed = Vec::new();
        client
            .read_to_end(&mut echoed)
            .await
            .expect("client read back");

        let seen = echo.await.expect("echo task");

        assert_eq!(seen, PAYLOAD, "the forward path altered the bytes");
        assert_eq!(echoed, PAYLOAD, "the reverse path altered the bytes");

        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// One stage, on the forward path only, which is what a declaration with no
/// direction means.
///
/// `block` with `pad` is the stage whose work is visible in the bytes: 43 bytes
/// in blocks of 8 leaves a short block at end of stream, which goes out padded.
/// The reverse path has no stages, so what comes back is what the far side
/// received, and a mirrored declaration would show up here as double padding.
#[cfg(feature = "block")]
#[tokio::test]
async fn tcp_round_trip_through_a_stage() {
    const BLOCK: usize = 8;

    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_tcp_echo(far);

        let port = reserve_port().await;
        let (relay, _trigger) = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("tcp:{far_addr}"),
            &[&format!("block,size={BLOCK},pad")],
        )
        .await;

        let mut client = connect_tcp(port).await;
        client.write_all(PAYLOAD).await.expect("client write");
        client.shutdown().await.expect("client half close");

        let mut echoed = Vec::new();
        client
            .read_to_end(&mut echoed)
            .await
            .expect("client read back");

        let mut expected = PAYLOAD.to_vec();
        expected.resize(PAYLOAD.len().div_ceil(BLOCK) * BLOCK, 0);

        let seen = echo.await.expect("echo task");

        assert_eq!(seen, expected, "the stage did not run on the forward path");
        assert_eq!(echoed, expected, "the reverse path altered the bytes");

        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// Three messages out, three back, each with the bounds it was sent with.
///
/// Sent together rather than in lockstep on purpose: a relay that coalesced
/// them would hand back one receive of everything, which a send-then-receive
/// loop would never notice.
#[tokio::test]
async fn seqpacket_preserves_message_boundaries() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let far_path = dir.path().join("far.sock");
        let relay_path = dir.path().join("relay.sock");

        let far = UnixSeqpacketListener::bind(&far_path).expect("far listener");
        let echo = spawn_seqpacket_echo(far);

        let (relay, _trigger) = start(
            &format!("unix-seqpacket-listen:{}", relay_path.display()),
            &format!("unix-seqpacket:{}", far_path.display()),
            &[],
        )
        .await;

        let client = connect_seqpacket(&relay_path).await;

        for message in MESSAGES {
            client.send(message).await.expect("client send");
        }

        for message in MESSAGES {
            let mut buf = [0u8; 4096];
            let n = client.recv(&mut buf).await.expect("client recv");

            assert_eq!(
                &buf[..n.bytes_read()],
                message,
                "a message came back with different bounds",
            );
        }

        // The peer's shutdown is what ends both sides.
        drop(client);

        echo.await.expect("echo task");
        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// A drain is end of stream, so a stage holding bytes has to hand them over.
///
/// The block is far larger than the payload and no flush interval is set, so
/// the stage has no reason of its own to let go: anything that reaches the far
/// side got there through `on_eof`. The case checks that nothing arrives before
/// the drain as well, since a stage that forwarded early would make the second
/// assertion pass for the wrong reason.
#[cfg(feature = "block")]
#[tokio::test]
async fn shutdown_flushes_buffered_bytes() {
    /// Long enough that an eager stage would have been seen.
    const HELD: Duration = Duration::from_millis(200);

    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let mut arrived = spawn_tcp_collector(far);

        let port = reserve_port().await;
        let (relay, trigger) = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("tcp:{far_addr}"),
            &["block,size=65536"],
        )
        .await;

        let mut client = connect_tcp(port).await;
        client.write_all(PAYLOAD).await.expect("client write");

        // Deliberately no half close: the drain has to be what ends the path.
        assert!(
            timeout(HELD, arrived.recv()).await.is_err(),
            "the stage forwarded a short block on its own, so this case proves nothing",
        );

        trigger.drain();

        let flushed = timeout(READY, arrived.recv())
            .await
            .expect("the drain flushed nothing")
            .expect("the far side went away");

        assert_eq!(
            flushed, PAYLOAD,
            "the bytes the stage was holding did not survive the drain",
        );

        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// The second stream transport, which the endpoint restructure ports the same
/// way as the first and which nothing else here would catch.
#[tokio::test]
async fn unix_round_trip() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let far_path = dir.path().join("far.sock");
        let relay_path = dir.path().join("relay.sock");

        let far = UnixListener::bind(&far_path).expect("far listener");
        let echo = spawn_unix_echo(far);

        let (relay, _trigger) = start(
            &format!("unix-listen:{}", relay_path.display()),
            &format!("unix:{}", far_path.display()),
            &[],
        )
        .await;

        let mut client = connect_unix(&relay_path).await;
        client.write_all(PAYLOAD).await.expect("client write");
        client.shutdown().await.expect("client half close");

        let mut echoed = Vec::new();
        client
            .read_to_end(&mut echoed)
            .await
            .expect("client read back");

        let seen = echo.await.expect("echo task");

        assert_eq!(seen, PAYLOAD, "the forward path altered the bytes");
        assert_eq!(echoed, PAYLOAD, "the reverse path altered the bytes");

        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// `fork`, which is `serve` rather than `run_once`: a listener, an accept loop,
/// a semaphore, and a fresh chain per connection.
///
/// The connections are sequential on purpose. What regresses here is an accept
/// loop that serves the first peer and then stops, or a per-connection chain
/// that is built once and shared; both show up without the timing sensitivity
/// of racing two clients.
#[tokio::test]
async fn fork_serves_connections_in_turn() {
    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let _far = spawn_tcp_echo_each(far);

        let port = reserve_port().await;
        let (relay, trigger) = start(
            &format!("tcp-listen:127.0.0.1:{port},fork"),
            &format!("tcp:{far_addr}"),
            &[],
        )
        .await;

        for payload in [&b"first connection"[..], b"second connection"] {
            let mut client = connect_tcp(port).await;
            client.write_all(payload).await.expect("client write");
            client.shutdown().await.expect("client half close");

            let mut echoed = Vec::new();
            client
                .read_to_end(&mut echoed)
                .await
                .expect("client read back");

            assert_eq!(echoed, payload, "a forked connection altered the bytes");
        }

        // A listener has no end of stream of its own: the drain is what stops
        // it accepting.
        trigger.drain();
        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// The connectionless datagram path, where the peer is learned by peeking at
/// the first message rather than by accepting.
#[tokio::test]
async fn udp_round_trip() {
    /// How long to wait for a reply before sending again.
    const RETRY: Duration = Duration::from_millis(100);

    run(async {
        let far = UdpSocket::bind(LOOPBACK_ANY).await.expect("far socket");
        let far_addr = far.local_addr().expect("far address");
        let _far = spawn_udp_echo(far);

        let port = reserve_udp_port().await;
        let (relay, trigger) = start(
            &format!("udp-listen:127.0.0.1:{port}"),
            &format!("udp:{far_addr}"),
            &[],
        )
        .await;

        let client = UdpSocket::bind(LOOPBACK_ANY).await.expect("client socket");
        let relay_addr = format!("127.0.0.1:{port}");
        let mut buf = [0u8; 4096];

        // A datagram sent before the relay has bound is dropped with no error
        // anywhere, so this resends rather than waiting for a connect to
        // succeed the way the stream cases do.
        let n = timeout(READY, async {
            loop {
                client
                    .send_to(PAYLOAD, &relay_addr)
                    .await
                    .expect("client send");

                if let Ok(received) = timeout(RETRY, client.recv_from(&mut buf)).await {
                    return received.expect("client recv").0;
                }
            }
        })
        .await
        .expect("no datagram came back");

        assert_eq!(&buf[..n], PAYLOAD, "the datagram changed on the way round");

        trigger.drain();
        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// A child process as the sink, which is the one endpoint whose far side is a
/// pipe pair rather than a socket. `cat` ends when its stdin does, so this also
/// covers end of stream reaching a child.
#[tokio::test]
async fn exec_child_round_trip() {
    run(async {
        let port = reserve_port().await;
        let (relay, _trigger) =
            start(&format!("tcp-listen:127.0.0.1:{port}"), "exec:cat", &[]).await;

        let mut client = connect_tcp(port).await;
        client.write_all(PAYLOAD).await.expect("client write");
        client.shutdown().await.expect("client half close");

        let mut echoed = Vec::new();
        client
            .read_to_end(&mut echoed)
            .await
            .expect("client read back");

        assert_eq!(echoed, PAYLOAD, "the child did not see the payload");

        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// Two blocking-backed endpoints and nothing declared, which is the one
/// combination that never reaches `pump`: `dispatch` sends it to `run_sync` and
/// the blocking pool instead.
///
/// One direction only. A `file:` source has nothing to be written back to, so
/// the reverse direction does not exist and must not be run at all.
#[tokio::test]
async fn file_to_file_takes_the_sync_path() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("in");
        let output = dir.path().join("out");

        std::fs::write(&input, PAYLOAD).expect("write the input file");

        let (relay, _trigger) = start(
            &format!("file:{}", input.display()),
            &format!("file:{}", output.display()),
            &[],
        )
        .await;

        relay.await.expect("relay task").expect("relay run");

        let copied = std::fs::read(&output).expect("read the output file");
        assert_eq!(copied, PAYLOAD, "the synchronous copy altered the bytes");
    })
    .await;
}
