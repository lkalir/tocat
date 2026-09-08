//! tun.rs: the tun and tap endpoints against real kernel interfaces.
//!
//! # Why these are gated rather than skipped quietly
//!
//! Creating an interface needs `CAP_NET_ADMIN`, which a developer running
//! `cargo test` will not have. These cases skip in that situation, because a
//! suite that fails on a laptop is a suite people stop running.
//!
//! A skip that is invisible is worse than no test, so `TOCAT_TEST_TUN` turns
//! the skip into a failure. CI sets it and grants the capability with `capsh`,
//! so a runner that quietly loses `/dev/net/tun` is a red build rather than a
//! green one that tested nothing.
//!
//! # What these prove that the unit tests cannot
//!
//! The unit tests cover option parsing and the rules derived from it. Nothing
//! there opens a device, so the ioctl, the netlink calls behind `ipv4` and
//! `mac`, the mtu check, and the packet path are all untested by them. Every
//! case here uses tocat's own options rather than shelling out to `ip`, so the
//! configuration path is under test too and CI needs no iproute2.
//!
//! # Why each case gets its own interface and subnet
//!
//! nextest runs cases concurrently in separate processes, and interfaces and
//! routes are host wide. Sharing either would make the suite order dependent.

use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    time::Duration,
};

use tocat_core::{
    endpoint::EndpointSpec,
    relay::Relay,
    shutdown::{self, Trigger},
};
use tokio::{
    net::UdpSocket,
    task::JoinHandle,
    time::{sleep, timeout},
};

const CASE: Duration = Duration::from_secs(20);
const READY: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
const BUFFER: usize = 64 * 1024;

const PAYLOAD: &[u8] = b"through the interface";

/// `CAP_NET_ADMIN`, which is what all of this needs.
const NET_ADMIN: u32 = 12;

async fn run<F: Future<Output = ()>>(body: F) {
    timeout(CASE, body).await.expect("the case timed out");
}

/// Whether this process can create an interface.
///
/// Read from the effective set rather than inferred from the user id: under
/// `capsh` the tests run as an ordinary user that has been granted exactly this
/// one capability, so a root check would be wrong in both directions.
fn has_net_admin() -> bool {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };

    status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << NET_ADMIN) != 0)
}

/// Decide whether to skip, and refuse to skip where skipping would hide a
/// broken runner.
fn unavailable() -> bool {
    let device = Path::new("/dev/net/tun").exists();
    let capability = has_net_admin();

    if device && capability {
        return false;
    }

    assert!(
        std::env::var_os("TOCAT_TEST_TUN").is_none(),
        "TOCAT_TEST_TUN is set, so these cases must run, but /dev/net/tun \
         exists: {device} and CAP_NET_ADMIN is held: {capability}",
    );

    eprintln!("skipping: needs /dev/net/tun and CAP_NET_ADMIN; set TOCAT_TEST_TUN to require it");

    true
}

async fn start(source: &str, sink: &str) -> (JoinHandle<anyhow::Result<()>>, Trigger) {
    let source: EndpointSpec = source.parse().expect("source spec");
    let sink: EndpointSpec = sink.parse().expect("sink spec");

    let (trigger, shutdown) = shutdown::channel();

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

    (tokio::spawn(relay.run(shutdown)), trigger)
}

/// Build a relay that is expected to fail, and return the rendered error.
///
/// Which of the two steps fails is not the point of any case that uses this,
/// and pinning it down would make the cases fragile against a change in when
/// endpoints are opened.
async fn failure(source: &str, sink: &str, buffer: usize) -> String {
    let source: EndpointSpec = source.parse().expect("source spec");
    let sink: EndpointSpec = sink.parse().expect("sink spec");

    let (_trigger, shutdown) = shutdown::channel();

    let relay = Relay::new(
        source,
        sink,
        Vec::new(),
        tocat_plugins::native_registry(),
        buffer,
        None,
    )
    .await;

    match relay {
        Err(error) => format!("{error:#}"),
        Ok(relay) => format!(
            "{:#}",
            relay.run(shutdown).await.expect_err("this must fail"),
        ),
    }
}

/// A UDP socket on loopback, which is the other side of every relay here.
///
/// Tokio's rather than the standard library's, and every wait below is an
/// await, because the relay is a spawned task on the same current thread
/// runtime. A blocking read here would starve it rather than wait for it.
async fn loopback() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").await.expect("bind loopback")
}

/// The same, but losing a race against the relay ending is reported as the
/// relay's own error.
///
/// A bare timeout says only that nothing arrived, which is the less useful half
/// of the story whenever the relay already failed and knows why.
async fn receive_while_running(
    socket: &UdpSocket,
    buf: &mut [u8],
    what: &str,
    relay: &mut JoinHandle<anyhow::Result<()>>,
) -> (usize, SocketAddr) {
    let received = timeout(READY, async {
        tokio::select! {
            received = socket.recv_from(buf) => received,
            ended = &mut *relay => panic!("the relay ended before {what} arrived: {ended:?}"),
        }
    })
    .await;

    received
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|error| panic!("failed waiting for {what}: {error}"))
}

/// Wait until an address the relay was asked to assign actually exists.
///
/// Doubles as the readiness check: binding it can only succeed once the
/// interface has been created, addressed and brought up, so nothing here needs
/// to poll the relay itself.
async fn await_address(address: Ipv4Addr, port: u16) -> UdpSocket {
    let deadline = std::time::Instant::now() + READY;

    loop {
        match UdpSocket::bind(SocketAddr::from((address, port))).await {
            Ok(socket) => return socket,
            Err(error) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the relay never assigned {address}: {error}",
                );

                sleep(POLL).await;
            }
        }
    }
}

/// A layer 3 interface carries whole IP packets in both directions.
///
/// Outbound: a datagram addressed into the interface's subnet is routed out of
/// it and reaches the relay's far side as one IP packet. Inbound: that packet
/// is sent back with its addresses and ports swapped, the relay writes it to
/// the interface, and the kernel delivers it to the socket that sent the
/// original as a reply.
///
/// The reply is built by swapping rather than by hand. Every field either
/// checksum covers is swapped a whole 16 bit word at a time, so both remain
/// valid without recomputation, and a packet the kernel itself produced cannot
/// be malformed in a way this case would then blame on the relay.
#[tokio::test]
async fn a_tun_carries_packets_both_ways() {
    if unavailable() {
        return;
    }

    run(async {
        let ours = Ipv4Addr::new(10, 201, 0, 1);
        let theirs = Ipv4Addr::new(10, 201, 0, 2);

        let far = loopback().await;
        let far_addr = far.local_addr().expect("far address");

        let (mut relay, _trigger) = start(
            &format!("tun:tocat-t0,ipv4={ours}/24"),
            &format!("udp:{far_addr}"),
        )
        .await;

        // Binding the interface's own address is the readiness check: it can
        // only succeed once the relay has created and addressed it.
        await_address(ours, 0).await;

        let sender = UdpSocket::bind("0.0.0.0:0").await.expect("bind a sender");
        sender
            .send_to(PAYLOAD, SocketAddr::from((theirs, 9999)))
            .await
            .expect("send into the subnet");

        // An interface that has just come up sends traffic of its own first:
        // IPv6 solicitations and IGMP reports, before anything this case asked
        // for. Take packets until ours turns up rather than assuming it leads.
        let mut buf = [0u8; 4096];
        let mut found = None;

        for _ in 0..16 {
            let (n, from) =
                receive_while_running(&far, &mut buf, "a packet out of the interface", &mut relay)
                    .await;

            let packet = &buf[..n];

            let is_ours = n >= 28
                && packet[0] >> 4 == 4
                && packet[9] == 17
                && packet[16..20] == theirs.octets();

            if !is_ours {
                continue;
            }

            assert!(
                packet.ends_with(PAYLOAD),
                "the payload did not survive the interface",
            );

            found = Some((packet.to_vec(), from));
            break;
        }

        let (packet, relay_addr) =
            found.expect("no IPv4 datagram for this subnet came out of the interface");

        // Turn it around: addresses at 12..20, ports at 20..24.
        let mut reply = packet;
        reply.swap(12, 16);
        reply.swap(13, 17);
        reply.swap(14, 18);
        reply.swap(15, 19);
        reply.swap(20, 22);
        reply.swap(21, 23);

        far.send_to(&reply, relay_addr)
            .await
            .expect("send the reply back to the relay");

        let (n, from) = receive_while_running(&sender, &mut buf, "the reply", &mut relay).await;

        assert_eq!(&buf[..n], PAYLOAD, "the reply was altered");
        assert_eq!(
            from,
            SocketAddr::from((theirs, 9999)),
            "the reply did not come back from the address it was sent to",
        );

        relay.abort();
    })
    .await;
}

/// A layer 2 interface carries Ethernet frames, and uses the hardware address
/// it was given.
///
/// Asserting on the source address of the frames is what makes this a test of
/// `mac=` rather than only of `tap:`. A tap is given a random address when it
/// is created, so a frame carrying the one we asked for cannot be a
/// coincidence.
///
/// The trigger is an ARP request: sending into the subnet with no neighbour
/// known makes the kernel ask, and asking is an Ethernet frame.
#[tokio::test]
async fn a_tap_carries_frames_with_the_hardware_address_it_was_given() {
    if unavailable() {
        return;
    }

    run(async {
        let ours = Ipv4Addr::new(10, 202, 0, 1);
        let theirs = Ipv4Addr::new(10, 202, 0, 2);
        let mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0xaa];

        let far = loopback().await;
        let far_addr = far.local_addr().expect("far address");

        let (relay, _trigger) = start(
            &format!("tap:tocat-p0,ipv4={ours}/24,mac=02:00:00:00:00:aa"),
            &format!("udp:{far_addr}"),
        )
        .await;

        await_address(ours, 9997).await;

        let sender = UdpSocket::bind("0.0.0.0:0").await.expect("bind a sender");
        sender
            .send_to(PAYLOAD, SocketAddr::from((theirs, 9999)))
            .await
            .expect("send into the subnet");

        // A fresh interface also emits IPv6 solicitations, so take frames until
        // the ARP arrives rather than assuming it is first.
        let mut buf = [0u8; 4096];
        let mut saw_arp = false;

        for _ in 0..16 {
            let Ok(Ok((n, _))) = timeout(READY, far.recv_from(&mut buf)).await else {
                break;
            };

            let frame = &buf[..n];
            assert!(n >= 14, "a frame shorter than an Ethernet header");
            assert_eq!(
                &frame[6..12],
                &mac,
                "a frame did not carry the hardware address the interface was given",
            );

            if u16::from_be_bytes([frame[12], frame[13]]) == 0x0806 {
                assert_eq!(&frame[0..6], &[0xff; 6], "an ARP request is a broadcast");
                saw_arp = true;
                break;
            }
        }

        assert!(saw_arp, "no ARP request came out of the interface");

        relay.abort();
    })
    .await;
}

/// An interface whose mtu exceeds the copy buffer is refused at startup.
///
/// This is the case the check exists for. Without it a packet longer than the
/// buffer would be truncated rather than split, silently, on every large
/// transfer, so a loud failure here is the whole point.
#[tokio::test]
async fn an_mtu_larger_than_the_buffer_is_refused() {
    if unavailable() {
        return;
    }

    run(async {
        let far = loopback().await;
        let far_addr = far.local_addr().expect("far address");

        let message = failure(
            "tun:tocat-t1,ipv4=10.203.0.1/24,mtu=4000",
            &format!("udp:{far_addr}"),
            2048,
        )
        .await;

        assert!(
            message.contains("mtu") && message.contains("4000"),
            "the error should name the mtu and the buffer, got: {message}",
        );
    })
    .await;
}

/// The layer of an existing interface is fixed, so opening a tap as a tun is
/// refused by the kernel and the error says which interface and which layer.
#[tokio::test]
async fn a_tap_cannot_be_opened_as_a_tun() {
    if unavailable() {
        return;
    }

    run(async {
        let far = loopback().await;
        let far_addr = far.local_addr().expect("far address");

        // Held open so the interface exists for the second relay to collide
        // with: an interface tocat creates is not persistent.
        let (holder, _trigger) = start(
            "tap:tocat-p1,ipv4=10.204.0.1/24",
            &format!("udp:{far_addr}"),
        )
        .await;

        await_address(Ipv4Addr::new(10, 204, 0, 1), 9996).await;

        let message = failure(
            "tun:tocat-p1",
            &format!("udp:{}", loopback().await.local_addr().expect("address")),
            BUFFER,
        )
        .await;

        assert!(
            message.contains("tocat-p1") && message.contains("tun"),
            "the error should name the interface and the layer, got: {message}",
        );

        holder.abort();
    })
    .await;
}
