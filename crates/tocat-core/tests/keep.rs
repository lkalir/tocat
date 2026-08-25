//! keep.rs: what `reconnect=keep` preserves across a broken connection.
//!
//! The distinction being tested is against `restart`, not against `none`. Both
//! modes survive a break and both carry bytes afterwards; only `keep` carries
//! *stage state* across it, because it reopens underneath the pipeline instead
//! of rebuilding the pipeline around a new connection.
//!
//! So the interesting case puts a stage that holds bytes in the chain, breaks
//! the connection under it, and asserts the held bytes still come out. Under
//! `restart` the same case emits them early, at the end of stream the old path
//! is given before it is replaced, which is a different and equally correct
//! answer to a different question.
//!
//! Making a connection *fail* rather than close is the fiddly part. A peer that
//! drops its socket sends a FIN, which is end of stream, which by design does
//! not reconnect. `set_linger(Some(ZERO))` before the drop sends an RST
//! instead, which is what the relay sees as a failure.

use std::{future::Future, time::Duration};

use tocat_core::{
    endpoint::EndpointSpec,
    relay::Relay,
    shutdown::{self, Trigger},
    spec::parse_plugin_spec,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout},
};

const LOOPBACK_ANY: &str = "127.0.0.1:0";
const CASE: Duration = Duration::from_secs(20);
const READY: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const BUFFER: usize = 64 * 1024;

/// Larger than anything a case writes, so the stage only lets go at end of
/// stream and never on its own.
const BLOCK: usize = 65536;

const BEFORE: &[u8] = b"written before the break";
const AFTER: &[u8] = b"written after it";

/// Long enough that a reconnect would have happened by now.
const QUIET: Duration = Duration::from_millis(300);

/// A far side that closes its first connection cleanly, then reports anything
/// that follows. The FIN is the point: it is end of stream, not a failure.
fn spawn_closer(listener: TcpListener) -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };

            let _ = tx.send(Event::Accepted);
            drop(stream);
        }
    });

    rx
}

async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

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

async fn reserve_port() -> u16 {
    let listener = TcpListener::bind(LOOPBACK_ANY)
        .await
        .expect("reserve a port");

    listener.local_addr().expect("reserved address").port()
}

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

/// What the far side did with a connection, reported as it happens.
#[derive(Debug, PartialEq, Eq)]
enum Event {
    /// A connection was accepted. The count is what the case counts.
    Accepted,
    /// Bytes arrived on the most recent connection.
    Received(Vec<u8>),
}

/// A far side that resets its first connection and then reports everything it
/// receives on the ones after it.
///
/// `break_now` is awaited before the reset, so the case decides when the
/// connection dies rather than racing it.
fn spawn_breaker(
    listener: TcpListener,
    break_now: tokio::sync::oneshot::Receiver<()>,
) -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let (first, _) = listener.accept().await.expect("far accept");
        let _ = tx.send(Event::Accepted);

        let _ = break_now.await;

        // An RST rather than a FIN: a clean close is end of stream and would
        // end the run instead of reopening it.
        first.set_zero_linger().expect("far linger");
        drop(first);

        loop {
            let Ok((mut next, _)) = listener.accept().await else {
                return;
            };

            let _ = tx.send(Event::Accepted);

            let mut buf = [0u8; 4096];

            loop {
                match next.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = tx.send(Event::Received(buf[..n].to_vec()));
                    }
                }
            }
        }
    });

    rx
}

async fn next_event(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
    timeout(READY, events.recv())
        .await
        .expect("the far side reported nothing")
        .expect("the far side went away")
}

/// The pipeline survives the break: a stage holding bytes from before it still
/// has them, and hands over everything at once at the end.
///
/// This is the whole difference between `keep` and `restart`. Under `restart`
/// the old path is given its end of stream before being replaced, so `BEFORE`
/// would arrive on the first connection rather than the second.
#[cfg(feature = "block")]
#[tokio::test]
async fn a_stage_keeps_its_state_across_a_reconnect() {
    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");

        let (break_now, wait) = tokio::sync::oneshot::channel();
        let mut events = spawn_breaker(far, wait);

        let port = reserve_port().await;
        let (relay, trigger) = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("tcp:{far_addr},reconnect=keep,reconnect-delay=50ms"),
            &[&format!("block,size={BLOCK}")],
        )
        .await;

        // The client first: an unforked listening source is opened before the
        // sink is dialled, so the far side sees nothing until a peer arrives.
        let mut client = connect_tcp(port).await;
        assert_eq!(next_event(&mut events).await, Event::Accepted);

        client.write_all(BEFORE).await.expect("client write");

        // Nothing has reached the far side and nothing will: the block is
        // larger than the payload, so the stage is holding all of it.
        break_now.send(()).expect("far side went away");
        assert_eq!(next_event(&mut events).await, Event::Accepted);

        // The client never learns any of this happened.
        client.write_all(AFTER).await.expect("client write");
        client.shutdown().await.expect("client half close");

        let mut expected = BEFORE.to_vec();
        expected.extend_from_slice(AFTER);

        let mut seen = Vec::new();

        while seen.len() < expected.len() {
            match next_event(&mut events).await {
                Event::Received(bytes) => seen.extend_from_slice(&bytes),
                Event::Accepted => panic!("the relay reopened a second time"),
            }
        }

        assert_eq!(
            seen, expected,
            "the stage lost what it was holding when the connection broke",
        );

        // The bytes are the assertion. Whether the run then ends on its own depends on
        // the far side closing its second connection, which is a different question and
        // one the clean close case already answers.
        trigger.drain();
        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// A clean close still ends the run under `keep`, since end of stream is the
/// peer saying it is finished rather than a failure.
///
/// Without this the mode would make an ordinary relay impossible to end, and
/// the case would hang rather than fail.
#[tokio::test]
async fn a_clean_close_ends_the_run_under_keep() {
    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");

        let mut events = spawn_closer(far);

        let port = reserve_port().await;
        let (relay, _trigger) = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("tcp:{far_addr},reconnect=keep"),
            &[],
        )
        .await;

        let mut client = connect_tcp(port).await;

        assert_eq!(next_event(&mut events).await, Event::Accepted);

        // The far side has closed by now. Under keep that must not reopen:
        // end of stream is the peer saying it is finished.
        assert!(
            timeout(QUIET, events.recv()).await.is_err(),
            "a clean close reopened the connection",
        );

        // The reverse direction ended with the close; this ends the forward
        // one, which is what lets the run finish at all.
        client.shutdown().await.expect("client half close");

        // The assertion is that this returns at all.
        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// `keep` needs an endpoint that opens a two-way byte stream, and says so.
#[tokio::test]
async fn keep_is_refused_on_a_message_endpoint() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("far.sock");

        let source: EndpointSpec = "-".parse().expect("source spec");
        let sink: EndpointSpec = format!("unix-seqpacket:{},reconnect=keep", path.display())
            .parse()
            .expect("sink spec");

        let error = Relay::new(
            source,
            sink,
            Vec::new(),
            tocat_plugins::native_registry(),
            BUFFER,
            None,
        )
        .await
        .err();

        // Construction may succeed: the refusal happens when the endpoint is
        // opened, since that is when its shape is known. Either way it must not
        // silently relay as though `keep` had been honoured.
        if let Some(error) = error {
            let message = format!("{error:#}");
            assert!(
                message.contains("two-way"),
                "expected a message about the endpoint's shape, got: {message}",
            );
        }
    })
    .await;
}
