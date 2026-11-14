# atrium-net: the host's network configuration daemon

**Status: proposal, 2026-11-14.** Decisions in §9 await Shankar.

network.md §9 and service-management.md §6 name `atrium-net` as the request-driven mediator
that owns the host's interfaces, addresses, routes, DNS and Wi-Fi association. The daemon
does not exist yet. Wi-Fi (Aura, wifi.md) is built up to W2 with `aura-ctl` standing in
for it and `dhclient` doing DHCP. This spec turns the sketch into something buildable.

## 1. Short answer

- One small privileged daemon, `atrium-net`, decides and applies: interface state,
  addresses, routes, the resolver's upstreams, which Wi-Fi network to join. It parses
  nothing that comes off the wire.
- Everything that parses network input runs jailed with no network and no privilege:
  a Rust DHCPv4 client and a router-advertisement reader, one process per interface
  (`atrium-lease`). It receives raw packets from `atrium-net` over a socketpair and
  returns *proposals*; `atrium-net` checks each proposal before applying it.
- Wi-Fi: `atrium-net` is the client of `aura-mlme`'s control socket that `aura-ctl` is
  today. It keeps the known-network profiles, auto-joins, and persists what W2 left in
  memory (WPA3-only networks after Transition Disable).
- DNS: base FreeBSD `local-unbound` as the system's caching, validating resolver on
  127.0.0.1; `atrium-net` only rewrites its forwarders. Apps keep resolving through the
  Insula broker (`CLASS_NET`), which uses that resolver.
- IPv6: SLAAC with stable-private (RFC 7217) and temporary (RFC 8981) addresses, RDNSS
  from router advertisements. DHCPv6 later.
- Privacy by default: no hostname sent, DHCP client identifier is the per-network MAC
  (RFC 7844 anonymity profile).
- Apps and the future settings UI reach it over a new Aqueduct class, `CLASS_NETCONF`,
  with per-capability grants. Scan results are location data (wifi.md decision 16).

## 2. Domain

`atrium-net` owns (network.md §9.1, unchanged): host interfaces up/down, addresses, the
default route and routes learned from DHCP/RA, `/etc/resolv.conf` and unbound's
forwarders, Wi-Fi association policy, VPN tunnels (later, §8).

It does not own: per-app pf anchors and the app epairs (`atrium-netd`, `jaild`), the
Wi-Fi protocol itself (`aura-mlme`, `aura-keys`), or credentials (Arca).

It tells `atrium-netd` when the egress interface changes, so NAT follows the default
route (network.md §13 item 7).

## 3. Processes and privilege

```
settings UI / apps ──CLASS_NETCONF──► atrium-net (root, host stack, Capsicum after start)
                                        │  routing socket, SIOC* ioctls, bpf per interface
                                        │  writes unbound forwarders + resolv.conf
                       socketpair ◄─────┤─────► atrium-lease@wlan0, @em0 (jailed, no network)
                                        │          DHCPv4 + RA parsing, lease timers
                       mlme.sock  ◄─────┘
                                     aura-mlme / aura-keys (jailed, wifi.md §7.2)
```

- **atrium-net** runs as root because it changes host interfaces; a jail cannot do that
  on the host's stack without being the host. To keep that root small:
  - no wire parsing at all;
  - Capsicum after start-up, holding only its descriptors (routing socket, one ioctl
    socket, a bpf descriptor per interface with a write filter, its state directory,
    the unbound control socket, its listening sockets);
  - every proposal from a lease process is checked: the address is not one of ours
    (loopback, multicast, 100.64.0.0/16 app range, link-local unless asked), the prefix
    is /8–/30, the router is on-link, lease times are clamped, at most 3 DNS servers
    of the right family, and so on (§5.2).
- **atrium-lease** (one per interface, uid `_atrium-lease`, jail with no network, empty
  devfs) holds the DHCP state machine and timers and parses DHCP replies and router
  advertisements. It sees only frames that the bpf filter in `atrium-net` let through
  (UDP 68 to us, ICMPv6 type 134). A compromise yields wrong proposals, which the checks
  in `atrium-net` bound. It never learns the Wi-Fi network's name.
- Wi-Fi keeps its own split (wifi.md §8.3); `atrium-net` gets the same view `aura-ctl`
  has: line protocol over `/atrium/sockets/aura/mlme.sock`, peer checked by uid.

## 4. Wi-Fi policy

- **Profiles.** One per remembered network: SSID, security floor (open / WPA2 / WPA3),
  `wpa3_only` (set by Transition Disable, never cleared automatically), MAC policy
  (per-network default, random, device), auto-join on/off, priority, metered, hidden.
  Stored in `/var/db/atrium-net/networks.toml`, root 0600. The secret stays in Arca's
  machine store under aura-keys (wifi.md §7.2); the profile holds no secret.
- **Auto-join.** At start, on link loss and after leaving: scan, take remembered
  networks that are present, highest priority first, then strongest. Never join open
  networks unless that profile has auto-join explicitly on (decision 17); never below
  the profile's security floor (aura-mlme enforces this too).
- **Roaming** stays in aura-mlme (W2). `atrium-net` treats a roam inside the same
  network as the same link: it keeps the lease and asks atrium-lease to confirm it
  (DHCP INIT-REBOOT) rather than starting over.
- **Country.** The regulatory country (W2) is a setting held here and sent to aura-mlme
  at start; later it can come from the time zone as a hint, never from scan results.

## 5. Addresses

### 5.1 IPv4

DHCPv4 in atrium-lease: DISCOVER/OFFER/REQUEST/ACK, renew at T1, rebind at T2, NAK and
expiry handling, INIT-REBOOT after a roam or link flap. Options used: subnet, router,
DNS, lease times, MTU (clamped 1280–9000), classless static routes (RFC 3442, checked
like the router). Options sent: none that identify the machine. Static addresses are a
profile setting applied directly by `atrium-net`.

### 5.2 Proposal checks (in atrium-net)

A proposal is applied only if: the address is unicast and not in 0/8, 127/8, 169.254/16
(unless DHCP failed and link-local is enabled), 224/4, 100.64.0.0/16 or another
interface's subnet; the router is inside the subnet or an explicit on-link route; at most
16 routes, none covering the app range or 127/8; DNS servers unicast and at most 3;
lease time at least 60 s. Anything else is logged and dropped, and the interface stays
on its previous good state.

### 5.3 IPv6

Kernel SLAAC (`accept_rtadv`) with stable-private interface IDs (RFC 7217, keyed by the
same machine secret as the per-app MACs) and temporary addresses (RFC 8981) preferred
for outgoing connections. atrium-lease reads RAs only for RDNSS/DNSSL (RFC 8106) and
proposes them; the kernel handles prefixes and routes. DHCPv6 (stateful) later, if a
network needs it.

## 6. DNS

- `local-unbound` from base listens on 127.0.0.1 and ::1, validates DNSSEC where the
  upstream allows, caches. `atrium-net` sets its forwarders from the active interface's
  DHCP/RA servers through `unbound-control` (no config rewrite, no restart).
- `/etc/resolv.conf` is fixed: `nameserver 127.0.0.1`. Nothing else writes it.
- A user setting can replace the network's servers (and later turn on DoT upstreams,
  which unbound supports).
- The Insula broker and Nuntius already resolve on the host; they now get the cache and
  validation for free.

## 7. Interfaces to others

### 7.1 CLASS_NETCONF (new Aqueduct class 18)

`CLASS_NET` (12) is the Insula connect broker, so configuration gets its own class.

| Op | Capability |
|---|---|
| `Status` (online/offline, metered, which kind of link; no addresses) | `network_read` |
| `ListInterfaces`, `GetRoutes` (addresses, routes) | `network_details` |
| `ListWifiNetworks` (SSID, security, signal bucket; never BSSIDs) | `network_wifi` + user grant (location) |
| `JoinWifi`, `ForgetWifi`, `SetProfile` | `network_wifi` |
| `SetStatic`, `SetDns`, `SetCountry` | `network_configure` |
| `Subscribe` (status changes) | `network_read` |

Passwords pass through `atrium-net` to aura-keys' `remember` and are not kept. The
Forum status item (online, Wi-Fi bars) uses `Status` + `Subscribe`.

### 7.2 atrium-netd

One event, `egress-changed {iface}`, over atrium-netd's existing control socket; it
re-points the NAT rule. If atrium-netd is not running, nothing changes for apps
(fail-closed already holds).

### 7.3 aura-ctl

Stays as a test and debug tool. The gates switch to `atrium-net` once N1 lands.

## 8. Not in v1

VPN tunnels (WireGuard first, later), DHCPv6, captive portal sign-in (v1 only detects
it, §9 decision 8), Wi-Fi hotspot, bridges, 802.1X on Ethernet, mDNS.

## 9. Decisions

1. **One privileged daemon that parses nothing, plus a jailed `atrium-lease` per
   interface for DHCP and RA.** This closes wifi.md decision 15. *Recommend yes.*
   Alternative: base `dhclient` (privsep, C) until later.
2. **Own Rust DHCPv4 client** rather than base `dhclient`. *Recommend yes*: dhclient's
   unprivileged half still parses in C, and its script hook runs `ifconfig` as root
   from parsed options.
3. **`local-unbound` from base as the resolver**, with forwarders set by `atrium-net`.
   *Recommend yes.* Alternative: own Rust resolver (much larger, no gain now).
4. **IPv6 in v1** with SLAAC, stable-private plus temporary addresses and RDNSS.
   *Recommend yes*; DHCPv6 later.
5. **Privacy defaults:** no hostname sent, client ID = the per-network MAC, RFC 7844
   option set. *Recommend yes.*
6. **Profiles in `/var/db/atrium-net/networks.toml` (root 0600)**, secrets stay in Arca.
   *Recommend yes.* Alternative: profiles in Arca too (it holds secrets, not settings).
7. **New Aqueduct class `CLASS_NETCONF` = 18** with the capabilities in §7.1, which
   replace network.md §9.2's sketch. *Recommend yes.*
8. **Captive portals: detect and notify in v1** (a plain-HTTP probe through Nuntius after
   each join); sign-in UX later. *Recommend yes.*
9. **Ethernet preferred over Wi-Fi** when both are up (lower route metric), and Wi-Fi
   stays associated as a standby. *Recommend yes.*

## 10. Build order

| Step | What | Done when |
|---|---|---|
| N0 | `atrium-net` core: interface list, link events, bpf, proposal checks; `atrium-lease` DHCPv4; unbound forwarders | arm64 cloud VM gets its address from QEMU's DHCP through atrium-net, resolves a name through unbound; a fuzzed hostile DHCP server never changes host state outside §5.2 |
| N1 | Wi-Fi: profiles, auto-join, roam keeps the lease, Transition Disable persisted | `aura-sim-gate.sh w1|w2` pass with atrium-net in place of aura-ctl + dhclient; restart keeps the WPA3-only flag |
| N2 | IPv6: SLAAC, RFC 7217 + 8981, RDNSS | AP VM runs radvd; the VM gets a temporary address, reaches the AP over v6, resolves via the RA's server |
| N3 | Ethernet + Wi-Fi together, egress events to atrium-netd, captive detection | pulling the VM's virtio link moves the default route and NAT to wlan0 |
| N4 | `CLASS_NETCONF`, capability checks, a `netctl` CLI, Forum status item | an app without `network_wifi` gets refused; one with it but no location grant gets no list |

N0–N3 need no hardware: the cloud VM (virtio-net, QEMU user DHCP) and the AP VM from the
Aura gates cover them.
