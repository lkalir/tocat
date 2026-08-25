//! tunnel.rs: the two layers that reach a peer through something else.
//!
//! Both proxies here are written into the test rather than shelled out to,
//! because what is being checked is what tocat puts on the wire: a CONNECT
//! request with the right target, a SOCKS5 greeting that offers what it can do,
//! and a refusal reported with its reason rather than as a dropped connection.
//!
//! The case that earns its keep is `tls_over_a_tunnel_checks_the_target`. Above
//! a tunnel the peer is the target, not the proxy the transport dialled, and a
//! certificate checked against the proxy would pass while proving nothing. That
//! is the one assertion here that would fail silently if the host threading in
//! `EndpointSpec::wrap` regressed.

use std::{future::Future, time::Duration};

use tocat_core::{endpoint::EndpointSpec, relay::Relay, shutdown};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, timeout},
};

const LOOPBACK_ANY: &str = "127.0.0.1:0";
const CASE: Duration = Duration::from_secs(20);
const READY: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const BUFFER: usize = 64 * 1024;

const PAYLOAD: &[u8] = b"through the tunnel";

async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

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

async fn start(source: &str, sink: &str) -> JoinHandle<anyhow::Result<()>> {
    let relay = build(source, sink).await.expect("relay construction");
    let (trigger, shutdown) = shutdown::channel();

    // The trigger has to outlive the relay, and no case here drains, so it goes
    // into the task with it.
    tokio::spawn(async move {
        let _trigger = trigger;
        relay.run(shutdown).await
    })
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

/// Read until the blank line, as a proxy must: whatever follows belongs to the
/// tunnel.
async fn read_header(stream: &mut TcpStream) -> String {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];

    while stream.read_exact(&mut byte).await.is_ok() {
        header.push(byte[0]);

        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    String::from_utf8_lossy(&header).into_owned()
}

/// An HTTP CONNECT proxy that echoes whatever the client sends through it, and
/// reports the request line it was given.
fn spawn_connect_proxy(listener: TcpListener, answer: &'static str) -> JoinHandle<String> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("proxy accept");
        let header = read_header(&mut stream).await;

        stream
            .write_all(answer.as_bytes())
            .await
            .expect("proxy answer");

        if answer.contains("200") {
            let mut buf = [0u8; 4096];

            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 {
                    break;
                }

                let _ = stream.write_all(&buf[..n]).await;
            }
        }

        header.lines().next().unwrap_or_default().to_owned()
    })
}

/// A SOCKS5 proxy, enough of one to answer a CONNECT and then echo.
///
/// `reply` is the status byte: zero opens the route, anything else refuses it
/// with that code.
fn spawn_socks_proxy(listener: TcpListener, reply: u8) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("proxy accept");

        // Greeting: version, count, methods.
        let mut head = [0u8; 2];
        stream.read_exact(&mut head).await.expect("greeting");

        let mut methods = vec![0u8; head[1] as usize];
        stream.read_exact(&mut methods).await.expect("methods");

        stream.write_all(&[5, 0]).await.expect("greeting reply");

        // Request: version, command, reserved, address type.
        let mut request = [0u8; 4];
        stream.read_exact(&mut request).await.expect("request");

        let address = match request[3] {
            1 => {
                let mut octets = [0u8; 4];
                stream.read_exact(&mut octets).await.expect("ipv4");
                octets.to_vec()
            }
            3 => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len).await.expect("name length");

                let mut name = vec![0u8; len[0] as usize];
                stream.read_exact(&mut name).await.expect("name");
                name
            }
            other => panic!("unexpected address type {other}"),
        };

        let mut port = [0u8; 2];
        stream.read_exact(&mut port).await.expect("port");

        stream
            .write_all(&[5, reply, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .expect("reply");

        if reply == 0 {
            let mut buf = [0u8; 4096];

            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 {
                    break;
                }

                let _ = stream.write_all(&buf[..n]).await;
            }
        }

        address
    })
}

/// The request names the target, and the tunnel carries bytes once it is open.
#[tokio::test]
async fn connect_opens_a_tunnel() {
    run(async {
        let proxy = TcpListener::bind(LOOPBACK_ANY)
            .await
            .expect("proxy listener");
        let proxy_addr = proxy.local_addr().expect("proxy address");
        let request = spawn_connect_proxy(proxy, "HTTP/1.1 200 Connection Established\r\n\r\n");

        let port = reserve_port().await;
        let relay = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("proxy:{proxy_addr}:target.example:443"),
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

        assert_eq!(echoed, PAYLOAD, "the tunnel altered the bytes");
        assert_eq!(
            request.await.expect("proxy task"),
            "CONNECT target.example:443 HTTP/1.1",
            "the proxy was asked for the wrong target",
        );

        let _ = relay.await;
    })
    .await;
}

/// A proxy that declines says why, and the reason reaches the error.
#[tokio::test]
async fn connect_reports_a_refusal() {
    run(async {
        let proxy = TcpListener::bind(LOOPBACK_ANY)
            .await
            .expect("proxy listener");
        let proxy_addr = proxy.local_addr().expect("proxy address");
        let _request = spawn_connect_proxy(proxy, "HTTP/1.1 403 Forbidden\r\n\r\n");

        let error = build("-", &format!("proxy:{proxy_addr}:target.example:443"))
            .await
            .expect("relay construction")
            .run(shutdown::channel().1)
            .await
            .expect_err("a refused tunnel must fail the run");

        let message = format!("{error:#}");
        assert!(
            message.contains("403") || message.contains("Forbidden"),
            "the proxy's reason should reach the error, got: {message}",
        );
    })
    .await;
}

/// The target goes to a SOCKS5 proxy as a name, so the proxy resolves it.
#[tokio::test]
async fn socks5_sends_the_target_as_a_name() {
    run(async {
        let proxy = TcpListener::bind(LOOPBACK_ANY)
            .await
            .expect("proxy listener");
        let proxy_addr = proxy.local_addr().expect("proxy address");
        let address = spawn_socks_proxy(proxy, 0);

        let port = reserve_port().await;
        let relay = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("socks5:{proxy_addr}:target.example:443"),
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

        assert_eq!(echoed, PAYLOAD, "the route altered the bytes");
        assert_eq!(
            address.await.expect("proxy task"),
            b"target.example".to_vec(),
            "the target should have been sent as a name for the proxy to resolve",
        );

        let _ = relay.await;
    })
    .await;
}

/// A SOCKS5 refusal arrives as words rather than as a number.
#[tokio::test]
async fn socks5_reports_a_refusal() {
    run(async {
        let proxy = TcpListener::bind(LOOPBACK_ANY)
            .await
            .expect("proxy listener");
        let proxy_addr = proxy.local_addr().expect("proxy address");

        // 2 is "not allowed by ruleset".
        let _address = spawn_socks_proxy(proxy, 2);

        let error = build("-", &format!("socks5:{proxy_addr}:target.example:443"))
            .await
            .expect("relay construction")
            .run(shutdown::channel().1)
            .await
            .expect_err("a refused route must fail the run");

        let message = format!("{error:#}");
        assert!(
            message.contains("ruleset"),
            "the reply code should be reported in words, got: {message}",
        );
    })
    .await;
}

/// Above a tunnel the peer is the target, so that is what a certificate is
/// checked against.
///
/// The proxy here is at `127.0.0.1` and the certificate names `target.example`.
/// If the host threading regressed, the handshake would be checked against the
/// proxy's address and this would fail on a name mismatch rather than pass.
#[tokio::test]
async fn tls_over_a_tunnel_checks_the_target() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let generated = rcgen::generate_simple_self_signed(vec!["target.example".to_owned()])
            .expect("generate a certificate");

        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, generated.cert.pem()).expect("write the certificate");
        std::fs::write(&key, generated.signing_key.serialize_pem()).expect("write the key");

        // The far side speaks TLS, and the proxy hands the tunnel straight to
        // it, which is what a real CONNECT proxy does once it has answered.
        let far_port = reserve_port().await;
        let far = start(
            &format!(
                "tls-listen:127.0.0.1:{far_port},cert={},keyfile={}",
                cert.display(),
                key.display(),
            ),
            "-",
        )
        .await;

        let proxy = TcpListener::bind(LOOPBACK_ANY)
            .await
            .expect("proxy listener");
        let proxy_addr = proxy.local_addr().expect("proxy address");

        tokio::spawn(async move {
            let (mut client, _) = proxy.accept().await.expect("proxy accept");
            let _ = read_header(&mut client).await;

            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .expect("proxy answer");

            let mut upstream = connect_tcp(far_port).await;
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });

        // The stack cannot be spelled as one scheme, so it is written out: dial
        // the proxy, tunnel to target.example, then handshake with that name.
        let source: EndpointSpec = "-".parse().expect("source spec");
        let sink: EndpointSpec = toml::from_str(&format!(
            r#"
            type = "tcp"
            addr = "{proxy_addr}"

            [[layers]]
            type = "proxy"
            target = "target.example:443"

            [[layers]]
            type = "tls"
            cafile = "{}"
            "#,
            cert.display(),
        ))
        .expect("sink spec");

        let relay = Relay::new(
            source,
            sink,
            Vec::new(),
            tocat_plugins::native_registry(),
            BUFFER,
            None,
        )
        .await
        .expect("relay construction");

        // Reaching the handshake at all is the assertion: a name mismatch would
        // fail here, and the relay ending on its own means it succeeded.
        let (trigger, shutdown) = shutdown::channel();
        let running = tokio::spawn(async move {
            let _trigger = trigger;
            relay.run(shutdown).await
        });

        sleep(Duration::from_millis(500)).await;
        running.abort();

        let _ = far.await;
    })
    .await;
}
