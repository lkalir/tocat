//! mtls.rs: client certificates, on both sides.
//!
//! The case that matters is `client-auth=required` refusing a client with no
//! certificate, because a server that asks for one and then accepts a client
//! without looks identical to a working one from the outside. Everything else
//! here exists so that case cannot pass for the wrong reason.
//!
//! A small PKI is built per run: one CA, a server certificate and a client
//! certificate under it. Both sides name the CA, which is what makes the trust
//! mutual rather than two unrelated checks.

use std::{future::Future, path::Path, time::Duration};

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
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

const NAME: &str = "localhost";
const PAYLOAD: &[u8] = b"through two certificates";

async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

/// A CA, a server pair and a client pair, all on disk.
struct Pki {
    ca: std::path::PathBuf,
    server_cert: std::path::PathBuf,
    server_key: std::path::PathBuf,
    client_cert: std::path::PathBuf,
    client_key: std::path::PathBuf,
}

fn pki(dir: &Path) -> Pki {
    let ca_key = KeyPair::generate().expect("ca key");

    let mut ca_params = CertificateParams::new(Vec::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    let ca_cert = ca_params.self_signed(&ca_key).expect("self sign the ca");
    let issuer = Issuer::new(ca_params, ca_key);

    let issued = |names: Vec<String>, cert_at: &Path, key_at: &Path| {
        let key = KeyPair::generate().expect("leaf key");
        let params = CertificateParams::new(names).expect("leaf params");
        let cert = params.signed_by(&key, &issuer).expect("sign the leaf");

        std::fs::write(cert_at, cert.pem()).expect("write the certificate");
        std::fs::write(key_at, key.serialize_pem()).expect("write the key");
    };

    let ca = dir.join("ca.pem");
    let server_cert = dir.join("server.pem");
    let server_key = dir.join("server-key.pem");
    let client_cert = dir.join("client.pem");
    let client_key = dir.join("client-key.pem");

    issued(vec![NAME.to_owned()], &server_cert, &server_key);
    issued(vec!["client".to_owned()], &client_cert, &client_key);

    std::fs::write(&ca, ca_cert.pem()).expect("write the ca");

    Pki {
        ca,
        server_cert,
        server_key,
        client_cert,
        client_key,
    }
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

async fn start(source: &str, sink: &str) -> (JoinHandle<anyhow::Result<()>>, shutdown::Trigger) {
    let relay = build(source, sink).await.expect("relay construction");
    let (trigger, shutdown) = shutdown::channel();

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

/// A server asking for a client certificate, and a client presenting one, with
/// both trusting the same CA.
#[tokio::test]
async fn a_mutual_handshake_carries_bytes() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let pki = pki(dir.path());

        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_echo(far);

        let server_port = reserve_port().await;
        let (server, _server_trigger) = start(
            &format!(
                "tls-listen:127.0.0.1:{server_port},cert={},keyfile={},cafile={},client-auth=required",
                pki.server_cert.display(),
                pki.server_key.display(),
                pki.ca.display(),
            ),
            &format!("tcp:{far_addr}"),
        )
        .await;

        let client_port = reserve_port().await;
        let (client_relay, _client_trigger) = start(
            &format!("tcp-listen:127.0.0.1:{client_port}"),
            &format!(
                "tls:{NAME}:{server_port},cafile={},cert={},keyfile={}",
                pki.ca.display(),
                pki.client_cert.display(),
                pki.client_key.display(),
            ),
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

        assert_eq!(echoed, PAYLOAD, "the mutual path altered the bytes");
        assert_eq!(echo.await.expect("echo task"), PAYLOAD);

        client_relay.await.expect("client relay").expect("run");
        server.await.expect("server relay").expect("run");
    })
    .await;
}

/// `client-auth=required` refuses a client with no certificate.
///
/// The assertion is on the far side: a server that asked and then accepted
/// anyway would relay the payload, and nothing else about the run would look
/// different.
#[tokio::test]
async fn required_client_auth_refuses_an_anonymous_client() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let pki = pki(dir.path());

        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_echo(far);

        let server_port = reserve_port().await;
        let (server, server_trigger) = start(
            &format!(
                "tls-listen:127.0.0.1:{server_port},fork,cert={},keyfile={},cafile={},client-auth=required",
                pki.server_cert.display(),
                pki.server_key.display(),
                pki.ca.display(),
            ),
            &format!("tcp:{far_addr}"),
        )
        .await;

        // No cert= or keyfile=: this client has nothing to present.
        let client_port = reserve_port().await;
        let (client_relay, _client_trigger) = start(
            &format!("tcp-listen:127.0.0.1:{client_port}"),
            &format!(
                "tls:{NAME}:{server_port},cafile={}",
                pki.ca.display(),
            ),
        )
        .await;

        let mut client = connect_tcp(client_port).await;
        let _ = client.write_all(PAYLOAD).await;

        client_relay
            .await
            .expect("client relay")
            .expect_err("the handshake must fail without a client certificate");

        drop(client);
        server_trigger.drain();
        server.await.expect("server relay").expect("run");

        let seen = echo.await.expect("echo task");
        assert!(
            seen.is_empty(),
            "the payload reached the far side despite the refused handshake",
        );
    })
    .await;
}

/// `client-auth=optional` accepts the same anonymous client.
///
/// Together with the case above this pins the difference between the two
/// settings, which is otherwise only visible in whether a connection happens.
#[tokio::test]
async fn optional_client_auth_accepts_an_anonymous_client() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let pki = pki(dir.path());

        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_echo(far);

        let server_port = reserve_port().await;
        let (server, _server_trigger) = start(
            &format!(
                "tls-listen:127.0.0.1:{server_port},cert={},keyfile={},cafile={},client-auth=optional",
                pki.server_cert.display(),
                pki.server_key.display(),
                pki.ca.display(),
            ),
            &format!("tcp:{far_addr}"),
        )
        .await;

        let client_port = reserve_port().await;
        let (client_relay, _client_trigger) = start(
            &format!("tcp-listen:127.0.0.1:{client_port}"),
            &format!("tls:{NAME}:{server_port},cafile={}", pki.ca.display()),
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

        assert_eq!(echoed, PAYLOAD);
        assert_eq!(echo.await.expect("echo task"), PAYLOAD);

        client_relay.await.expect("client relay").expect("run");
        server.await.expect("server relay").expect("run");
    })
    .await;
}

/// The combinations that cannot mean anything are refused before a socket is
/// opened, which is the rule the whole endpoint layer is built on.
#[tokio::test]
async fn wrong_side_options_are_refused_at_build_time() {
    run(async {
        let dir = tempfile::tempdir().expect("temp dir");
        let pki = pki(dir.path());

        let port = reserve_port().await;
        let certs = format!(
            "cert={},keyfile={}",
            pki.server_cert.display(),
            pki.server_key.display(),
        );

        for (endpoint, expected) in [
            // A listener with nothing to present.
            (format!("tls-listen:127.0.0.1:{port}"), "cert= and keyfile="),
            // Asking for client certificates without saying who may issue them.
            (
                format!("tls-listen:127.0.0.1:{port},{certs},client-auth=required"),
                "cafile=",
            ),
            // Naming an issuer that nothing would consult.
            (
                format!(
                    "tls-listen:127.0.0.1:{port},{certs},cafile={}",
                    pki.ca.display()
                ),
                "client-auth",
            ),
            // A client option on a listener.
            (
                format!("tls-listen:127.0.0.1:{port},{certs},verify=none"),
                "client option",
            ),
            // A listener option on a client.
            (
                format!("tls:{NAME}:{port},client-auth=required"),
                "tls-listen option",
            ),
            // Half an identity.
            (
                format!("tls:{NAME}:{port},cert={}", pki.server_cert.display()),
                "go together",
            ),
        ] {
            let error = build("-", &endpoint)
                .await
                .err()
                .unwrap_or_else(|| panic!("{endpoint} was accepted"));

            let message = format!("{error:#}");
            assert!(
                message.contains(expected),
                "{endpoint}\n  expected a message mentioning {expected:?}, got: {message}",
            );
        }
    })
    .await;
}
