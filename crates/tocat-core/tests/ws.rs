//! ws.rs: WebSocket as a layer.
//!
//! The property under test is the one that makes this layer different from
//! TLS: it declares `Preserve`, so a message sent is a message received, and a
//! relay with `ws:` on one end is a datagram endpoint. Every case here drives
//! the relay through a `chan:` queue at the far end, because a queue is a
//! message endpoint and will therefore show a boundary that the layer lost.
//!
//! No hand written WebSocket client: a second relay dials the first, which
//! exercises `ws:` and `ws-listen:` against each other and keeps the test in
//! the vocabulary the tool actually has.

use std::{future::Future, path::Path, time::Duration};

use tocat_core::{
    endpoint::{self, DEFAULT_CAPACITY, EndpointSpec},
    relay::Relay,
    shutdown::{self, Trigger},
};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};

const CASE: Duration = Duration::from_secs(20);
const READY: Duration = Duration::from_secs(5);
const BUFFER: usize = 64 * 1024;

/// Three lengths, none equal, so a relay that joined them would be caught by
/// the first assertion rather than the last.
const MESSAGES: [&[u8]; 3] = [b"one", b"two-two", b"three-three-three"];

async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

async fn build(source: &str, sink: &str, buffer: usize) -> anyhow::Result<Relay> {
    let source: EndpointSpec = source.parse().expect("source spec");
    let sink: EndpointSpec = sink.parse().expect("sink spec");

    Relay::new(
        source,
        sink,
        Vec::new(),
        tocat_plugins::native_registry(),
        buffer,
        None,
    )
    .await
}

async fn start(source: &str, sink: &str) -> (JoinHandle<anyhow::Result<()>>, Trigger) {
    start_with(source, sink, BUFFER).await
}

async fn start_with(
    source: &str,
    sink: &str,
    buffer: usize,
) -> (JoinHandle<anyhow::Result<()>>, Trigger) {
    let relay = build(source, sink, buffer)
        .await
        .expect("relay construction");
    let (trigger, shutdown) = shutdown::channel();

    (tokio::spawn(relay.run(shutdown)), trigger)
}

async fn reserve_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve a port");

    listener.local_addr().expect("reserved address").port()
}

/// A self signed pair for the `wss` case.
fn certificate(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("generate a certificate");

    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");

    std::fs::write(&cert, generated.cert.pem()).expect("write the certificate");
    std::fs::write(&key, generated.signing_key.serialize_pem()).expect("write the key");

    (cert, key)
}

/// Messages keep their bounds across the layer, in both directions.
#[tokio::test]
async fn ws_preserves_message_boundaries() {
    run(async {
        let port = reserve_port().await;

        let mut inbox = endpoint::register("ws-out", DEFAULT_CAPACITY);
        let server = start(&format!("ws-listen:127.0.0.1:{port}"), "chan:ws-out").await;

        let outbox = endpoint::register("ws-in", DEFAULT_CAPACITY);
        let client = start("chan:ws-in", &format!("ws:127.0.0.1:{port}")).await;

        for message in MESSAGES {
            outbox.tx.send(message.to_vec()).await.expect("send");
        }

        for message in MESSAGES {
            let received = timeout(READY, inbox.rx.recv())
                .await
                .expect("nothing arrived")
                .expect("the relay went away");

            assert_eq!(received, message, "a message crossed with different bounds");
        }

        client.1.drain();
        server.1.drain();
    })
    .await;
}

/// The two layer stack: TLS underneath, WebSocket over it.
#[tokio::test]
async fn wss_composes_tls_and_websocket() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let (cert, key) = certificate(dir.path());
        let port = reserve_port().await;

        let mut inbox = endpoint::register("wss-out", DEFAULT_CAPACITY);
        let server = start(
            &format!(
                "wss-listen:127.0.0.1:{port},cert={},keyfile={}",
                cert.display(),
                key.display(),
            ),
            "chan:wss-out",
        )
        .await;

        let outbox = endpoint::register("wss-in", DEFAULT_CAPACITY);
        let client = start(
            "chan:wss-in",
            &format!("wss:localhost:{port},cafile={}", cert.display()),
        )
        .await;

        outbox.tx.send(MESSAGES[0].to_vec()).await.expect("send");

        let received = timeout(READY, inbox.rx.recv())
            .await
            .expect("nothing arrived")
            .expect("the relay went away");

        assert_eq!(received, MESSAGES[0]);

        client.1.drain();
        server.1.drain();
    })
    .await;
}

/// A listener serves one path and answers anything else with a refusal rather
/// than upgrading it.
#[tokio::test]
async fn a_listener_refuses_another_path() {
    run(async {
        let port = reserve_port().await;

        let _inbox = endpoint::register("path-out", DEFAULT_CAPACITY);
        let (server, server_trigger) = start(
            &format!("ws-listen:127.0.0.1:{port}/socket"),
            "chan:path-out",
        )
        .await;

        let _outbox = endpoint::register("path-in", DEFAULT_CAPACITY);
        let (client, _client_trigger) =
            start("chan:path-in", &format!("ws:127.0.0.1:{port}/elsewhere")).await;

        client
            .await
            .expect("client relay")
            .expect_err("the upgrade must be refused for another path");

        server_trigger.drain();
        let _ = server.await;
    })
    .await;
}

/// A message larger than the copy buffer is an error, not a truncation.
///
/// The other message endpoints truncate, because a datagram sender does not
/// know what the receiver will take. A WebSocket peer believes it sent a whole
/// message, so half of one is worse than a failure.
#[tokio::test]
async fn an_oversized_message_is_an_error() {
    run(async {
        let port = reserve_port().await;
        let small = 64;

        let _inbox = endpoint::register("big-out", DEFAULT_CAPACITY);
        let (server, server_trigger) = start_with(
            &format!("ws-listen:127.0.0.1:{port}"),
            "chan:big-out",
            small,
        )
        .await;

        let outbox = endpoint::register("big-in", DEFAULT_CAPACITY);
        let (client, _client_trigger) =
            start_with("chan:big-in", &format!("ws:127.0.0.1:{port}"), BUFFER).await;

        outbox.tx.send(vec![b'x'; small * 4]).await.expect("send");

        let error = server
            .await
            .expect("server relay")
            .expect_err("an oversized message must fail the run");

        let message = format!("{error:#}");
        assert!(
            message.contains("copy buffer") || message.contains("-b"),
            "the error should name the buffer, got: {message}",
        );

        server_trigger.drain();
        let _ = client.await;
    })
    .await;
}
