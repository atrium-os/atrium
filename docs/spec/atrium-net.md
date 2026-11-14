# atrium-net: the host's network configuration daemon

**Status: accepted 2026-11-14** (all §9 decisions as recommended).

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
- **No bouncing between networks.** Auto-join only chooses while not connected; a working
  network is never left for a higher-priority one that appears (the user can switch).
- **Roaming** stays in aura-mlme (W2), with hysteresis: the average signal must be below
  -70 dBm, the other BSS 8 dB better on two scans in a row, and 30 s must have passed since
  the last move (wifi.md §7.3). `atrium-net` treats a roam inside the same
  network as the same link: it keeps the lease and asks atrium-lease to confirm it
  (DHCP INIT-REBOOT) rather than starting over.
- **Country.** The regulatory country (W2) is a setting held here and sent to aura-mlme
  at start; later it can come from the time zone as a hint, never from scan results.

### 4.1 Bad links

How each kind of trouble plays out. "Gate" marks what a simulated gate checks.

- **Short drop (a tunnel, a lift, a blip).** aura-mlme declares the link lost after 8 s
  without beacons (or at once on the firmware's beacon-loss report) and does not try to
  deauthenticate through a dead link. atrium-net keeps the address and routes for 60 s
  (`status` shows `link=lost`); atrium-lease pauses. Reconnect scans after a loss come about
  every 10 s for the first minute (matching the address grace), then 20, 30 and 60 s
  apart, for 10 min; after that the slow scan ladder (30 s up to 5 min) applies. On reconnect atrium-lease confirms the old lease (INIT-REBOOT); the
  same address means nothing changes and TCP connections survive a drop of up to about a
  minute. Not back within 60 s: the address and routes go and apps see the network gone.
  Gate: tunnel scenario in aura-sim-gate W2.
- **Long drop or leaving for good.** As above, then the address goes at 60 s. A user
  `leave` (or `forget`) drops the address at once.
- **Low signal.** Within one network, roaming hysteresis (above). Across networks (built
  with N3): while connected below -70 dBm aura-mlme scans every 10 s anyway and reports,
  after each scan, the best signal per SSID it saw (`nearby ssid= rssi=`, no BSSIDs,
  never our own BSS). atrium-net moves to another remembered auto-join network only when
  ours has been below -80 dBm for 30 s, the other has been above -70 dBm on two scans in a
  row, no other BSS of our own network is above -70 (then aura-mlme roams instead), and
  30 s have passed since the last move. Highest priority wins, then the stronger signal.
  A weak network with nothing better around is kept. A move is a `network-move` event.
- **Flaky internet (link fine, upstream dead or lossy).** The link layer cannot see this.
  DHCP keeps running to the lease's end (renew, then rebind by broadcast at T2, then drop
  and start over). The connectivity probe (§4.2) runs on every bound interface: one whose
  probe fails is reported `internet=no`; when another interface has internet, the default
  route moves to it, and back only after the doubling hold-down of decision 9. With only
  one interface it is reported, never torn down. Gate: n3 dead-upstream scenario.
- **A network that keeps failing** (wrong password, AP refusing us, no association).
  Three failures in a row skip it for 10 min and emit `event what=network-failing`, so
  auto-join tries the next network rather than hammering one. Gate: unit test.
- **Mobile hotspots.** A sleeping or switched-off hotspot is a lost link (as above). An
  Android hotspot marks its DHCP replies with option 43 `ANDROID_METERED`: the lease then
  carries `metered=1`. iPhone hotspots send no marker, so a profile can be marked
  `metered` by the user. Metered is advisory: reported in `status` and lease events for
  apps to defer bulk work; it changes no routing (Ethernet is preferred anyway). Small
  prefixes (an iPhone's /28) are ordinary. A hotspot restarted with a new subnet NAKs the
  old lease and a new one is taken. Gate: metered unit test.
- **Ethernet and Wi-Fi flapping.** Decision 9: doubling hold-down before moving back.
  Gate: n3 cable-pull and flapping scenarios.

### 4.2 Egress and the connectivity probe (N3)

**Egress.** One managed interface carries the IPv4 default route (`egress.rs`, pure and
unit-tested). A candidate is bound (a checked lease with a router) with its link up.
Better means: internet not known to be broken, then wired over Wi-Fi. Losing the current
egress (link down, lease gone) moves at once; moving *to* a better interface waits until
it has been that good for its hold-down: 5 s, doubled for each flap within 2 min of the
last (a flap is its link dropping, or its probe failing while it carried the route), up
to 5 min. Wi-Fi in its 60 s grace keeps the route only when nothing else is up. IPv6
follows: only the egress interface takes default routers from RAs (`ND6_IFF_NO_RADR` on
the others), the router list is flushed on each change and the egress sends fresh router
solicitations. Each change is an `egress iface=..` event on the control socket and, when
atrium-net runs with `--netd-dir`, an `egress-changed iface=..` line on atrium-netd's
`ctl.sock` (§7.2); atrium-netd is still a design (atrium-netd.md), so the gate checks the
line with a stand-in listener.

**Probe.** A plain-HTTP GET to `--probe URL` (off without one: no third party should
learn when the machine joins a network, so the default waits for an Atrium-run endpoint, Shankar's choice 2026-11-14;
`--probe-expect TEXT` takes a 200 with a known body instead of a 204),
per bound interface, after each new lease and then every 5 min (`--probe-every`), 30 s
after a failure. A first failure is retried after 5 s before it counts (a probe sent as a
lease binds can lose its first packets). The name is resolved with that interface's own DHCP DNS server, and
when the interface does not carry the default route atrium-net pins a /32 host route to
the DNS server (if off-link) and to the answer through that interface's router for the
probe's duration, so each interface is judged by its own path. Results: 204 (or the
expected body) is `internet=yes`; any other HTTP answer is `internet=captive` with the
`Location` reported (hex, printable ASCII, at most 256 bytes) as `portal=`; no answer,
NXDOMAIN or a refused connection is `internet=no`. Reported in `status` and as
`internet iface=.. state=..` events. Captive sign-in UX is later (§8).

**Who parses it.** Decision 8 said "through Nuntius"; as built, the probe is its own
sandboxed process instead, `atrium-probe`, because Nuntius is a per-app fetcher started by
portcullisd that cannot be pinned to one interface or ask a given DNS server, and
atrium-net cannot start anything once in capability mode. atrium-net spawns it before
`cap_enter`; it chroots to `/var/empty`, drops to uid `_atrium-probe` (65035), and sets
`RLIMIT_NPROC` 0 and `RLIMIT_NOFILE` 8. It needs to connect, so it is not in capability
mode. It parses one DNS reply and one HTTP head (`probe.rs`, host-fuzzed); atrium-net
checks the address it reports (ordinary unicast, not one of ours) and the Location
before using either. A compromised probe can at worst claim an interface has no internet,
which moves the route between our own already-checked interfaces.

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

Kernel SLAAC (`accept_rtadv`) with stable-private interface IDs (RFC 7217) and
temporary addresses (RFC 8981) preferred for outgoing connections. atrium-lease reads
RAs only for RDNSS (RFC 8106) and proposes the servers; the kernel handles prefixes and
routes. DHCPv6 (stateful) later, if a network needs it.

As built (N2):
- atrium-net sets `use_stableaddr`, `use_tempaddr`, `prefer_tempaddr` and per interface
  `accept_rtadv auto_linklocal stableaddr -ifdisabled`, and sends three router
  solicitations (from `::`, 4 s apart) when a link comes up, so the address does not
  wait for the router's periodic RA. The solicitation is built by atrium-net; nothing in
  it comes from the lease process.
- FreeBSD keys RFC 7217 on `kern.hostuuid` (readable by local users, not a secret, but
  never sent). Atrium's installer should give each install a random hostuuid rather
  than the SMBIOS UUID; until then a local user can predict our stable addresses on a
  known prefix, which only matters for addresses they could look up anyway.
- The bpf filter passes ICMPv6 type 134 with hop limit 255 and no extension headers.
  atrium-lease checks the rest (link-local source, code, checksum, option lengths), keeps
  at most three servers per RA, drops unusable ones (loopback, multicast, IPv4-mapped,
  6to4, Teredo, site-local) and caps lifetimes at 3 h; atrium-net checks the list again
  (`check_dns6`) and gives unbound DHCP's servers and the RA's together, link-local ones
  as `fe80::1%<ifindex>`.
- DNSSL (search domains) is not read: nothing uses search domains.
- Leaving a Wi-Fi network removes its SLAAC addresses at once (the kernel would keep
  them until their lifetimes end).
- Not defended: a rogue router on the link can still announce prefixes, routes and
  usable-looking DNS servers (the kernel and RFC 8106 accept them). RA guard is the
  network's job; on Wi-Fi the network's key already bounds who can send.

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
   Hysteresis (added 2026-11-14, Shankar asked): the default route moves to Wi-Fi as
   soon as the cable's link drops, and back to Ethernet only after the cable has been up
   with a checked lease for 5 s; each flap within 2 min doubles that wait (up to 5 min),
   so a bad cable cannot make the route bounce.

## 10. Build order

| Step | What | Done when |
|---|---|---|
| N0 | `atrium-net` core: interface list, link events, bpf, proposal checks; `atrium-lease` DHCPv4; unbound forwarders | the VM gets its address on a simulated LAN through atrium-net, resolves a name through unbound; a hostile DHCP server never changes host state outside §5.2. **Done 2026-11-14** (§10.1) |
| N1 | Wi-Fi: profiles, auto-join, roam keeps the lease, Transition Disable persisted | `aura-sim-gate.sh w1|w2` pass with atrium-net in place of aura-ctl + dhclient; restart keeps the WPA3-only flag |
| N2 | IPv6: SLAAC, RFC 7217 + 8981, RDNSS | AP VM runs radvd; the VM gets a temporary address, reaches the AP over v6, resolves via the RA's server |
| N3 | Ethernet + Wi-Fi together, egress events to atrium-netd, captive detection | pulling the VM's virtio link moves the default route and NAT to wlan0. **Built 2026-11-14** (§10.4); NAT waits for atrium-netd |
| N4 | `CLASS_NETCONF`, capability checks, a `netctl` CLI, Forum status item | an app without `network_wifi` gets refused; one with it but no location grant gets no list |

N0–N3 need no hardware: the cloud VM (a second virtio NIC on a simulated LAN) and the AP
VM from the Aura gates cover them.

### 10.1 N0 as built (2026-11-14)

- `atrium-net/` workspace: `net-proto` (lease line format and the §5.2 checks),
  `atrium-lease` (DHCPv4 client: RFC 2131 states incl. INIT-REBOOT, renew/rebind/expiry,
  RFC 3442 routes, RFC 7844 option set), `atrium-net` (the daemon). Host tests cover the
  state machine, malformed replies, hostile leases and a 20 000-round mutating-server
  fuzz: `cd atrium-net && cargo test`.
- atrium-lease's sandbox is Capsicum rather than a jail: it chroots to `/var/empty`, drops
  to `_atrium-lease` (uid 65034) and calls `cap_enter` before reading anything, holding
  only its socket. That is stricter than a jail (no files, no new network endpoints).
- atrium-net runs in capability mode after start-up. It reaches unbound's control socket
  with `connectat` on a directory descriptor; addresses go through `SIOCAIFADDR`, routes
  through the routing socket. The default route is changed before it is added: with
  multipath routing on, adding a second default just adds a path.
- Only BOOTREQUESTs for the interface's own MAC leave through bpf, rate-limited, unicast
  only to the applied lease's server (renewals go to the MAC the ACK came from).
- Unbound's forwarders are set at run time (`forward`), so an unbound restart falls back
  to `forward.conf` until the next lease; atrium-net will rewrite it in N1.
- Known gaps: a dead atrium-lease makes atrium-net exit (respawning needs `exec`, which
  capability mode forbids; the service manager restarts it, N1 adds a small spawner);
  no ARP probe before using an address (DHCPDECLINE) yet; no rc/services.d manifest yet.
- Gate: `scripts/atrium-net-gate.sh vm|n0` (RUNBOOK §13.6e), 14 checks. The VM's
  second NIC is a QEMU UDP socket netdev to `atrium-net/sim/lan.py`, a simulated LAN
  with router, DHCP and DNS that can turn hostile (17 kinds of bad reply).

### 10.2 N1 as built (2026-11-14)

- atrium-net takes `--wifi wlan0`: it holds a connection to aura-mlme (reconnecting every
  2 s if aura-mlme restarts), asks aura-keys to remember, prepare and forget credentials
  (a fresh connection per call), and runs the Wi-Fi policy of §4 (`wifi.rs`, host-tested).
  Profiles live in the state directory's `networks` file (one line per network, no
  secret). `atrium-netctl` is the command line: `remember`, `forget`, `list`, `join`,
  `leave`, `scan`, `wifi-status`, `country`, `status`, `watch`.
- The Aura gates (`scripts/aura-sim-gate.sh w1|w2`) run atrium-net in place of aura-ctl +
  dhclient. W2 adds: WPA3-only kept across a restart, auto-join with DHCP and no join
  command, the same address confirmed after a roam, leave drops the address and holds
  off auto-join, a 20 s tunnel (address kept, same address after) and a long drop
  (address gone after the 60 s grace, back on rejoin). W2 38/38, W1 20/20.
- §4.1 behaviour: beacon loss in aura-mlme (8 s), address grace 60 s, reconnect scans
  every ~10 s in the first minute, three failures skip a network for 10 min, DHCP option
  43 `ANDROID_METERED` and the profile's `metered` flag reported as `metered=1`.
- Not yet: unbound's `forward.conf` rewrite, the lease-process spawner, the cross-network
  low-signal move (N3), connectivity probe (N3).

### 10.3 N2 as built (2026-11-14)

- IPv6 as in §5.3: kernel SLAAC with stable-private and temporary addresses, router
  solicitations from atrium-net, RDNSS read by atrium-lease (`ra.rs`, host-tested with a
  20 000-round mutating fuzz and the gate LAN's own RA) and checked again by atrium-net
  (`check_dns6`) before unbound gets DHCP's and the RA's servers together.
- Gate: `scripts/atrium-net-gate.sh n0` now 28 checks, IPv4 and IPv6, including 25 RAs
  from a rogue router (12 kinds) with no bad server reaching unbound.
- Found while testing: a FreeBSD kernel race. Deleting a SLAAC address while RAs for its
  prefix arrive can free the prefix that a newly made address still points to
  (`in6_purgeifaddr` checks `ndpr_addrcnt` without the ND6 lock while `prelist_update`
  increments it); the next RA or NA walks the freed prefix and the kernel panics in
  `pfxlist_onlink_check`. `lan.py flap6` plus address deletions in a loop reproduce it
  in about 15 s. atrium-net turns RA processing off on the interface while it deletes
  addresses, which closes its own window. With that fixed, the same stress found a second
  bug: `nd6_na_input` removes a router whose NA dropped the Router flag while holding the
  neighbor entry's lock, and `defrouter_del` then takes the ND6 lock, the reverse of
  `pfxlist_onlink_check`'s order: the IPv6 stack deadlocks (seen in DDB). Both are fixed
  in a fork patch (address-prefix linkage under the ND6 write lock with a prefix
  reference held across the purge; router removal after the neighbor unlock): 509 SLAAC
  addresses made and deleted under the flood with no panic or hang, 10 minutes clean, the
  gate 28/28 on that kernel. Pushed to atrium-os/freebsd-src branch `atrium-os` as
  92ffd19c3; a private report to the FreeBSD Security Officer is drafted, not sent.
- Not yet: DHCPv6, the AP VM's radvd for IPv6 over Wi-Fi (the code path is the same as
  wired), RA MTU beyond what the kernel applies.

### 10.4 N3 as built (2026-11-14)

- Egress policy and probe as in §4.2: `atrium-net/src/egress.rs` (8 unit tests: cable
  pull, doubling and its 5 min cap and 2 min reset, dead upstream, captive, a lone
  interface, Wi-Fi grace), `atrium-lease/src/probe.rs` + `src/bin/atrium-probe.rs`
  (DNS and HTTP parsing, 20 000-round mutation fuzz each). Leases no longer install the
  default route themselves (`plan.rs`); `route_default()` does, every loop round.
- Gate: `scripts/aura-sim-gate.sh n3` runs atrium-net on `vtnet1` (lan.py) and `wlan0`
  (the AP VM) together. Each network has its own "internet" beyond its router (lan.py
  198.51.100.80, inet.py in the AP VM 192.0.2.80 on its lo), and each network's DNS
  answers `probe.atrium.test` with its own one, so a probe sent out the wrong interface
  fails. The cable is QEMU's `set_link lan off|on` on the monitor. Hold-downs are measured
  from atrium-net's log, which now carries its clock (`[12.3]`). 28 checks; one run's timeline:
  cable pulled, Wi-Fi at once; cable back, Ethernet 5.0 s later; two more quick flaps,
  20 s; Ethernet's upstream dead, Wi-Fi at once; upstream back, 40 s.
- Found by the gate: (1) with multipath routing (on by default) `RTM_CHANGE` of the
  default only edits the path whose gateway it names, and the `RTM_ADD` fallback adds a
  second path, so flows hashed onto the pulled cable; the default is now deleted whole
  (no gateway named) and added again, and the gate checks there is one. (2) A kernel
  panic: aura_wlan changed `wlan0`'s MAC (per-network address) from a `write(2)` on its
  device with no vnet set, and with IPv6 on, the stack's link-address handler
  (`nd6_grand_start`) read `V_` variables through a NULL vnet; `aura_wlan.c` now enters
  the interface's vnet around `if_setlladdr`.
- Cross-network low-signal move (§4.1): `wifi.rs` `weak_scan_done` on aura-mlme's
  `roam-scan`/`nearby`/`roam-scan-done` lines, 6 host tests. Gate scenario `weak_move`
  (end of `n3`, now 31 checks; alone as `aura-sim-gate.sh move`): a second AP `aura-other`
  (`hostapd-other.conf`, channel 1) at -50 dBm; ours at -75 is kept for 90 s, at -85 the
  move came 49 s after the drop (30 s weak plus scans) and took a lease there. W2 38/38. Found while
  building it: after a same-network roam atrium-net never re-confirmed the lease (it now
  treats `disconnected why=roam` as a rejoin and sends LeaseUp on `authorized`), and the
  status line carried `link=` twice in the address grace period (now one: `lost`).
- Not yet: real atrium-netd (NAT re-pointing); the Atrium-run probe endpoint (decided by Shankar
  2026-11-14: probing stays off until Atrium hosts one; no third-party default).
