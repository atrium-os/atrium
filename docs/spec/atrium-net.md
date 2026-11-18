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
  from router advertisements; DHCPv6 for DNS servers and addresses where the RA asks
  for it (§5.3).
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
`cap_enter`; it chroots to `/var/empty`, drops to uid `_atrium-probe` (65048), and sets
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

As built (2026-11-15): a fixed setting (`Manual`: address/prefix, optional router, up to 3
DNS servers) belongs to a saved Wi-Fi network (stored with its profile, `maddr=`,
`mrouter=`, `mdns=`) or to a wired port by name (the `settings` file in the state dir).
`atrium-net` turns it into a lease of its own (no server, never expires) that goes
through the same checks (§5.2) as a DHCP one, and atrium-lease is told `down` for that
interface; DHCP answers are ignored while it applies. A refused fixed setting is logged
and counted as a refusal; it is not sent back to DHCP silently. Changing the setting of
the network in use applies it at once (fixed to fixed, DHCP to fixed, fixed back to
DHCP with a fresh DISCOVER). `Manual::check` (in `atrium-netconf`) refuses a prefix
outside 8–30, the network or broadcast address, a router outside the subnet or equal to
the address.

Address conflicts (RFC 5227, built 2026-11-17): before using a new address (an ACK
after DISCOVER/OFFER/REQUEST; not a reboot or renewal of one already used), atrium-lease
asks for three ARP probes 600-900 ms apart and waits as long again, about 2-3 s in all
(the RFC asks 4-7 s; a host that has the address answers within milliseconds). The bpf
filter passes ARP to atrium-lease, which reads it; atrium-net only sends the probes it
builds itself from an address (`probe addr=`, ordinary unicast only, DHCP token bucket),
sender address 0.0.0.0. Any ARP from another MAC that has the address, or that probes for
it too, is a conflict: DHCPDECLINE, and DISCOVER again after 10 s (RFC 2131 §3.1). Once
bound, defending the address is the kernel's. Gate: `lan.py conflict` (a host that has
10.88.0.50) in `atrium-net-gate.sh n0`.

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
routes. DHCPv6 where the RA's flags ask for it (built 2026-11-15, below).

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
- A user setting can replace the network's servers, and ask them over TLS. As built (2026-11-15): `SetDns` (up to 3 unicast IPv4/IPv6
  servers, not loopback) is kept in the state dir's `settings` file; while set, unbound
  forwards to those alone, and an empty list goes back to each network's. `GetRoutes`
  returns it as `own_dns`.
- **DNS over TLS** (RFC 7858, built 2026-11-18, Shankar's pick): `SET_DNS_TLS` (op 0x43,
  `network-configure`) takes the own servers and the host name their certificate must
  show (`dns.quad9.net`). unbound then asks them only on port 853 over TLS
  (`forward_add +t . A@853#NAME`), checking the chain against
  `/var/unbound/atrium-ca.pem` (a copy of `/etc/ssl/cert.pem` inside unbound's chroot,
  made by `atrium-net-service.sh` with `conf.d/atrium-tls.conf`) and the name. Strict:
  a wrong certificate or a blocked port 853 gives SERVFAIL, never plain DNS (a captive
  portal must be signed into with it off). Without a bundle unbound would take any
  certificate, so atrium-net asks `get_option tls-cert-bundle` and refuses
  ("no-tls-bundle"). Kept in `settings` (`dns servers=.. tls=NAME`), shown as
  `dns own=.. tls=NAME` and `Routes.dns_tls`; `SET_DNS` turns it off. A restarted
  unbound is told again (its forwarders differ, or its uptime went down: list_forwards
  can't show TLS). `atrium-netctl dns A,B tls NAME`. Gate `atrium-net-gate.sh n0`
  61/61: a second unbound on 192.0.2.53:853 (lo1) with a test CA answers; nothing
  reaches the LAN's DNS; refused without a bundle; a wrong name SERVFAILs; back after an
  unbound restart; off again. Curia's DNS servers row has the name field (curia.md §3).
- The Insula broker and Nuntius already resolve on the host; they now get the cache and
  validation for free.

### 6.1 `.local` names (accepted 2026-11-18)

So that `ssh nas.local`, a printer's or another machine's `.local` name resolves like any
other name, through the same resolver.

- **Path:** unbound gets a stub zone for `local.` pointing at `127.0.0.1@5354`
  (`/var/unbound/conf.d/atrium-local.conf`: `stub-zone`, `domain-insecure: "local."`,
  `do-not-query-localhost: no`). Everything else keeps forwarding as now; unbound caches
  the answers.
- **`atrium-mdns`** answers that stub. One process, uid `_atrium-mdns` (65049), chroot
  `/var/empty`, capability mode, started by atrium-net's spawner like `atrium-lease`. It
  holds three descriptors, all made in the spawner and handed over its fd 3: a TCP socket
  listening on 127.0.0.1:5354 (unbound talks TCP to it: `stub-tcp-upstream`, since capability
  mode forbids `sendto` to an address) and a receive/send pair like §7.1's, but on a port of
  its own rather than 5353: one-shot queries (RFC 6762 §5.1) whose answers come back unicast.
  Two sockets connected alike on 5353 are refused on FreeBSD, and a unicast datagram to 5353
  reaches only one socket, so printer discovery keeps 5353 to itself.
  It can't open others and can't send anywhere but 224.0.0.251:5353 and back to unbound.
  It is started again with new sockets when the egress changes, and not run when there is
  none (or only a VPN tunnel): then `.local` gets SERVFAIL.
- **What it answers:** A and AAAA queries for names of one to
  four labels under `local.`, nothing else (no PTR, no SRV: printers have prelum-browse).
  One query per question, answers collected for 1 s, at most 8 queries in flight and
  a negative-cache of 10 s per name so a loop in some program can't flood the LAN.
- **Checks on answers** (in `atrium-mdns`, the only parser of them): the record's name is
  the question's; addresses must be on the active interface's subnet or link-local
  (169.254/16), so a `.local` answer can never point a program at the internet or at
  loopback; at most 4 addresses; TTL clamped to 120 s. Bounded parser, fuzzed, as
  prelum-dnssd.
- **Privacy:** Atrium still announces nothing and answers no mDNS queries (other machines
  can't find it by name); asking leaks the name asked for to the local link only.
- **Build:** L1 `atrium-mdns` + its parser and checks with host tests; L2 spawner,
  restart on egress change, unbound stub, gate in `atrium-net-gate.sh` (lan.py answers
  `gatebox.local`, hostile answers refused); L3 AAAA over ff02::fb.
- **As built (2026-11-18), L1 + L2:** crate `atrium-net/atrium-mdns` (lib: query, answer
  checks, resolver; 7 host tests incl. a fuzz loop), spawner requests `b'S'` (the stub
  listener, made once at start) and `b'U'` (the one-shot pair), atrium-net runs one
  atrium-mdns per active network (`mdns=1` on its status line; a dead one is started
  again after 5 s; `--no-mdns`, and none without unbound or when the binary is missing).
  `atrium-net/etc/unbound-atrium-local.conf` is installed by `atrium-net-service.sh`.
  Gates: `atrium-net-gate.sh n0` 53/53 (gatebox.local through unbound and one mDNS
  question, the cache, evil/loopy/nothere.local NXDOMAIN, getent, kill and restart),
  `aura-sim-gate.sh svc` 23/23 (the installed service resolves `.local`).
- **As built (2026-11-18), L3:** AAAA questions go over the same IPv4 one-shot pair, not
  ff02::fb: one socket pair, and hosts answer AAAA over IPv4 alike. An AAAA answer counts
  only inside one of the interface's own /64s (global or unique-local, from SLAAC or
  DHCPv6; not the prefix's anycast address, never loopback or fe80::/10, which a DNS
  answer can't give a zone). atrium-net passes the /64s on the start line (`v6=P/64,..`)
  and starts atrium-mdns again when they change; with none, AAAA is answered at once
  with no data. An AAAA nobody answers is NOERROR/no data (the name may have an IPv4
  address), not NXDOMAIN. 9 host tests. Gate `atrium-net-gate.sh n0` 55/55
  (gatebox.local AAAA fd77:0:0:88::77; evil.local's ::1, 2001:db8::1, fe80::1 and anycast
  refused).

## 7. Interfaces to others


DHCPv6 as built (2026-11-15):
- atrium-lease reads the RA's M and O flags. O alone: an Information-Request, and the
  Reply's DNS servers (option 23, at most 3, ordinary unicast only) join the RA's in the
  `dns6` line (RA servers first, three in all); asked again at the information refresh
  time, 10 min to 1 day. M: Solicit, the first usable Advertise, Request; Renew at T1,
  Rebind at T2, the address dropped when its valid lifetime ends or the server refuses
  it. IA_NA only: no prefix delegation, rapid commit, reconfigure or authentication. The
  client's DUID is DUID-LL of the interface's MAC, so Wi-Fi's per-network address gives
  a per-network DUID. Lifetimes are capped at a week; T1/T2 of 0 become 0.5 and 0.8 of
  the preferred lifetime; Solicit and Information-Request back off to 120 s.
- atrium-lease proposes `send6` payloads; atrium-net checks them (`wrap::check_send6`:
  a client message type, only the options the client builds, exactly one client id and
  it is this interface's DUID, at most 256 bytes; the DHCPv4 token bucket) and sends
  them from the interface's link-local address to ff02::1:2. The bpf filter passes UDP
  547 to 546; atrium-lease takes only frames to a link-local address with a valid UDP
  checksum. `lease6` is checked again (`check_lease6`: global or unique-local unicast,
  not another interface's address, valid at most a week, preferred within valid) and
  applied as a /128 with its lifetimes (`SIOCAIFADDR_IN6`); on-link prefixes and routes
  still come from RAs. `lost6`, link down and leaving a Wi-Fi network remove it.
- Gate: `atrium-net-gate.sh n0` 35/35, with `lan.py d6-info`, `d6-addr` and `d6-hostile`
  (17 kinds of bad message, pushed with the client's own transaction id: 62 sent, none
  applied).
- Found on the way: getifaddrs(3) needs a sysctl that capability mode refuses, so after
  start-up atrium-net could not list the host's addresses (the status line had no
  `inet6`, leaving a Wi-Fi network did not remove its SLAAC addresses, and the overlap
  check saw only the addresses from start-up). It now reads them once at start and then
  keeps them from the routing socket's RTM_NEWADDR/RTM_DELADDR messages.
- Fixed 2026-11-17: no RFC 8981 temporary address on the first gate run after a boot. An
  RA handled before atrium-net starts (here rc.conf's `ifconfig_DEFAULT="... inet6
  accept_rtadv"`, and the gate LAN's first RA) makes a stable SLAAC address while
  `use_tempaddr` is still 0, and the kernel adds a temporary address only when it makes a
  new stable one (`prelist_update`), so none ever came. At start atrium-net now removes,
  per /64, SLAAC addresses that have no temporary partner (flags by `SIOCGIFAFLAG_IN6`,
  RAs off while deleting); the next RA makes both. A restart leaves prefixes that have
  one alone.

### 7.1 CLASS_NETCONF (new Aqueduct class 18)

`CLASS_NET` (12) is the Insula connect broker, so configuration gets its own class.

| Op | Capability |
|---|---|
| `Status` (online/offline, metered, which kind of link; no addresses) | `network_read` |
| `ListInterfaces`, `GetRoutes` (addresses, routes) | `network_details` |
| `ListWifiNetworks` (SSID, security, signal bucket; never BSSIDs) | `network_wifi` + user grant (location) |
| `JoinWifi`, `ForgetWifi`, `SetProfile` | `network_wifi` |
| `SetStatic`, `SetDns`, `SetDnsTls`, `SetCountry` | `network_configure` |
| `Subscribe` (status changes) | `network_read` |

Passwords pass through `atrium-net` to aura-keys' `remember` and are not kept. The
Forum status item (online, Wi-Fi bars) uses `Status` + `Subscribe`.

As built (N4, §10.5): manifest keys `network-read`, `network-details`, `network-wifi`,
`network-configure`, and `wifi-nearby` for the list. The "user grant (location)" is the
install consent line `wifi-nearby` adds ("See the Wi-Fi networks around you (this can
tell where you are)"), so the list is a capability of its own rather than a second
check on `network-wifi`; there is no separate grant store. `network-configure` is
restricted (settings app only). Apps never reach atrium-net itself: `atrium-netconf`
serves the five sockets.

Printer discovery (2026-11-18, for printing.md §7.1): a sixth socket, `print-discovery`
(directory 0700, socket 0600, peer must be root: prelumd only, never granted to an
app), with one op, `PRINT_DISCOVERY` (0x50). Its answer is the active interface's name
with two descriptors, in this order: **receive** (bound 0.0.0.0:5353 with SO_REUSEPORT,
joined to 224.0.0.251 on that interface, sending side shut down) and **send** (bound
5353, `IP_MULTICAST_IF` that interface, TTL 255, no loopback, connected to
224.0.0.251:5353, so it reaches nothing else: FreeBSD refuses `sendto` elsewhere with
EISCONN). A worker sends QU queries on the send socket and reads unicast and multicast
answers on the receive one, without leaving capability mode. No interface with an IPv4
address: `FAILED` "no-network". atrium-net cannot bind in capability mode, so its
spawner (§10.1) makes the pair (`b'M'` + the interface's address) and atrium-net
passes them up its control socket (`mdns4`) with SCM_RIGHTS. Client:
`atrium_netconf::client::Client::print_discovery()`; check tool `atrium-netctl --app
print-discovery printers SECS`. IPv6 (ff02::fb) can follow as an optional second pair.

### 7.2 atrium-netd

One event, `egress-changed {iface}`, over atrium-netd's existing control socket; it
re-points the NAT rule. If atrium-netd is not running, nothing changes for apps
(fail-closed already holds).

As built (2026-11-15), without atrium-netd: apps' NAT names an interface group,
`nat on atrium_egress inet from 100.64.0.0/16 to any -> (atrium_egress:0)`
(etc/atrium.pf), and atrium-net keeps the egress interface alone in that group
(`SIOCAIFGROUP`/`SIOCDIFGROUP` on its ioctl socket, in capability mode): at start, and
whenever the egress changes, every other interface there was at start is taken out.
pf follows group membership with no reload, so apps move with the default route.
States made through the old interface die with its address, as for the host's own
connections. With no egress the group is empty and apps have no way out. Until
atrium-net runs, the install scripts put the default-route interface in the group (now
and at every boot, `/etc/start_if.<if>`). `--egress-group NAME` / `--no-egress-group`.
The `egress-changed` line to atrium-netd stays for when atrium-netd exists. Gate:
`aura-sim-gate.sh nat`.

### 7.3 aura-ctl

Stays as a test and debug tool. The gates switch to `atrium-net` once N1 lands.

## 8. Not in v1

VPN tunnels (designed in [vpn.md](vpn.md): atrium-net applies checked tunnel proposals and owns the `atrium/vpn` pf anchor), DHCPv6 prefix delegation, captive portal sign-in (v1 only detects
it, §9 decision 8; proposed in §8.1), Wi-Fi hotspot, bridges, 802.1X on Ethernet, mDNS.

### 8.1 Captive portal sign-in (accepted 2026-11-18, P1–P4 as recommended)

Hotels, trains and cafés hold traffic until a web page is accepted. v1 detects that
(`internet=captive`, `portal=`) and the bar says "sign-in needed"; nothing opens the page.
It matters more now: with DNS over TLS on (§6) nothing resolves behind a portal, and
with a VPN's block-when-not-connected (vpn.md §5.4) nothing leaves at all.

**What the user sees.** When the egress interface turns captive, one notification
"Sign in to <network>" (and the same line in the bar's network menu and Curia's Network
page). Pressing it opens a **portal window**: a Navigator Unrecorded trail (throwaway
jar, nothing kept) at the probe's `Location`, or, with none, at plain HTTP to the
network's router (`http://<router>/`): portals catch any plain-HTTP request, and this one
tells no third party anything. The window title says it is the network's page, not a site's.
When the probe says `internet=yes` the window closes itself and a notice says
"Connected to <network>". Closing it early just stops the fast re-probe.

**What is different about that one window** (and nothing else):
- **DNS:** it asks the network's own DHCP/RA servers directly, not unbound: portals
  answer every name with their own address, and the user's own or encrypted servers
  are unreachable until sign-in. Its Nuntius instance is started with those servers
  as its resolvers (a new launch option, `ATRIUM_NUNTIUS_RESOLVERS`, set only by
  portcullisd for a portal window) instead of `/etc/resolv.conf`.
- **Addresses:** the private-network grant, for that instance only: most portals
  are on 10/8, 172.16/12 or 192.168/16. Loopback and the host's own addresses stay
  refused.
- **Path:** out the captive interface (its FIB when the VPN's app scope uses FIBs),
  and with block-when-not-connected on, it is the one tagged exception the VPN spec
  already reserves (vpn.md §6, captive portal row). It carries no cookies, no
  passwords from Arca, no persona: the portal never learns who the user is beyond what
  they type.
- **Lifetime:** the window, its jar and the exception end when it closes, when the
  probe says online, or after 10 minutes.

**atrium-net's part (S1).** A new `network-portal` capability (its own socket, granted
only to the Navigator's portal launcher in portcullisd, never to apps) with
`PORTAL_INFO` → {interface, network name, portal URL, the interface's DNS servers,
router} and `PORTAL_WATCH on|off`: while on, the egress interface is re-probed every
5 s instead of every 5 min, and the `internet` event ends the flow.

**The catch: detection needs the probe, and the probe is off** (§4.2: no third party
learns when the machine joins a network, until Atrium runs an endpoint). Without a
probe nothing turns captive, so nothing offers sign-in. Until the endpoint exists,
"Sign in to this network" is offered by hand: in the bar's network menu whenever the
link is up, and automatically when DNS over TLS has failed for 10 s on a new network
(a strong hint of a portal, learned without contacting anyone).

**Build:** S1 atrium-net `PORTAL_INFO`/`PORTAL_WATCH` + lan.py portal mode (DNS answers
everything with 10.88.0.1, HTTP 302 to a sign-in page, a POST lets the client through);
S2 Nuntius `ATRIUM_NUNTIUS_RESOLVERS` and the per-instance private grant; S3 Navigator
portal window (Navigator thread); S4 bar/Curia/notification entry points; gate: join
the portal LAN with DNS over TLS on, open the window, accept, online, window closed,
DNS over TLS working again.

*Built 2026-11-18:* S1 (the root-only `network-portal` socket, `PORTAL_INFO`,
`PORTAL_WATCH`, `atrium-netctl portal [watch on|off]`; gate n0 checks the portal LAN's
302, the socket's permissions, refusal to another user, 5 s probes while watched and the
watch ending once online) and S2 (Nuntius resolvers, portcullisd `OpenPortalFetcher`
gated on the `network-portal` manifest capability, `nuntius::launch::open_portal`).
S4 entry points built the same day: "Sign in to this network…" in the bar's network
menu and "Sign in to network" on Settings' Network page (primary when the probe saw a
portal), both whenever a link is up, both asking portcullisd's `PortalWindow` (Settings
with the press, so an open window comes forward). `network-wifi` now reaches
portcullisd's socket for this. lan.py's router serves the sign-in page (GET /login, one
form; POST /accept lets the machine through), and gate n0 (70/70) sends that form.
Left: the automatic offer when DNS over TLS fails on a new network (P3), and S3.

*S3, the window's shape (Navigator thread, 2026-11-18):* the Navigator runs one window per
instance and `Launch` passes no arguments, so the sign-in window is its own Navigator
instance in a reserved persona, `portal` (portcullis-peer `PORTAL_PERSONA`; no person's
persona may take the name): its own uid, jail and overlay, no trails written, no folders.
portcullisd's `PortalWindow { activation }` (for S4's bar, Curia and notice) starts it, or
brings it forward on a press; the instance alone may ask `PortalStatus` (→ network name,
start URL, online) and `OpenPortalFetcher`. Nuntius gained `Method::Form`: a form's POST
whose 301/302/303 answer is followed as a GET without the body, as a browser does; plain
`Post` still never follows.

*S3 built (Navigator thread, 2026-11-18):* the window reads the page's plain forms in its page
jail and sends one only on a press of its button (navigator-ui.md §7 "Forms in the Wi-Fi
sign-in window"), polls `PortalStatus` every 5 s and closes when online or after ten minutes.
Workers' environments carry `ATRIUM_` keys only, so the private grant is
`ATRIUM_NUNTIUS_PRIVATE_NETWORK=1`. In the sandbox casper connects only to addresses it
resolved (`CAPNET_CONNECTDNS`), so `ViaDns` has casper resolve each address numerically before
connecting. The confined VM gate runs it against `portal_standin` (a netconf stand-in: "Hotel
Guest", the fixture server's /portal/, online once a file exists), and with `PORTAL_LAN=1` on
lan.py's portal LAN with the real atrium-net, probe and atrium-netconf (141/141, 2026-11-18).
That run found two more: nuntiusd gave up when one of the network's DNS servers was
unreachable from its jail (an IPv6 one; the routed jail has IPv4 only), and now passes over
it; and the fetcher wrote each request in pieces, which lan.py's router took for a request
cut short, and now writes it at once. A wired network has no name, so the window says "Sign
in to the wired network".

P5 (Shankar, 2026-11-18): the sign-in window may send the page's plain HTML forms (text,
checkboxes, buttons), only on the person's press, and only in that window. Everywhere
else the Navigator still sends no form (legacy-web §5.4.1e).

**Decisions for Shankar**
- P1. A portal window as an Unrecorded Navigator trail with the network's DNS and the
  private-network grant, nothing else changed. *Recommend yes.*
- P2. `network-portal` as its own capability, held only by the portal launcher.
  *Recommend yes.*
- P3. Without a probe: a manual "Sign in to this network", plus the automatic offer
  when DNS over TLS fails on a new network. *Recommend yes.* Alternative: turn the
  probe on against a third party (rejected on 2026-11-14).
- P4. The window closes itself when online, and after 10 minutes at most.
  *Recommend yes.*

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
| N4 | `CLASS_NETCONF`, capability checks, a `netctl` CLI, Forum status item | an app without `network_wifi` gets refused; one with it but no location grant gets no list. **Built 2026-11-14** (§10.5) |

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
- Unbound's forwarders are set at run time (`forward`). An unbound restart falls back
  to `forward.conf`; since 2026-11-17 atrium-net reads `list_forwards` every 10 s and
  sets the network's servers again when they differ (no file is written, so a crash of
  atrium-net never leaves stale servers on disk).
- Known gaps then: a dead atrium-lease made atrium-net exit (respawning needs `exec`,
  which capability mode forbids). Fixed 2026-11-17: atrium-net forks a spawner first
  thing, before it opens anything (`spawn.rs`). The spawner reads one byte per request
  (which of the two programs fixed at fork time, atrium-lease or atrium-probe), starts
  it with a fresh socketpair as its fd 3 and nothing else, and hands atrium-net its end
  over SCM_RIGHTS; the kernel reaps the children. A parser that exits, sends a bad line
  or an unexpected message is replaced: `hello` and, if the link is up on DHCP, `up`; the
  applied lease stays until the new parser's. More than five deaths in a minute hold
  that interface's parser off for a minute. A dead probe helper is started again after
  10 s. `status` shows `lease_restarts`. The spawner shows in `pgrep -x atrium-net` as a
  second process (it exits with atrium-net);
  ARP probe before using an address: built 2026-11-17 (below); no rc/services.d manifest yet
  (rc.d services since, §10.6).
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
- The cross-network low-signal move and the connectivity probe came in N3 (§10.4).

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
- IPv6 over Wi-Fi checked 2026-11-17 (`aura-sim-gate.sh wifi6`, 11/11): the AP VM's
  `ra6.py` advertises fd77:0:0:77::/64 and RDNSS on br0; RAs arrive as group frames
  under the GTK; SLAAC stable and temporary addresses, the AP as default router, ping6
  both ways, `dns6=` from RDNSS, and the addresses gone when we leave. MTU since 2026-11-18: a
  network's MTU no longer outlives it. atrium-net puts the port back to 1500 when the
  lease that set an MTU is lost, when a renewal stops carrying one, and on the first
  lease after a restart (`ResetMtu`, only if it differs). The RA's MTU is the kernel's
  (`nd6_ra_opt_mtu`: 1280 up to the port's MTU) and shows as `mtu6=` beside `mtu=` on
  the status line. Found: the kernel keeps an RA's lowered link MTU after the network
  is gone and refuses to raise it from userland (`SIOCSIFINFO_IN6` checks against
  `in6_ifmtu`, which is the lowered value), so only a later RA with an MTU option
  raises it; most routers send one. Lower is only slower, never broken, so this is
  left to the kernel; a fork fix would compare against the port's MTU instead. Gate
  `atrium-net-gate.sh n0` 63/63 (MTU 1400 by DHCP and RA applied and shown; back to
  1500 after a restart on a network without one).

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
- Not yet: the Atrium-run probe endpoint (decided by Shankar 2026-11-14: probing stays
  off until Atrium hosts one; no third-party default). NAT follows the egress since
  2026-11-15 (§7.2).

### 10.5 N4 as built (2026-11-14)

- `atrium-net/netconf` (crate `atrium-netconf`): the `CLASS_NETCONF` wire types
  (postcard; `Status`, `Iface`, `Routes`, `Profile`, `Join`, `Nearby`), the client, the
  per-socket rules (`Grant::allows`), and `server::serve`. `lines.rs` turns atrium-net's
  control lines into those types (BSSIDs and channels never cross). Host tests over real
  sockets with a fake backend: what each socket allows, status names hidden from
  `network-read`, subscribe sends only changes.
- `atrium-netconf` (binary): one socket per capability under `/atrium/sockets`; two
  connections to atrium-net's control socket (requests; events), opened as root, then
  setuid to `_atrium-netconf` (65036) and `cap_enter`. App input is decoded there, not in
  the root daemon. It exits if atrium-net goes away.
- atrium-net: `wifi=1` on status lines; `wifi-remember` without a secret changes an
  already-remembered network's settings. aura-mlme: `rssi` in `status` while connected
  (bars), and a join while a scan runs uses that scan (the radio refused a second one
  with EBUSY: remember-then-join raced auto-join's scan).
- Portcullis: the five capabilities mount their socket dirs; consent lines;
  `network-configure` warns as restricted.
- `atrium-netctl --app CAP ...`: the CLASS_NETCONF client as an app with one capability.
- Forum bar: the network icon (cable, Wi-Fi bars, crossed out with no internet, slashed
  with none) replaces the placeholder; its menu names the network, disconnects, and joins
  a remembered or open network found by a scan when it opens. New secured networks need a
  passphrase, so they are left to Settings. Manifest gains `network-read`,
  `network-wifi`, `wifi-nearby`.
- Gate: `aura-sim-gate.sh n4` (RUNBOOK §13.6e).
- Since: `SET_STATIC`/`SET_DNS` built 2026-11-15 (§5.1 and §6); rc.d services (§10.6).
- Since (2026-11-14): the bar checked live under Forum (`aura-sim-gate.sh bar`, icon and
  menu frames); the Settings page is Curia's Network page ([curia.md](curia.md) §7).
  aura-mlme answers `scan` while connected with a scan on the association's address
  (the roam scan, listed as `bss` lines and `scan-done`), so the nearby list works
  while joined. A `scan` while a scan runs (auto-join's, or a join's) is listed when it
  ends, never refused (2026-11-15); atrium-netconf answers a busy radio (a handshake
  running) at once.


### 10.6 Services (2026-11-15)

atrium-net and atrium-netconf start with the system as rc.d services, not as
Portcullis-jailed services: atrium-net must be the host to change the host's
interfaces (§3), and atrium-netconf must open atrium-net's root-only control socket
before it drops to `_atrium-netconf` and enters capability mode.

- `atrium-net/etc/atrium-net` (`atrium_net`): after `netif` and `local_unbound`, before
  `atrium_netconf` and Portcullis. Supervised by `daemon -r -R 2`: started again 2 s
  after any exit; `service atrium-net stop` ends it for good. Ports: `atrium_net_ifaces`
  (default `AUTO`: every Ethernet port rc.conf does not configure, that is no
  `ifconfig_<name>` and no `ifconfig_DEFAULT`, or `ifconfig_<name>="up"`; a port left to
  rc.conf or dhclient is never taken) and `atrium_net_wifi` (default `AUTO`: `wlan0`
  if it exists). Without local-unbound running it starts with `--no-unbound` and says
  so. Logs to `/var/log/atrium/atrium-net.log`.
- `atrium-net/etc/atrium-netconf` (`atrium_netconf`): after `atrium_net`; waits up to
  20 s for its control socket, then the same supervision. It exits when atrium-net goes
  away and is started again, so a crash of atrium-net is survived by both.
- `scripts/atrium-net-service.sh DIR` (in the guest, once): the three users (65034–36),
  binaries in `/usr/local/bin` and `/usr/local/libexec`, the two scripts,
  `local-unbound-setup`, and `atrium_net_enable`, `atrium_netconf_enable`,
  `local_unbound_enable`.
- Gate: `aura-sim-gate.sh svc` (RUNBOOK §13.6e).
- Ports that come and go (2026-11-15): daemon(8) runs `atrium-net onerun`, which picks
  the ports at every start (waiting while there is none) and execs atrium-net with
  `--follow-ports`. atrium-net reads `RTM_IFANNOUNCE` on its routing socket: a port
  arriving (not loopback, bridge, tap/tun, epair, vlan, wg and the like; `plan::is_port`)
  or a managed one going away makes it exit with status 75, and the supervisor starts
  it again 2 s later with the ports there are now. Addresses stay on a port across the
  restart, so wired leases come back at once; atrium-netconf follows. No devd rule is
  needed. Checked by unloading and loading the Wi-Fi driver in `aura-sim-gate.sh svc`.
