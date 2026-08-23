//! tls.rs: the TLS layer over a real handshake.
//!
//! Three things are worth proving and one of them is the reason this file
//! exists. A round trip shows the layer carries bytes. A pin shows the
//! alternative to turning verification off actually works. And a failed
//! verification has to end the run, because the failure mode that matters for
//! a TLS layer is not "it broke", it is "it quietly did not encrypt".
//!
//! The certificate is generated in the test rather than checked in: a fixture
//! with an expiry date is a test that fails on a Tuesday two years from now.

use std::{future::Future, path::Path, sync::Arc, time::Duration};

use sha2::{Digest as _, Sha256};
use tocat::{
    config::parse_plugin_spec,
    endpoint::EndpointSpec,
    relay::Relay,
    shutdown::{self, Trigger},
};
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

const PAYLOAD: &[u8] = b"through the handshake and back";

/// The name on the generated certificate, and the name the client asks for.
/// Not `127.0.0.1`: a name in the SAN list is what a certificate is checked
/// against, and using the literal address would test a different code path.
const NAME: &str = "localhost";

async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

/// A self signed pair, written where the relay can read it.
struct Pair {
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    /// The leaf's DER, for computing a pin.
    der: Vec<u8>,
}

fn generate(dir: &Path) -> Pair {
    let generated =
        rcgen::generate_simple_self_signed(vec![NAME.to_owned()]).expect("generate a certificate");

    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    std::fs::write(&cert_path, generated.cert.pem()).expect("write the certificate");
    std::fs::write(&key_path, generated.signing_key.serialize_pem()).expect("write the key");

    Pair {
        cert_path,
        key_path,
        der: generated.cert.der().to_vec(),
    }
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

/// The plain far side: echoes and returns what it saw, so a case can assert on
/// what actually crossed the wire.
fn spawn_echo(listener: TcpListener) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return Vec::new();
        };

        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];

        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    let _ = stream.write_all(&buf[..n]).await;
                }
            }
        }

        let _ = stream.shutdown().await;

        seen
    })
}

/// A TLS client, for the cases where the relay is the server.
async fn tls_client(port: u16, pair: &Pair) -> tokio_rustls::client::TlsStream<TcpStream> {
    use rustls::pki_types::{CertificateDer, ServerName};

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(pair.der.clone()))
        .expect("trust the generated certificate");

    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();

    let name = ServerName::try_from(NAME.to_owned()).expect("server name");

    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, connect_tcp(port).await)
        .await
        .expect("client handshake")
}

/// `tls-listen:` accepts a handshake and hands the plaintext on.
#[tokio::test]
async fn tls_listen_round_trip() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let pair = generate(dir.path());

        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_echo(far);

        let port = reserve_port().await;
        let (relay, _trigger) = start(
            &format!(
                "tls-listen:127.0.0.1:{port},cert={},keyfile={}",
                pair.cert_path.display(),
                pair.key_path.display(),
            ),
            &format!("tcp:{far_addr}"),
            &[],
        )
        .await;

        let mut client = tls_client(port, &pair).await;
        client.write_all(PAYLOAD).await.expect("client write");
        client.shutdown().await.expect("client half close");

        let mut echoed = Vec::new();
        client
            .read_to_end(&mut echoed)
            .await
            .expect("client read back");

        assert_eq!(echoed, PAYLOAD, "the reverse path altered the bytes");
        assert_eq!(
            echo.await.expect("echo task"),
            PAYLOAD,
            "the far side did not receive the plaintext",
        );

        relay.await.expect("relay task").expect("relay run");
    })
    .await;
}

/// `pin=` accepts the certificate it was given the fingerprint of, without any
/// trust anchor being involved.
///
/// This is the case that makes `verify=none` unnecessary for the appliance with
/// a self signed certificate, which is most of the reason people reach for it.
#[tokio::test]
async fn a_pinned_certificate_is_accepted() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let pair = generate(dir.path());
        let pin = hex(&Sha256::digest(&pair.der));

        // The relay is the server here as well; the client side of the pin is
        // exercised by pointing a second relay at it.
        let server_port = reserve_port().await;

        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_echo(far);

        let (server, _server_trigger) = start(
            &format!(
                "tls-listen:127.0.0.1:{server_port},cert={},keyfile={}",
                pair.cert_path.display(),
                pair.key_path.display(),
            ),
            &format!("tcp:{far_addr}"),
            &[],
        )
        .await;

        let client_port = reserve_port().await;
        let (client_relay, _client_trigger) = start(
            &format!("tcp-listen:127.0.0.1:{client_port}"),
            &format!("tls:{NAME}:{server_port},pin=sha256:{pin}"),
            &[],
        )
        .await;

        let mut client = connect_tcp(client_port).await;
        client.write_all(PAYLOAD).await.expect("client write");
        client.shutdown().await.expect("client half close");

        let mut echoed = Vec::new();
        client
            .read_to_end(&mut echoed)
            .await
            .expect("client read back");

        assert_eq!(echoed, PAYLOAD, "the pinned path altered the bytes");
        assert_eq!(echo.await.expect("echo task"), PAYLOAD);

        client_relay.await.expect("client relay").expect("run");
        server.await.expect("server relay").expect("run");
    })
    .await;
}

/// A peer that is not speaking TLS is an error, and nothing is sent to it in
/// the clear first.
///
/// The assertion about the far side is the important half. A layer that failed
/// open would look like a working relay, and the only way to notice is that the
/// bytes arrived unencrypted.
#[tokio::test]
async fn a_failed_handshake_does_not_fall_back_to_plaintext() {
    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_echo(far);

        let port = reserve_port().await;
        let (relay, _trigger) = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("tls:{far_addr}"),
            &[],
        )
        .await;

        let mut client = connect_tcp(port).await;
        let _ = client.write_all(PAYLOAD).await;

        let error = relay
            .await
            .expect("relay task")
            .expect_err("a handshake with a plain TCP peer must fail");

        let message = format!("{error:#}");
        assert!(
            message.contains("handshake") || message.contains("tls"),
            "the error should name the handshake, got: {message}",
        );

        drop(client);

        let seen = echo.await.expect("echo task");
        assert!(
            !seen.windows(PAYLOAD.len()).any(|w| w == PAYLOAD),
            "the payload reached the peer in the clear",
        );
    })
    .await;
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
