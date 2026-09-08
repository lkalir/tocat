# `tun` and `tap`

These open one of the kernel's virtual network interfaces and hand the relay its
packets, so you can put an interface on one side of a tocat pipeline and
something else on the other.

| Scheme | Carries         | Layer |
| ------ | --------------- | ----- |
| `tun`  | IP packets      | 3     |
| `tap`  | Ethernet frames | 2     |

```console
$ tocat tun:tocat0 udp:relay.example:5555
```

The body is the interface name. Leave it off and the kernel picks one, which is
then logged so you know what to configure.

## These are message endpoints

One read is one packet and one write is one packet, the same as `udp:`. Three
things follow, and they are the ones that catch people out:

- **A packet longer than the copy buffer is truncated, not split.** tocat reads
  the interface MTU when it opens and refuses to start if it is larger than the
  buffer, naming both numbers, so this fails loudly instead of quietly losing
  the end of every large packet. Raise `--buffer-size` when you raise the MTU.
- **The other side has to keep boundaries too.** Relaying to `udp:` or
  `unix-dgram:` works. Relaying to `tcp:` needs a plugin that frames, or the
  packets run together into a stream nobody can take apart again.
- **Layers are refused.** Neither `tls` nor `noise` keeps message boundaries, so
  a layer over an interface is rejected before anything opens.

## Running without root

Creating an interface needs `CAP_NET_ADMIN`. Attaching to one that already
exists does not, so the usual arrangement is that root makes the interface once
and the relay runs as an ordinary user:

```console
# ip tuntap add mode tap user alice name tap0
# ip addr add 10.99.0.1/24 dev tap0
# ip link set tap0 up
```

```console
$ tocat tap:tap0 udp:peer.example:5555
```

Nothing in that second command needs privilege. tocat does not bring the
interface up, set an address, or change the MTU unless you ask it to, which is
what keeps the unprivileged case working.

Note the lifetimes differ. An interface tocat creates itself is not persistent
and disappears when the relay ends. One made with `ip tuntap add` stays,
together with its addresses and routes, which is usually what you want for
anything that restarts.

## Options

| Option           | Description                                                |
| ---------------- | ---------------------------------------------------------- |
| `ipv4=A.B.C.D/N` | Address and prefix. Needs `CAP_NET_ADMIN`, implies `up`    |
| `ipv6=ADDR/N`    | The same for IPv6                                          |
| `up`             | Bring the interface up. Needs `CAP_NET_ADMIN`              |
| `mac=ADDR`       | Hardware address. `tap` only. Needs `CAP_NET_ADMIN`        |
| `mtu=N`          | Set the MTU. Needs `CAP_NET_ADMIN`                         |
| `packet-info`    | Keep the packet information header in front of each packet |
| `name=`          | Name for this endpoint in logs                             |

Everything but `name` is unset by default, because each of the others needs
privilege the attach case does not have. `packet-info` is off to match
`ip tuntap add` without its `pi` flag, and the two ends have to agree.

`up` has three states rather than two, and the spellings are `up`, `up=true` and
`up=false` as everywhere else in tocat. Leaving it out means tocat does not
touch the interface state at all, which is what lets an attach run unprivileged.
That is not the same as `up=false`, which brings the interface *down* and needs
the same privilege as bringing it up.

## Addressing an interface tocat creates

`ipv4=` and `ipv6=` take a CIDR:

```console
$ tocat tun:t0,ipv4=10.10.0.1/24 udp:peer.example:5555
```

This is mainly for an interface tocat creates rather than attaches to, and the
reason is the lifetime difference above. A created interface does not exist
until tocat opens it and is gone when tocat exits, so there is no moment at
which `ip addr add` could reach it: before is too early, and after is a race
against your own relay. These options are the only way to address one.

For an interface you made with `ip tuntap add`, address it there instead. It
needs privilege either way, and doing it once outside the relay means the
address survives a restart.

Setting an address implies `up`, since an address on a down interface does
nothing. Add `up=false` if you want the address without it.

## Pinning a tap's hardware address

A tap is given a random hardware address when it is created, so an interface
tocat creates has a different one every run. Anything keyed on it stops matching
after a restart: a DHCP reservation, a firewall rule, a bridge forwarding entry,
a peer's ARP cache. `mac=` pins it.

```console
$ tocat tap:tap0,mac=02:00:00:00:00:01,ipv4=10.10.0.1/24 udp:peer.example:5555
```

Either separator works, so `02-00-00-00-00-01` is the same address. The low bit
of the first octet has to be clear, because that bit marks a multicast address
and an interface cannot be given one; tocat says so rather than letting the
kernel report it as "cannot assign requested address", which reads like a
conflict. Setting the second bit, as in the `02:` above, marks the address as
locally administered, which is what you want for one you invented rather than
one from a vendor's range.

`tun:` has no such option. A tun interface is point to point and has no hardware
address, so the option is refused rather than accepted and ignored.

## When it will not open

**"An existing interface of the other layer is rejected by the kernel."** You
pointed `tun:` at a TAP interface or the reverse. The layer is fixed when the
interface is created and cannot be changed by opening it differently.

**A permission error.** Either the interface does not exist and creating it
needs privilege, or you asked for `up` or `mtu=` and those need privilege even
on an interface you can otherwise open. Create it in advance with
`ip tuntap add` and drop those options.

Note that `up` is not a switch between "bring it up" and "bring it down".
Without it tocat does not touch the interface state at all, which is what lets
an attach work unprivileged; an interface someone else already brought up stays
up.

**A packet arrives with four bytes of prefix, or is rejected as malformed.**
`packet-info` disagrees with how the interface was made.

## A worked example

Two hosts, an Ethernet segment bridged over UDP:

```console
# on each host
# ip tuntap add mode tap user alice name tap0
# ip link set tap0 up
```

```console
# alice, on host A
$ tocat tap:tap0 udp:hostB:5555

# alice, on host B
$ tocat tap:tap0 'udp-listen:5555'
```

Frames that reach `tap0` on either host come out of `tap0` on the other. Add
`noise` and it would be refused, because a layer cannot sit on a message
endpoint; encrypt the UDP side instead, or put the `encrypt` plugin in the
pipeline where boundaries are preserved.
