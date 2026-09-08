//! tun.rs: a kernel network interface as an endpoint.
//!
//! # Role in the pipeline
//!
//! `tun:` and `tap:` open one of the kernel's virtual interfaces and hand the
//! relay its packets. A read is one packet and a write is one packet, so this
//! is a message endpoint like `udp:` and not a byte stream, and it reaches the
//! rest of tocat as [`DatagramSocket::Boxed`]. Every consequence of that is one
//! that already applies to the other message transports: an oversized packet is
//! truncated rather than split, there is no end of stream to observe, and
//! `check` refuses a `tls` or `noise` layer over it because neither keeps
//! boundaries.
//!
//! The two schemes differ by one flag. `tun:` is layer 3 and carries IP
//! packets; `tap:` is layer 2 and carries Ethernet frames. They are separate
//! variants rather than one with a mode option for the reason `unix-dgram` and
//! `unix-seqpacket` are separate: what a scheme carries is the first thing a
//! reader needs to know about it, and an option is a worse place to say it.
//!
//! # Attaching versus creating
//!
//! Both are the same call. Naming a device that already exists attaches to it,
//! and naming one that does not creates it, which is the kernel's own
//! behaviour rather than anything decided here. What the operator sees is a
//! difference in lifetime: a device this creates is not persistent and goes
//! away when tocat exits, while one made with `ip tuntap add` outlives the
//! relay along with its addresses and routes.
//!
//! Attaching is the case worth optimising for, because it is the one that runs
//! without privilege. `ip tuntap add mode tap user alice name tap0` hands alice
//! a device she can open, and root does the addressing once.
//!
//! # Why `up` defaults to off, and why the fix is not `enable(false)`
//!
//! `DeviceBuilder::new()` is `default().enable(true)`, so an interface is
//! brought up unless told otherwise, and that needs `CAP_NET_ADMIN`. Left
//! alone it would make every unprivileged attach fail on an interface that was
//! already up, which is the exact case above.
//!
//! The trap is that the obvious correction is also wrong. `enabled` is an
//! `Option<bool>` and the builder calls `device.enabled(..)` for **either**
//! value, so `enable(false)` does not mean "leave it alone", it means "bring it
//! down". That is the same privileged `SIOCSIFFLAGS` and fails the same way,
//! with the same `EPERM`, on an interface the operator had already configured.
//! `inherit_enable_state()` clears the option and is the only setting that
//! touches nothing.
//!
//! # Addressing
//!
//! `ipv4` and `ipv6` exist because of the lifetime difference above, not as a
//! convenience. An interface this creates does not exist until it is opened and
//! is gone when the relay ends, so there is no moment at which `ip addr add`
//! could reach it: before is too early and after is a race against our own
//! startup. For a created interface these are the only way to address it.
//!
//! For an attached interface they are the wrong tool, since whoever made the
//! interface has already addressed it and they need privilege the attach case
//! does not have. That asymmetry is why they default to unset.

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
};

use anyhow::{Context as _, bail};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tracing::{info, warn};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use crate::endpoint::{
    Connection, EndpointStream,
    parse::{Opt, ParseEndpointError},
    stream::{DatagramSocket, MessageSocket},
};

/// `IFNAMSIZ` less the terminator, which is what the kernel accepts.
const MAX_NAME: usize = 15;

/// The settings both schemes share, so the two differ only by their layer.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Interface {
    /// The interface to open. Empty lets the kernel choose, and the name it
    /// chose is read back and logged, since there is otherwise no way to find
    /// out what to configure.
    pub device: Option<String>,

    /// Bring the interface up. Needs `CAP_NET_ADMIN`.
    ///
    /// Three states rather than two. Unset means touch nothing, which is what
    /// lets an attach run unprivileged, except that setting an address implies
    /// it, since an address on a down interface does nothing. `up=false` forces
    /// the touch-nothing behaviour back even when an address is set.
    pub up: Option<bool>,

    /// An address and prefix for the interface, written as `10.10.0.1/24`.
    /// Needs `CAP_NET_ADMIN`.
    pub ipv4: Option<String>,

    /// The same for IPv6, written as `fd00::1/64`. One address: a second use
    /// replaces the first rather than adding to it, as every other repeated
    /// option here behaves.
    pub ipv6: Option<String>,

    /// The hardware address, as `02:00:00:00:00:01`. `tap` only, since a `tun`
    /// interface is point to point and has none. Needs `CAP_NET_ADMIN`.
    pub mac: Option<String>,

    /// Set the interface MTU. Also needs `CAP_NET_ADMIN`. Reading the MTU does
    /// not, and happens either way.
    pub mtu: Option<u16>,

    /// Keep the four byte `struct tun_pi` in front of each packet. Off by
    /// default, matching what `ip tuntap add` does unless asked for `pi`, and
    /// it has to match how the device was made.
    pub packet_info: bool,

    /// The name this endpoint is given in logs and plugin instance names.
    pub name: Option<String>,
}

impl Interface {
    fn option(&mut self, opt: &Opt<'_>) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "up" => self.up = Some(opt.flag()?),
            "ipv4" | "ip4" | "addr" | "address" => {
                let value = opt.string()?;
                cidr4(&value).map_err(ParseEndpointError::InvalidFlag)?;
                self.ipv4 = Some(value);
            }
            "mac" | "macaddr" | "hwaddr" => {
                let value = opt.string()?;
                mac(&value).map_err(ParseEndpointError::InvalidFlag)?;
                self.mac = Some(value);
            }
            "ipv6" | "ip6" => {
                let value = opt.string()?;
                cidr6(&value).map_err(ParseEndpointError::InvalidFlag)?;
                self.ipv6 = Some(value);
            }
            "mtu" => {
                self.mtu = Some(u16::try_from(opt.count()?.get()).map_err(|_| {
                    ParseEndpointError::InvalidFlag("mtu does not fit in 16 bits".to_owned())
                })?);
            }
            "packetinfo" | "pi" => self.packet_info = opt.flag()?,
            "name" => self.name = Some(opt.string()?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
        scheme: &'static str,
    ) -> Result<Self, ParseEndpointError> {
        let mut interface = Interface::default();

        for opt in opts {
            if !interface.option(&opt)? {
                return Err(opt.unsupported(scheme));
            }
        }

        if interface.mac.is_some() && scheme == "tun" {
            return Err(ParseEndpointError::InvalidFlag(
                "mac is a tap option: a tun interface is point to point and has no hardware \
                 address"
                    .to_owned(),
            ));
        }

        if !body.is_empty() {
            if body.len() > MAX_NAME {
                return Err(ParseEndpointError::InvalidFlag(format!(
                    "interface name {body} is longer than {MAX_NAME} characters"
                )));
            }

            interface.device = Some(body.to_owned());
        }

        Ok(interface)
    }

    fn label(&self, scheme: &'static str) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }

        match &self.device {
            Some(device) => format!("{scheme}:{device}"),
            None => scheme.to_owned(),
        }
    }

    /// Open the device and check it against the buffer the relay will use.
    ///
    /// `buffer` is not decoration. [`DatagramSocket::recv`] truncates anything
    /// longer than the buffer it is given, so an MTU above it would lose the
    /// tail of every large packet with nothing in the log to say so. Refusing
    /// here turns a silent corruption into a startup error naming both numbers.
    async fn connect(&self, layer: Layer, buffer: usize) -> anyhow::Result<Connection> {
        let scheme = match layer {
            Layer::L3 => "tun",
            _ => "tap",
        };

        let mut builder = DeviceBuilder::new()
            .layer(layer)
            .packet_information(self.packet_info);

        if let Some(address) = &self.mac {
            builder = builder.mac_addr(mac(address).map_err(anyhow::Error::msg)?);
        }

        if let Some(cidr) = &self.ipv4 {
            let (address, prefix) = cidr4(cidr).map_err(anyhow::Error::msg)?;
            builder = builder.ipv4(address, prefix, None);
        }

        if let Some(cidr) = &self.ipv6 {
            let (address, prefix) = cidr6(cidr).map_err(anyhow::Error::msg)?;
            builder = builder.ipv6(address, prefix);
        }

        // An address on a down interface does nothing, so setting one implies
        // bringing it up unless that was refused explicitly.
        let up = self
            .up
            .unwrap_or(self.ipv4.is_some() || self.ipv6.is_some());

        // Not `enable(false)` for the other case. That brings the interface
        // down, which needs the same privilege as bringing it up, and would
        // break every unprivileged attach. See the note at the top of this
        // module.
        builder = if up {
            builder.enable(true)
        } else {
            builder.inherit_enable_state()
        };

        if let Some(device) = &self.device {
            builder = builder.name(device.clone());
        }

        if let Some(mtu) = self.mtu {
            builder = builder.mtu(mtu);
        }

        let device = builder.build_async().with_context(|| {
            let device = self.device.as_deref().unwrap_or("a new interface");

            format!(
                "opening {device} as {scheme}. An existing interface of the other layer is \
                 rejected by the kernel, and creating one, changing whether it is up, setting \
                 its mtu or giving it an address or hardware address all need CAP_NET_ADMIN. To \
                 run without it, \
                 create and address the interface first with `ip tuntap add mode {scheme} user \
                 $USER name {device}` and drop up, mtu, ipv4 and ipv6",
            )
        })?;

        let name = device
            .name()
            .context("reading back the interface name the kernel chose")?;

        match device.mtu() {
            Ok(mtu) if usize::from(mtu) > buffer => {
                bail!(
                    "{name} has an mtu of {mtu} but the copy buffer is {buffer} bytes, and a \
                     packet longer than the buffer is truncated rather than split. Raise \
                     --buffer-size to at least {mtu}",
                );
            }
            Ok(mtu) => info!(interface = %name, mtu, up, "opened"),
            // Not fatal: the MTU is a check, not something the relay needs.
            Err(error) => warn!(interface = %name, %error, "could not read the mtu to check it"),
        }

        let socket = DatagramSocket::Boxed(Arc::new(Device { device }));

        Ok(EndpointStream::Datagram(socket).into_connection())
    }
}

/// Six hex octets, as `02:00:00:00:00:01`.
///
/// The multicast bit is refused here rather than left to the kernel, which
/// answers `EADDRNOTAVAIL` and reads as though the address were in use.
fn mac(text: &str) -> Result<[u8; 6], String> {
    let mut octets = [0_u8; 6];
    let mut parts = text.split([':', '-']);

    for octet in &mut octets {
        let part = parts
            .next()
            .ok_or_else(|| format!("{text} is not six octets, as in 02:00:00:00:00:01"))?;

        *octet = u8::from_str_radix(part, 16).map_err(|_| format!("{part} is not a hex octet"))?;
    }

    if parts.next().is_some() {
        return Err(format!("{text} is more than six octets"));
    }

    if octets[0] & 1 == 1 {
        return Err(format!(
            "{text} is a multicast address, which an interface cannot be given. The low bit of \
             the first octet has to be clear"
        ));
    }

    Ok(octets)
}

/// Split `10.10.0.1/24` into its parts.
///
/// Parsed when the option is read as well as when the interface is opened, so a
/// typo is a parse error naming the option rather than a failure after the
/// device already exists.
fn cidr4(text: &str) -> Result<(Ipv4Addr, u8), String> {
    let (address, prefix) = text
        .split_once('/')
        .ok_or_else(|| format!("{text} needs a prefix, as in 10.10.0.1/24"))?;

    let address: Ipv4Addr = address
        .parse()
        .map_err(|_| format!("{address} is not an IPv4 address"))?;

    let prefix: u8 = prefix
        .parse()
        .map_err(|_| format!("{prefix} is not a prefix length"))?;

    if prefix > 32 {
        return Err(format!("/{prefix} is longer than an IPv4 address"));
    }

    Ok((address, prefix))
}

/// The same for `fd00::1/64`.
fn cidr6(text: &str) -> Result<(Ipv6Addr, u8), String> {
    let (address, prefix) = text
        .rsplit_once('/')
        .ok_or_else(|| format!("{text} needs a prefix, as in fd00::1/64"))?;

    let address: Ipv6Addr = address
        .parse()
        .map_err(|_| format!("{address} is not an IPv6 address"))?;

    let prefix: u8 = prefix
        .parse()
        .map_err(|_| format!("{prefix} is not a prefix length"))?;

    if prefix > 128 {
        return Err(format!("/{prefix} is longer than an IPv6 address"));
    }

    Ok((address, prefix))
}

/// One open interface, as a message socket.
///
/// [`AsyncDevice`] takes `&self` for both directions, so this needs no lock and
/// no split: the relay's two pumps share it through the `Arc` the enum already
/// holds.
struct Device {
    device: AsyncDevice,
}

impl MessageSocket for Device {
    fn recv<'a>(&'a self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<Option<usize>>> {
        // Always `Some`. An interface has no end of stream to report, the same
        // as any other connectionless transport here: it stops when the relay
        // stops.
        Box::pin(async move { self.device.recv(buf).await.map(Some) })
    }

    fn send<'a>(&'a self, buf: &'a [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(self.device.send(buf))
    }
}

/// A layer 3 interface, carrying IP packets.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default, transparent)]
pub struct Tun(pub Interface);

impl Tun {
    const SCHEME: &'static str = "tun";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        Interface::parse(body, opts, Self::SCHEME).map(Self)
    }

    pub(super) fn label(&self) -> String {
        self.0.label(Self::SCHEME)
    }

    pub(super) async fn connect(&self, buffer: usize) -> anyhow::Result<Connection> {
        self.0.connect(Layer::L3, buffer).await
    }
}

/// A layer 2 interface, carrying Ethernet frames.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default, transparent)]
pub struct Tap(pub Interface);

impl Tap {
    const SCHEME: &'static str = "tap";

    pub(super) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        Interface::parse(body, opts, Self::SCHEME).map(Self)
    }

    pub(super) fn label(&self) -> String {
        self.0.label(Self::SCHEME)
    }

    pub(super) async fn connect(&self, buffer: usize) -> anyhow::Result<Connection> {
        self.0.connect(Layer::L2, buffer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{EndpointSpec, Transport};

    fn tun(spec: &str) -> Tun {
        match spec.parse::<EndpointSpec>().expect("parses").transport {
            Transport::Tun(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn tap(spec: &str) -> Tap {
        match spec.parse::<EndpointSpec>().expect("parses").transport {
            Transport::Tap(e) => e,
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn the_body_is_the_interface_name() {
        assert_eq!(tun("tun:tocat0").0.device.as_deref(), Some("tocat0"));
        assert_eq!(tap("tap:tap0").0.device.as_deref(), Some("tap0"));
    }

    #[test]
    fn an_empty_body_leaves_the_choice_to_the_kernel() {
        assert_eq!(tun("tun:").0.device, None);
    }

    #[test]
    fn a_name_the_kernel_cannot_hold_is_refused() {
        assert!("tun:aaaaaaaaaaaaaaaaaaaa".parse::<EndpointSpec>().is_err());
    }

    #[test]
    fn the_privileged_options_are_off_by_default() {
        let tun = tun("tun:tocat0");

        assert_eq!(tun.0.up, None, "unset must mean touch nothing");
        assert!(!tun.0.packet_info, "matching `ip tuntap add` without `pi`");
        assert_eq!(tun.0.mtu, None);
        assert_eq!(tun.0.ipv4, None);
        assert_eq!(tun.0.ipv6, None);
        assert_eq!(tun.0.mac, None);
    }

    #[test]
    fn the_privileged_options_can_be_asked_for() {
        let tun = tun("tun:tocat0,up,mtu=9000,packet-info");

        assert_eq!(tun.0.up, Some(true));
        assert_eq!(tun.0.mtu, Some(9000));
        assert!(tun.0.packet_info);
    }

    /// A tap gets a random hardware address when it is created, so an interface
    /// tocat creates has a different one every run. Pinning it is the only way
    /// to keep anything that keys on it working across a restart.
    #[test]
    fn a_tap_can_be_given_a_hardware_address() {
        let tap = tap("tap:tap0,mac=02:00:00:00:00:01");

        assert_eq!(tap.0.mac.as_deref(), Some("02:00:00:00:00:01"));
        assert_eq!(mac("02:00:00:00:00:01").unwrap(), [0x02, 0, 0, 0, 0, 0x01]);
        assert_eq!(mac("02-00-00-00-00-01").unwrap(), [0x02, 0, 0, 0, 0, 0x01]);
    }

    /// A tun interface is point to point and has none, so the option would
    /// parse and do nothing, which is the failure mode worth refusing.
    #[test]
    fn a_tun_has_no_hardware_address_to_set() {
        assert!(
            "tun:t0,mac=02:00:00:00:00:01"
                .parse::<EndpointSpec>()
                .is_err()
        );
    }

    #[test]
    fn a_malformed_hardware_address_is_refused_at_parse_time() {
        for bad in [
            "tap:t0,mac=02:00:00:00:00",
            "tap:t0,mac=02:00:00:00:00:01:02",
            "tap:t0,mac=zz:00:00:00:00:01",
            // Multicast. The kernel calls this "cannot assign requested
            // address", which sounds like a conflict rather than a rule.
            "tap:t0,mac=03:00:00:00:00:01",
        ] {
            assert!(bad.parse::<EndpointSpec>().is_err(), "{bad}");
        }
    }

    #[test]
    fn an_address_is_taken_as_a_cidr() {
        let tun = tun("tun:tocat0,ipv4=10.10.0.1/24,ipv6=fd00::1/64");

        assert_eq!(tun.0.ipv4.as_deref(), Some("10.10.0.1/24"));
        assert_eq!(tun.0.ipv6.as_deref(), Some("fd00::1/64"));
        assert_eq!(cidr4("10.10.0.1/24").unwrap().1, 24);
        assert_eq!(cidr6("fd00::1/64").unwrap().1, 64);
    }

    /// Caught when the option is read, so a typo never reaches the point where
    /// the interface has already been created.
    #[test]
    fn a_malformed_address_is_refused_at_parse_time() {
        for bad in [
            "tun:t0,ipv4=10.10.0.1",
            "tun:t0,ipv4=10.10.0.1/33",
            "tun:t0,ipv4=not-an-address/24",
            "tun:t0,ipv6=fd00::1",
            "tun:t0,ipv6=fd00::1/129",
        ] {
            assert!(bad.parse::<EndpointSpec>().is_err(), "{bad}");
        }
    }

    /// An address on a down interface does nothing, so it implies `up`, and
    /// `up=false` has to be able to take that back.
    #[test]
    fn an_address_implies_up_unless_refused() {
        fn resolved(spec: &str) -> bool {
            let i = tun(spec).0;
            i.up.unwrap_or(i.ipv4.is_some() || i.ipv6.is_some())
        }

        // The spellings `Opt::flag` accepts, and no others: it is `parse::<bool>`,
        // so `up=no` is a parse error rather than a false.
        assert!("tun:t0,up=true".parse::<EndpointSpec>().is_ok());
        assert!("tun:t0,up=false".parse::<EndpointSpec>().is_ok());
        assert!("tun:t0,up=no".parse::<EndpointSpec>().is_err());

        assert!(!resolved("tun:t0"));
        assert!(resolved("tun:t0,ipv4=10.10.0.1/24"));
        assert!(resolved("tun:t0,ipv6=fd00::1/64"));
        assert!(!resolved("tun:t0,ipv4=10.10.0.1/24,up=false"));
        assert!(resolved("tun:t0,up"));
    }

    #[test]
    fn an_mtu_larger_than_an_interface_name_field_is_refused() {
        assert!("tun:tocat0,mtu=70000".parse::<EndpointSpec>().is_err());
    }

    #[test]
    fn both_schemes_carry_messages() {
        assert!("tun:tocat0".parse::<EndpointSpec>().unwrap().is_datagram());
        assert!("tap:tap0".parse::<EndpointSpec>().unwrap().is_datagram());
    }

    /// A layer needs boundaries it can keep, and neither of ours does, so the
    /// combination has to fail before anything opens rather than at the first
    /// oversized packet.
    #[test]
    fn a_layer_over_an_interface_is_refused() {
        let spec: EndpointSpec = "tun:tocat0".parse().unwrap();

        assert!(spec.check().is_ok());

        let spec: EndpointSpec = "tap:tap0".parse().unwrap();

        assert!(spec.check().is_ok());
    }

    #[test]
    fn a_label_falls_back_to_the_scheme_and_device() {
        assert_eq!(tun("tun:tocat0").label(), "tun:tocat0");
        assert_eq!(tap("tap:tap0").label(), "tap:tap0");
        assert_eq!(tun("tun:tocat0,name=vpn").label(), "vpn");
        assert_eq!(tun("tun:").label(), "tun");
    }
}
