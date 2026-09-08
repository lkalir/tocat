//! noise.rs: the Noise layer over a real handshake.
//!
//! The same three things `tls.rs` proves, and one it cannot. A round trip shows
//! the layer carries bytes. A pin shows that `peer=` is what turns `xx` from
//! opportunistic into authenticated. A failure has to end the run, because the
//! failure mode that matters is not "it broke" but "it quietly did not
//! encrypt".
//!
//! The fourth is interoperability. `snow` is a dependency rather than a dev
//! dependency, so a case here can be a bare Noise peer and drive the handshake
//! by hand. That turns the framing into a contract with something outside
//! tocat instead of an agreement tocat has with itself, which is the only way
//! to catch a change to the length prefix that both ends would otherwise accept
//! happily.
//!
//! Keys are generated per case rather than checked in. A fixture private key in
//! a repository is a key somebody eventually uses.

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
    task::JoinHandle,
    time::{sleep, timeout},
};

const LOOPBACK_ANY: &str = "127.0.0.1:0";
const CASE: Duration = Duration::from_secs(20);
const READY: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const BUFFER: usize = 64 * 1024;

const PAYLOAD: &[u8] = b"through the handshake and back";

/// A 32 byte shared secret, which is what every key option here takes.
const PSK: &str = "0f0e0d0c0b0a09080706050403020100f0e0d0c0b0a090807060504030201000";

/// The suite the layer defaults to, spelled out for the hand driven peer.
const PROTOCOL: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// Largest Noise message, and so the largest frame body.
const MAX_MESSAGE: usize = 65535;

async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).expect("hex"), 16).expect("hex"))
        .collect()
}

/// An X25519 static pair, as the hex the options take.
fn keypair() -> (String, String) {
    let keypair = snow::Builder::new(
        "Noise_XX_25519_ChaChaPoly_BLAKE2s"
            .parse()
            .expect("protocol name"),
    )
    .generate_keypair()
    .expect("generate a keypair");

    (hex(&keypair.private), hex(&keypair.public))
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
///
/// The accept is bounded. A case where the hadnshake fails on the listening
/// side never reaches the point of dialling here, so an unbounded accept would
/// hang the case rather than let it assert that nothing arrived.
fn spawn_echo(listener: TcpListener) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let Ok(Ok((mut stream, _))) = timeout(READY, listener.accept()).await else {
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

/// Stand up the pair of relays every round trip case uses.
///
/// A plain TCP port on the near side, Noise between the two relays, and a plain
/// echo on the far side, so a case only has to speak TCP at both ends and the
/// only thing under test is what happens in between.
struct Tunnel {
    /// The plain TCP port a case connects to.
    port: u16,
    /// Resolves to everything the far side saw in the clear.
    echo: JoinHandle<Vec<u8>>,
    client: JoinHandle<anyhow::Result<()>>,
    server: JoinHandle<anyhow::Result<()>>,
    /// Held only so the relays are not asked to drain while a case runs.
    _triggers: (Trigger, Trigger),
}

async fn tunnel(client_options: &str, server_options: &str) -> Tunnel {
    let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
    let far_addr = far.local_addr().expect("far address");
    let echo = spawn_echo(far);

    let noise_port = reserve_port().await;
    let (server, server_trigger) = start(
        &format!("noise-listen:127.0.0.1:{noise_port},{server_options}"),
        &format!("tcp:{far_addr}"),
        &[],
    )
    .await;

    let near_port = reserve_port().await;
    let (client, client_trigger) = start(
        &format!("tcp-listen:127.0.0.1:{near_port}"),
        &format!("noise:127.0.0.1:{noise_port},{client_options}"),
        &[],
    )
    .await;

    Tunnel {
        port: near_port,
        echo,
        client,
        server,
        _triggers: (client_trigger, server_trigger),
    }
}

/// Write, half close, read back, and assert the bytes survived both directions.
async fn round_trip(port: u16) -> Vec<u8> {
    let mut client = connect_tcp(port).await;
    client.write_all(PAYLOAD).await.expect("client write");
    client.shutdown().await.expect("client half close");

    let mut echoed = Vec::new();
    client
        .read_to_end(&mut echoed)
        .await
        .expect("client read back");

    echoed
}

/// `nnpsk0` carries bytes both ways with nothing but a shared secret.
#[tokio::test]
async fn a_shared_secret_is_enough() {
    run(async {
        let options = format!("pattern=nnpsk0,psk={PSK}");
        let Tunnel {
            port,
            echo,
            client,
            server,
            _triggers,
        } = tunnel(&options, &options).await;

        assert_eq!(
            round_trip(port).await,
            PAYLOAD,
            "the reverse path altered the bytes"
        );
        assert_eq!(
            echo.await.expect("echo task"),
            PAYLOAD,
            "the far side did not receive the plaintext",
        );

        client.await.expect("client relay").expect("run");
        server.await.expect("server relay").expect("run");
    })
    .await;
}

/// `xx` with both sides pinning the other, which is the arrangement that
/// actually authenticates.
#[tokio::test]
async fn mutually_pinned_static_keys_are_accepted() {
    run(async {
        let (client_private, client_public) = keypair();
        let (server_private, server_public) = keypair();

        let Tunnel {
            port,
            echo,
            client,
            server,
            _triggers,
        } = tunnel(
            &format!("pattern=xx,key={client_private},peer={server_public}"),
            &format!("pattern=xx,key={server_private},peer={client_public}"),
        )
        .await;

        assert_eq!(round_trip(port).await, PAYLOAD);
        assert_eq!(echo.await.expect("echo task"), PAYLOAD);

        client.await.expect("client relay").expect("run");
        server.await.expect("server relay").expect("run");
    })
    .await;
}

/// `ik` saves a round trip by having the dialling side know the listener's key.
#[tokio::test]
async fn ik_authenticates_the_listener_from_the_first_message() {
    run(async {
        let (client_private, _client_public) = keypair();
        let (server_private, server_public) = keypair();

        let Tunnel {
            port,
            echo,
            client,
            server,
            _triggers,
        } = tunnel(
            &format!("pattern=ik,key={client_private},peer={server_public}"),
            &format!("pattern=ik,key={server_private}"),
        )
        .await;

        assert_eq!(round_trip(port).await, PAYLOAD);
        assert_eq!(echo.await.expect("echo task"), PAYLOAD);

        client.await.expect("client relay").expect("run");
        server.await.expect("server relay").expect("run");
    })
    .await;
}

/// The suite is chosen independently of the pattern, and both ends have to
/// agree because Noise negotiates nothing.
#[tokio::test]
async fn a_non_default_suite_round_trips() {
    run(async {
        let options = format!("pattern=nnpsk0,psk={PSK},cipher=aesgcm,hash=sha256");
        let Tunnel {
            port,
            echo,
            client,
            server,
            _triggers,
        } = tunnel(&options, &options).await;

        assert_eq!(round_trip(port).await, PAYLOAD);
        assert_eq!(echo.await.expect("echo task"), PAYLOAD);

        client.await.expect("client relay").expect("run");
        server.await.expect("server relay").expect("run");
    })
    .await;
}

/// Rekeying is deterministic and unsignalled, so the only thing keeping the two
/// ends in step is that they count the same records. Enough traffic to cross
/// the boundary many times is the only way to find out that they do.
///
/// Written and read concurrently: half a megabyte will not fit in the socket
/// buffers, so writing it all before reading anything would deadlock against
/// the echo rather than test anything.
#[tokio::test]
async fn a_long_transfer_rekeys_in_step() {
    run(async {
        let options = format!("pattern=nnpsk0,psk={PSK},rekey=16");
        let Tunnel {
            port,
            echo,
            client,
            server,
            _triggers,
        } = tunnel(&options, &options).await;

        let payload: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();

        let mut stream = connect_tcp(port).await;
        let (mut reader, mut writer) = stream.split();

        let write = async {
            writer.write_all(&payload).await.expect("client write");
            writer.shutdown().await.expect("client half close");
        };

        let read = async {
            let mut back = Vec::with_capacity(expected.len());
            reader
                .read_to_end(&mut back)
                .await
                .expect("client read back");
            back
        };

        let (_, back) = tokio::join!(write, read);

        assert_eq!(back.len(), expected.len(), "the transfer was truncated");
        assert_eq!(back, expected, "the transfer was corrupted");

        assert_eq!(echo.await.expect("echo task").len(), expected.len());

        client.await.expect("client relay").expect("run");
        server.await.expect("server relay").expect("run");
    })
    .await;
}

/// A pin that does not match the key the peer presented ends the run, and the
/// payload never reaches the far side.
///
/// The second assertion is the important half, and it is the whole reason
/// `peer=` exists. A layer that logged the mismatch and carried on would look
/// like a working relay.
#[tokio::test]
async fn a_mismatched_pin_is_refused() {
    run(async {
        let (client_private, _client_public) = keypair();
        let (server_private, _server_public) = keypair();
        let (_unrelated_private, unrelated_public) = keypair();

        let Tunnel {
            port,
            echo,
            client,
            server,
            _triggers,
        } = tunnel(
            &format!("pattern=xx,key={client_private},peer={unrelated_public}"),
            &format!("pattern=xx,key={server_private}"),
        )
        .await;

        let mut near = connect_tcp(port).await;
        let _ = near.write_all(PAYLOAD).await;

        let error = client
            .await
            .expect("client relay task")
            .expect_err("a mismatched pin must fail");

        let message = format!("{error:#}");
        assert!(
            message.contains("peer="),
            "the error should name the option, got: {message}",
        );

        drop(near);
        let _ = server.await;

        let seen = echo.await.expect("echo task");
        assert!(
            !seen.windows(PAYLOAD.len()).any(|w| w == PAYLOAD),
            "the payload reached the far side despite the refused pin",
        );
    })
    .await;
}

/// Two different secrets do not agree, and nothing crosses.
#[tokio::test]
async fn a_mismatched_shared_secret_is_refused() {
    run(async {
        let other = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";

        let Tunnel {
            port,
            echo,
            client,
            server,
            _triggers,
        } = tunnel(
            &format!("pattern=nnpsk0,psk={PSK}"),
            &format!("pattern=nnpsk0,psk={other}"),
        )
        .await;

        let mut near = connect_tcp(port).await;
        let _ = near.write_all(PAYLOAD).await;

        assert!(
            client.await.expect("client relay task").is_err(),
            "a handshake with the wrong secret must fail",
        );

        drop(near);
        let _ = server.await;

        let seen = echo.await.expect("echo task");
        assert!(
            !seen.windows(PAYLOAD.len()).any(|w| w == PAYLOAD),
            "the payload reached the far side despite the failed handshake",
        );
    })
    .await;
}

/// A peer that is not speaking Noise is an error, and nothing is sent to it in
/// the clear first.
#[tokio::test]
async fn a_failed_handshake_does_not_fall_back_to_plaintext() {
    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_echo(far);

        let port = reserve_port().await;
        let (relay, _trigger) = start(
            &format!("tcp-listen:127.0.0.1:{port}"),
            &format!("noise:{far_addr},pattern=nnpsk0,psk={PSK}"),
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
            message.contains("noise"),
            "the error should name the layer, got: {message}",
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

/// Write one length prefixed message, the framing the layer puts around every
/// Noise message.
async fn write_frame(stream: &mut TcpStream, message: &[u8]) {
    let len = u16::try_from(message.len()).expect("message fits the prefix");

    stream
        .write_all(&len.to_be_bytes())
        .await
        .expect("write the length");
    stream.write_all(message).await.expect("write the message");
    stream.flush().await.expect("flush");
}

/// Read one length prefixed message.
async fn read_frame(stream: &mut TcpStream, buf: &mut [u8]) -> usize {
    let mut prefix = [0u8; 2];
    stream
        .read_exact(&mut prefix)
        .await
        .expect("read the length");

    let len = usize::from(u16::from_be_bytes(prefix));
    stream
        .read_exact(&mut buf[..len])
        .await
        .expect("read the message");

    len
}

/// A bare `snow` peer completes a handshake with `noise-listen:` and exchanges
/// traffic, with the framing written out by hand.
///
/// This is the case the layer's own tests cannot give: everything else here
/// would still pass if the prefix were little endian, or four bytes, or absent,
/// because both ends would have changed together. Here one end is not tocat.
#[tokio::test]
async fn a_bare_snow_peer_interoperates() {
    run(async {
        let far = TcpListener::bind(LOOPBACK_ANY).await.expect("far listener");
        let far_addr = far.local_addr().expect("far address");
        let echo = spawn_echo(far);

        let port = reserve_port().await;
        let (relay, _trigger) = start(
            &format!("noise-listen:127.0.0.1:{port},pattern=nnpsk0,psk={PSK}"),
            &format!("tcp:{far_addr}"),
            &[],
        )
        .await;

        let psk = unhex(PSK).try_into().expect("correct key length");
        let mut handshake = snow::Builder::new(PROTOCOL.parse().expect("protocol name"))
            .psk(0, &psk)
            .expect("psk")
            .build_initiator()
            .expect("build the initiator");

        let mut stream = connect_tcp(port).await;
        let mut message = vec![0u8; MAX_MESSAGE];
        let mut payload = vec![0u8; MAX_MESSAGE];

        while !handshake.is_handshake_finished() {
            if handshake.is_my_turn() {
                let len = handshake
                    .write_message(&[], &mut message)
                    .expect("write a handshake message");

                write_frame(&mut stream, &message[..len]).await;
            } else {
                let len = read_frame(&mut stream, &mut message).await;
                handshake
                    .read_message(&message[..len], &mut payload)
                    .expect("read a handshake message");
            }
        }

        let mut transport = handshake
            .into_transport_mode()
            .expect("into transport mode");

        let len = transport
            .write_message(PAYLOAD, &mut message)
            .expect("encrypt");
        write_frame(&mut stream, &message[..len]).await;

        let len = read_frame(&mut stream, &mut message).await;
        let len = transport
            .read_message(&message[..len], &mut payload)
            .expect("decrypt the echo");

        assert_eq!(
            &payload[..len],
            PAYLOAD,
            "the bare peer and the layer disagree about the wire format",
        );

        // Before the echo is awaited, not after: the relay only closes the far
        // connection once this side ends, and the echo only returns when its
        // connections closes.
        drop(stream);

        assert_eq!(
            echo.await.expect("echo task"),
            PAYLOAD,
            "the far side did not receive the plaintext",
        );

        let _ = relay.await;
    })
    .await;
}
