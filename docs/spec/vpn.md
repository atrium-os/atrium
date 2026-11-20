# VPN: tunnels with the usual Atrium walls (Crypta)

**Status: accepted 2026-11-18** (Shankar: all §13 decisions 1–17 as recommended). Build order §14, starting with V0.
**Scope:** VPN client support for Atrium: our own tunnels first (WireGuard, later IKEv2),
then corporate VPNs (Cisco AnyConnect / Secure Client, Palo Alto GlobalProtect), including
their single sign-on and posture checks.
**Background:** Shankar 2026-11-15: "In usual Atrium style, we first design it with all the
safety and security we usually do and then think about accessing usual corporate protection
VPNs like Cisco, GlobalProtect etc." atrium-net.md §2 already names VPN tunnels as part of
atrium-net's domain and §8 defers them; network.md §13 item 4 leaves "VPN as default route"
open. This spec fills both.

Related: [atrium-net.md](atrium-net.md) (routes, DNS, egress, CLASS_NETCONF),
[network.md](network.md) §0 (per-app stacks), [wifi.md](wifi.md) §8 (the split this copies),
[arca.md](arca.md) (keys and bearer secrets, couriers), [sigillum.md](sigillum.md) §10
(certificates on tokens), [curia.md](curia.md) (Settings), [navigator-ui.md](navigator-ui.md)
(sign-in pages), [LICENSING-POLICY.md](../LICENSING-POLICY.md).

## 1. Short answer

- **Proposed name: Crypta** (Latin *crypta*, a covered, hidden passage, as in the Roman
  *cryptoporticus*). `cryptad` is the manager, `crypta-wg`, `crypta-ac`, `crypta-gp`,
  `crypta-ike` the protocol engines.
- **Every protocol runs as a jailed user-space engine**, one jail per active tunnel, as an
  unprivileged uid, with no files, no devices but its own `tun`, and network only to its
  gateway's addresses and ports. The kernel gets no new code: the engine reads and writes
  packets on a `tun` interface. That is the D9 rule (new hostile-input parsers go to jailed
  user space first) and keeps FreeBSD's `IPSEC` options off (kernel-hardening.md).
- **The root work stays in atrium-net**: the tunnel's addresses, routes, DNS and the
  leak-blocking pf anchor. It applies what an engine *proposes* only after checking it, the
  same way it checks DHCP proposals from `atrium-lease` (atrium-net.md §5.2).
- **Keys never leave Arca.** A WireGuard static key is an Arca key; the engine gets the
  *effect* (an X25519 result per handshake), never the key. Certificate keys sign through
  Arca or, on a smartcard, through Sigillum. Passwords and one-time codes are typed into
  Arca's own field. Session cookies from a gateway are bearer items bound to that gateway,
  released only to that gateway's engine after its TLS check, like a courier (arca.md §5).
- **Scope is a choice per tunnel:** the whole machine, or only chosen apps. Per-app routing
  is cheap here because every app already has its own network stack and epair
  (network.md §0): an app's host-side epair is put in the tunnel's routing table (FIB). An
  app outside the tunnel never sees it; an app inside it has no other way out, even when the
  tunnel is down.
- **No leaks by construction:** a "block when not connected" switch in pf, endpoint
  exceptions narrowed to the engine's own flows, DNS sent only into the tunnel for the names
  it covers, IPv6 blocked when the tunnel has none, and defences for the published leak
  attacks (TunnelVision, TunnelCrack) in §6.
- **Settings belong to cryptad** (profiles in `/var/db/crypta`), served over a new Aqueduct
  class `CLASS_VPN` (19) by an unprivileged front, as atrium-netconf serves atrium-net.
  Curia gets a VPN page; the Forum bar gets a VPN item.
- **Native protocols:** WireGuard first (v1). IKEv2/IPsec later, only when a real server
  needs it.
- **Corporate VPNs:** our own Rust implementations of the AnyConnect (CSTP/DTLS) and
  GlobalProtect (HTTPS + ESP) client protocols, as more engines under the same walls. Not
  OpenConnect (LGPL, ruled out by LICENSING-POLICY.md) and never the vendors' own clients.
  SAML sign-in happens in a Navigator page whose final token is caught by Arca's courier, so
  neither the page nor the UI ever holds it. Posture (HIP) reports state only true facts
  from Atrium's own services, show the user what is sent, and never run code the gateway
  downloads.

## 2. What we protect, and from whom

### 2.1 Assets

- Traffic meant for the tunnel: it must never go out in the clear, including DNS and IPv6,
  including while the tunnel is starting, roaming or broken.
- Credentials: WireGuard keys, preshared keys, certificate keys, passwords, OTP codes,
  gateway session cookies, SAML tokens.
- The user's privacy towards the VPN operator: a corporate VPN should learn what it needs
  about this machine and no more, and, when the user scopes it to work apps, nothing about
  the rest.
- The host and other apps, against a hostile gateway or a hostile network.

### 2.2 Attackers

| # | Attacker | Can do |
|---|---|---|
| V1 | The local network (café Wi-Fi, hotel, a rogue DHCP server) | DHCP options (routes, DNS), RAs, DNS answers, captive portals, drop or delay packets, spoof the gateway's name |
| V2 | Anyone on the internet | send packets to the engine's UDP port (WireGuard, ESP, DTLS), floods |
| V3 | A hostile or compromised VPN gateway | everything the server side of the protocol can send: config pushes (routes, DNS, scripts, banners), XML, posture requests, packets into the tunnel |
| V4 | The VPN operator acting within its rights (the employer) | sees all tunnelled traffic and the posture report, pushes policy |
| V5 | A local app | wants to learn or change VPN state, read credentials, escape or join a tunnel |
| V6 | A compromised engine (after a parser bug) | see §4.4 |

## 3. Processes and privilege

```
 Curia VPN page / Forum item ─CLASS_VPN─► crypta-ctl (unprivileged front, capmode)
                                              │
                                           cryptad (_crypta, no network, no devices)
                     profiles in /var/db/crypta, state machines, posture facts
              │ spawn via jaild        │ apply/withdraw proposals      │ token/sign/DH ops
              ▼                        ▼                               ▼
   crypta-wg / -ac / -gp / -ike     atrium-net (root, capmode)       arcad ── sigillumd
   one jail per tunnel,             tunN addresses, FIB routes,      (keys, certificates,
   uid from a range,                unbound forward-zones,           gateway cookies)
   /dev/tunN only, vnet epair       pf anchor atrium/vpn
   pinned to the gateway
```

| Process | Runs as | Holds | Parses hostile input |
|---|---|---|---|
| `cryptad` | `_crypta`, jail with no network, no devices; its state directory only | profiles (no secrets), tunnel states, posture facts | profile files it wrote; imported WireGuard `.conf` files (small INI, fuzzed) |
| `crypta-ctl` | `_crypta-ctl`, capmode after binding its sockets | nothing | `CLASS_VPN` requests from apps (postcard, bounded) |
| engines `crypta-*` | one uid per live tunnel from a reserved range, labelled in `/var/run/atrium/crypta-registry` (uid, profile, gateway), like couriers (arca.md §5.3) | that tunnel's session keys and cookies, for its life | everything the gateway and the internet send it |
| `atrium-net` | root (existing) | nothing new | none: it receives checked, typed proposals from cryptad |
| `arcad`, `sigillumd` | existing | keys, certificates, cookies | existing |

**Why cryptad is not inside atrium-net.** atrium-net is root and parses nothing; a VPN
manager has profiles, imports, state machines and talks to Arca and jaild. Keeping it out
keeps the root daemon small. atrium-net keeps owning every host-state change, so there is
one place that decides routes and DNS.

**Engine jail.** Started by jaild on cryptad's request (a new `crypta` section in
`jaild.policy.toml`, the way couriers have theirs):

- network: its own vnet with one epair in FIB 0, pf passing only the gateway's resolved
  addresses and ports (plus the resolvers at connect time), like a courier jail
  (arca.md §5.4);
- devfs: a ruleset that shows only `/dev/tunN` (atrium-net creates `tunN` and gives the
  engine's jail that one node);
- no filesystem beyond a read-only root; capability mode after start-up; `RLIMIT_NPROC` 0;
- the sysfil profile `network`;
- its talk with cryptad is one socketpair, a fixed postcard schema in both directions.

## 4. The native protocol: WireGuard

### 4.1 Why WireGuard first

Small fixed-format protocol (four message types), modern crypto with no negotiation to
downgrade, roams across networks without renegotiating, interoperates with every common
server (Linux kernel, FreeBSD `if_wg`, wireguard-go, commercial privacy VPNs, Tailscale
and Headscale exit nodes in plain WireGuard mode). It also covers the "my own server"
case Shankar is most likely to use first.

### 4.2 crypta-wg

Our own Rust implementation of the WireGuard protocol (Noise_IKpsk2, the cookie reply, the
transport, timers, the replay window), user space on `tun`. Written from the WireGuard
paper and the protocol page; test vectors and interop against Linux kernel WireGuard and
FreeBSD's `if_wg`. Crypto from RustCrypto crates (`chacha20poly1305`, `blake2`; X25519
done by Arca, §4.3). boringtun (BSD-3) is a fallback to vendor if writing our own turns out
slower than expected; it is not chosen first because it keeps the static key in its own
state, which §4.3 avoids.

Hostile input is the four message types from anyone on the internet (V2). Initiations
without a valid `mac1` (computed from our public key, so the engine checks it without any
secret) are dropped before anything else; under load the cookie mechanism applies, as the
protocol specifies. Every parser is fuzzed on the host.

### 4.3 The static key stays in Arca

A WireGuard handshake needs the static private key for two Diffie-Hellman steps per
handshake (static-static and static-ephemeral), so a handshake every two minutes per peer.
Arca gets a key kind `WireGuardKey` (X25519) and one op:

- `DH_X25519` (new): `id[16] | point[32]` → `shared[32]`, only for the recipient the item
  is bound to (`crypta:<profile id>`), answered to an engine whose uid the crypta registry
  labels with that profile. The private half never leaves Arca (arca.md §1, promise 1).

The engine gets the effect of the key while it runs and never the key: a compromised engine
can impersonate us to that one peer while it lives, and nothing afterwards.

The optional preshared key (WireGuard's post-quantum hedge) is a bearer item (`kind
WireGuardPsk`), released to the same recipient after the engine starts.

Key creation: Curia's "New WireGuard tunnel" asks arcad to *generate* the key and shows the
public key to paste into the server. Importing a `.conf` file (most providers hand one out)
goes through cryptad: the file reaches it as a descriptor from Scrinium's picker, cryptad
parses it, stores `PrivateKey` and `PresharedKey` in Arca, wipes them and keeps the rest as
the profile. The file itself still holds the key; Curia offers to delete it afterwards.

Which store: a tunnel that must run before anyone logs in (an always-on machine tunnel)
lives in Arca's machine store (arca.md §6.4, class "after boot"); a person's tunnel lives in
that person's store with class "after first unlock", so it keeps running while the screen
is locked.

### 4.4 If an engine is compromised

- It holds that tunnel's session keys and sees that tunnel's cleartext traffic, which the
  far end (the VPN server) sees anyway.
- It can inject packets into the host as if from the tunnel's network, which the server
  can already do. Those packets meet the same pf rules as any tunnel traffic (§5.4).
- It can ask Arca for that one profile's DH results and items. It cannot reach another
  tunnel's items (different recipient), the filesystem, another app or the LAN (its pf
  rules pass only the gateway).
- It can send atrium-net nothing directly. Its reports go to cryptad as typed values, and
  everything that changes the host goes through atrium-net's checks (§5.2).

So a compromised engine is bounded by what the VPN server already has. That is the line the
corporate engines (§9) are held to as well.

## 5. Applying a tunnel: atrium-net's part

### 5.1 What atrium-net gets

When an engine is up it reports, through cryptad, a **tunnel proposal**: tunnel addresses
(v4, v6), MTU, routes to include, routes to exclude, DNS servers, DNS domains, and whether
the server asks for all traffic. For WireGuard these come from the profile (AllowedIPs,
Address, DNS); for corporate engines from the gateway's config push.

### 5.2 Checks (in atrium-net, before anything changes)

Same shape as atrium-net.md §5.2:

- tunnel addresses unicast, not ours (loopback, link-local, multicast, the app range
  100.64.0.0/16, another interface's subnet);
- at most 1024 include routes and 256 excludes; none covering 127/8, ::1, link-local,
  multicast, or the app range (a corporate network that uses 100.64.0.0/10 conflicts with
  the app range; atrium-net refuses that route and says so, and jaild's app range is
  configurable, network.md §3.3);
- an exclude may only cover addresses, never the gateway route itself;
- at most 3 DNS servers per family, unicast, inside the tunnel's routes;
- at most 64 DNS domains, each a valid name of at most 253 bytes, never the root unless the
  tunnel carries all traffic, never `localhost`, `local` or Atrium's own names;
- MTU clamped to 1280–1500; lifetimes and rekey intervals clamped;
- anything else in the push (scripts, proxy settings, client-update URLs) is dropped and
  logged (§9.5).

### 5.3 Routes and scope

Every tunnel gets its own routing table (FIB). atrium-net raises `net.fibs` as needed
(at runtime; to verify on 16-CURRENT, else a boot tunable with a fixed budget of 8).

- **Machine scope.** FIB 0 (the host's own) gets the tunnel's routes; with "all traffic"
  the default route moves to `tunN`. Apps follow, since their epairs are in FIB 0.
- **App scope.** The tunnel's FIB gets the tunnel's routes, and with "all traffic" its
  default route. jaild puts the host end of each chosen app's epair into that FIB
  (`ifconfig epairNa fib N`: packets *received* on an interface use its FIB), at launch and
  live when the user changes the list. Their NAT goes out `tunN`. Everything else stays in
  FIB 0 and never sees the tunnel.
- **Helpers follow the app they serve.** Jails that act for an app (its Nuntius fetcher, its
  Arca couriers, Navigator's page and converter jails) are started in the FIB of that app.
  The launchers (portcullisd, courierd, navigatord) pass the app's FIB to jaild; a helper
  never chooses its own.
- **Split tunnels** are just include routes on the tunnel and nothing else in its FIB.
  An app-scoped split tunnel has the tunnel's routes plus FIB 0's routes copied for
  everything else, so a work app still reaches the internet directly when the corporate
  gateway asks for split tunnelling.

Fail closed: an app-scoped FIB with "all traffic" and no tunnel has no default route at all,
so a work app whose tunnel drops has nowhere to send, which is the kill switch for free.

**The gateway route.** The engine's own outer packets must not enter the tunnel. They come
from the engine's epair in FIB 0; atrium-net pins a host route to each gateway address
through the physical egress (as the probe pins its routes, atrium-net.md §4.2), and pf
(§5.4) allows only the engine's own flows to use it.

### 5.4 The pf anchor `atrium/vpn`

atrium-net owns one anchor, written through a `/dev/pf` descriptor it opens before
`cap_enter`. (network.md gives pf to atrium-netd for per-app anchors; host egress policy
is atrium-net's domain, and atrium-netd is not built. Decision 6.)

- `nat on tunN from 100.64.0.0/16 -> (tunN)`.
- Engine flows are tagged on the engine's epair (`tag CRYPTA_OUTER`), and only tagged
  packets may go to a gateway's address and port out the physical interface. Anything
  else to that address goes into the tunnel or is blocked. This closes the "server IP"
  leak (§6).
- **Block when not connected** (per profile; default on for machine-scope full tunnels and
  for privacy VPNs): `block out quick on <physical>` for everything except tagged engine
  flows, atrium-net's own DHCP, ND/RA and the connectivity probe, and the captive-portal
  exception (§6). It stays loaded across tunnel restarts, roams and engine crashes, and is
  removed only when the user turns the tunnel off.
- No tunnel IPv6 while the tunnel carries all traffic: v6 out the physical interfaces is
  blocked for the scope (machine, or the app FIB), so v6 cannot carry around a v4-only
  tunnel.
- Inbound: nothing from the tunnel reaches the host or apps unless it belongs to a flow they
  opened (stateful `pass out` only). A corporate network cannot open connections to this
  machine. A future "reachable over VPN" setting would add per-app inbound grants through
  the existing manifest model.

### 5.5 DNS

- **Machine scope:** unbound gets a `forward-zone` per tunnel DNS domain pointing at the
  tunnel's DNS servers; with "all traffic", the tunnel's servers become the forwarders for
  everything. The pushed domains are marked `domain-insecure` only when the profile says so
  (internal zones under a signed public parent often fail DNSSEC; the user sees this as a
  setting, default off for WireGuard, on for corporate pushes).
- **App scope:** a second `local-unbound` instance per tunnel, run in that tunnel's FIB on
  its own loopback address, forwarding as above; apps in the scope are given it as their
  resolver (the Insula broker answers them from it; their own resolver address is
  redirected to it on their gateway). Apps outside the scope never resolve through it, so
  the local network never sees a work app's lookups and the employer never sees anyone
  else's.
- Names under a tunnel domain never go to the physical network's servers, even when the
  tunnel is down: they fail instead.

### 5.6 Network changes

On `egress-changed` (atrium-net.md §4.2) cryptad tells each engine to rebind: WireGuard
just sends from the new path, AnyConnect reconnects DTLS or CSTP with its session cookie,
GlobalProtect re-establishes ESP. The gateway host routes are re-pinned through the new
egress first. Sleep and resume are treated the same way.

## 6. Leaks and the attacks behind them

| Attack | What it does | Defence here |
|---|---|---|
| **TunnelVision** (CVE-2024-3661) | a rogue DHCP server pushes classless routes (option 121) more specific than the tunnel's, pulling traffic out of it | machine full tunnel: atrium-net accepts no DHCP routes except the on-link subnet while it is up, and pf's block-when-not-connected drops non-tunnel egress anyway; app scope: DHCP routes only ever enter FIB 0, which those apps do not use |
| **TunnelCrack LocalNet** | the LAN hands out a public prefix so "allow local network" leaks traffic to that public range | "Allow local network" is off by default for full tunnels and, when on, covers only directly connected subnets inside RFC 1918, ULA and link-local |
| **TunnelCrack ServerIP** | the attacker spoofs the gateway's DNS answer so the "route to the server" exception covers a victim address | the exception is for the engine's tagged flows to the gateway's port only (§5.4); the gateway name is resolved over DNSSEC where signed, and the profile keeps the last good addresses and warns when they change |
| DNS leak | names resolved outside the tunnel | §5.5: tunnel domains forward only into the tunnel; full tunnel replaces all forwarders |
| IPv6 leak | v4-only tunnel, v6 goes direct | §5.4 v6 block |
| Start-up and crash leak | traffic flows before the tunnel is up or after the engine dies | block-when-not-connected is loaded before the engine starts and survives it; app-scope FIBs have no default route without the tunnel |
| Captive portal | the network wants a sign-in before anything works | with the block on, the user may start "Sign in to this network": a Navigator page in an Unrecorded trail, whose jail is in FIB 0 and tagged as the one exception, for as long as the portal flow lasts. The probe tells when it is done. Built with atrium-net's captive sign-in (atrium-net.md §8) |
| Handshake flood | CPU exhaustion of the engine | `mac1` check first, cookie replies under load, per-jail CPU limit (rctl) |

## 7. Settings and the API

### 7.1 Profiles

`/var/db/crypta/profiles.toml` (`_crypta`, 0600), written only by cryptad. Per profile:
name, kind (`wireguard`, `anyconnect`, `globalprotect`, `ikev2`), owner (the machine, or a
person), scope (machine or a list of apps), block-when-not-connected, allow-local-network,
connect (manual, on login, always), the protocol's settings (peer, endpoint, AllowedIPs,
keepalive; or gateway URL and group), the Arca item ids it uses, posture settings (§10).
No secret is ever in the file.

A person's profile can use machine scope only if that person is an administrator (Vestibulum
role); otherwise app scope over their own apps. A machine profile set to "always" starts at
boot, before login.

### 7.2 CLASS_VPN (new Aqueduct class 19)

Served by `crypta-ctl` for cryptad, one socket per capability, as atrium-netconf does
(atrium-net.md §7.1):

| Op | Capability |
|---|---|
| `Status`, `Subscribe` (tunnels: name, kind, state, scope; never addresses or gateways) | `vpn-read` |
| `ListProfiles`, `GetProfile`, `Connect`, `Disconnect` | `vpn-use` |
| `AddProfile`, `ImportWireGuard`, `SetProfile`, `RemoveProfile`, `SetApps`, `AnswerPrompt` | `vpn-configure` (restricted: Curia only) |

`vpn-read` tells an app nothing about where the machine is (no addresses, no gateway
names), only whether its own traffic is tunnelled. No capability lets an app put itself in
or out of a tunnel; only the user does, in Curia.

### 7.3 Curia's VPN page

A list of tunnels (name, state, scope), Connect and Disconnect, and per tunnel: settings,
"Apps that use this VPN", the block switch, the posture report (§10), the last sign-in. New
tunnel: WireGuard (generate a key, or import a file) or a corporate VPN (gateway address and
kind). Sign-in prompts (§9.3) appear on this page. It follows Curia's resilience rules
(curia.md §10): when cryptad is not running the page says so and offers nothing, and it
reconnects when cryptad returns. Losing cryptad does not lose tunnels' leak protection:
pf rules and FIBs are atrium-net's, and stay as they were until cryptad comes back and
reconciles.

### 7.4 Forum item

A status item (forum-status-items.md) with `vpn-read` + `vpn-use`: a lock-tunnel icon
(connected, connecting, needs sign-in, blocked because down), a menu to connect and
disconnect saved tunnels. "Needs sign-in" opens Curia's VPN page at the prompt.

## 8. IKEv2 / IPsec (later)

Many corporate and self-hosted servers speak IKEv2 (strongSwan, Windows RRAS and Always On
VPN, Cisco and Fortinet in IKEv2 mode, most phones' built-in VPNs). When one is needed:

- `crypta-ike`: our own Rust IKEv2 initiator (RFC 7296), certificate and EAP-TLS
  authentication through Arca and Sigillum (signatures only, the key never moves),
  EAP-MSCHAPv2 only behind a profile flag (weak). Same jail, same proposal path.
- ESP in the engine on `tun`, not the kernel's IPsec stack: no `IPSEC` options in the
  kernel, no PF_KEY, no root IKE daemon. The engine handles ESP and UDP encapsulation
  (RFC 3948) itself.
- strongSwan (GPL) is a test peer only, never shipped.

Not proposed now: it is a large parser (certificates, EAP, many payloads) for a need nobody
has stated yet.

## 9. Corporate VPNs

### 9.1 The shape

The corporate VPNs in the ask, with what they speak:

| Product | Protocol | Data channel | Sign-in | Posture |
|---|---|---|---|---|
| Cisco AnyConnect / Secure Client (ASA, FTD) | XML auth over HTTPS (`config-auth`), then CSTP: an HTTP `CONNECT` over TLS | DTLS 1.2 beside the TLS tunnel, TLS-only fallback | password, OTP, group select, client certificate, SAML (in a browser, final `acSamlv2Token` cookie) | HostScan / Secure Firewall Posture: the gateway sends a program to run (§10.3) |
| Palo Alto GlobalProtect (PAN-OS) | HTTPS + XML: portal (`prelogin`, `getconfig`), gateway (`login`, `getconfig`) | ESP in UDP (port 4501), SSL tunnel fallback | password, OTP, client certificate, SAML (browser, final `prelogin-cookie` / `portal-userauthcookie` headers) | HIP report: an XML document of host facts the client submits (§10.2) |

Both are proprietary protocols with public, independent descriptions: the OpenConnect
protocol Internet-Draft (draft-mavrogiannopoulos-openconnect) for AnyConnect, and
OpenConnect's protocol notes for GlobalProtect. OpenConnect itself proves that a clean
client works against real gateways.

### 9.2 Own engines, not OpenConnect, not vendor clients

- **OpenConnect** (libopenconnect) is LGPL-2.1, and LICENSING-POLICY.md forbids LGPL in the
  runtime. It also runs a "vpnc-script" as root from pushed values and parses XML with
  libxml2 in C. Not used, not linked. Its source is not used as a reference either (same
  rule as wifi.md §4 for GPL code); its protocol documents and the Internet-Draft are.
- **Vendor clients** (Cisco Secure Client for Linux, GlobalProtect for Linux) are Linux
  binaries that want root, systemd and their own kernel pieces, and they fetch and run
  updates. Hospes (our Linux compat) hosts foreign *libraries* in a jail, not foreign root
  daemons. Not supported.
- **Our engines:** `crypta-ac` (AnyConnect) and `crypta-gp` (GlobalProtect), Rust, under the
  engine jail of §3: TLS by rustls (the stack the rest of Atrium uses, TLS.md), XML by a
  strict, bounded pull parser (no DTDs, no entities, size and depth limits), ESP and the
  CSTP framing as fixed-format parsers, all fuzzed on the host. DTLS (AnyConnect's fast path)
  needs a DTLS 1.2 implementation with its pre-shared session resumption; rustls has none,
  so v1 of `crypta-ac` is TLS-only (works everywhere, slower on lossy links), and DTLS comes
  with a permissive DTLS library or our own (decision 13).
- Later engines fit the same walls: Fortinet SSL VPN, Ivanti/Pulse, F5, all described in
  OpenConnect's protocol notes.

A compromised corporate engine is bounded as in §4.4: it holds that gateway's session and
sees the corporate traffic, which the gateway (the employer, V4) already has.

### 9.3 Sign-in

**Forms.** Both protocols send forms: username, password, OTP, a group or gateway choice,
sometimes a second factor prompt. The engine turns each form into a typed prompt (fields
with kinds: text, choice, secret, OTP) that cryptad sends to Curia's VPN page (or, from the
Forum item, opens the page). Secret and OTP fields are Arca's credential field
(arca-autofill.md, the way Curia's Wi-Fi passphrase works since C2): Arca holds what was
typed and Curia gets a handle; the engine redeems the handle at arcad with a new op
`TAKE_VPN` (like `TAKE_WIFI`, curia.md §8), only for the engine labelled with that profile
and only after its TLS to the gateway checked out. Curia, cryptad and crypta-ctl never hold
a password. A saved password is an ordinary Arca bearer item whose recipient is the gateway
endpoint, released only to that gateway's engine after TLS (the courier rule, arca.md §5).

**Client certificates.** Corporate gateways often require a machine or user certificate.
The engine's TLS client authentication uses a signer that calls arcad with the item's
binding: a software key held by Arca (`kind TlsClientKey`, TPM-sealed where present), or a
`TokenKeyRef` that arcad forwards to sigillumd, which has the smartcard or token sign
(sigillum.md §10.1, the path the Navigator's couriers already use). The chooser sheet,
PIN entry and "use this certificate for this gateway" binding are Sigillum's, unchanged.

**SAML / SSO** (Okta, Entra ID, Ping, ADFS; the usual corporate setup today):

1. The engine's pre-login gets a SAML request (a redirect URL or an HTML form to post) from
   the gateway.
2. cryptad asks navigator-ui to open a **sign-in page**: a trail in a jar of its own,
   `vpn-<profile>` (an Arca partition, navigator-ui.md §2.4), so the identity provider's
   session cookies are kept for the next VPN sign-in but never mixed with personal browsing.
   The trail shows "Sign in to <gateway>" in the strip, and the user signs in with whatever
   the IdP asks: password into Arca's field, push approval, a passkey or security key
   through Sigillum's WebAuthn.
3. The last step of SAML posts to the gateway's own address (the assertion consumer
   service). That fetch goes, like every cookied Navigator fetch, through Arca's courier for
   the gateway endpoint. The courier is told, for this endpoint and jar, which response
   fields are tokens: the `acSamlv2Token` cookie for AnyConnect, the `prelogin-cookie`,
   `portal-userauthcookie` and `saml-username` headers for GlobalProtect. It hands them to
   Arca as items bound to the gateway and strips them (the "declared token fields" of
   arca.md §5, step 5). The page sees a finished sign-in and never the token.
4. navigatord tells cryptad the flow ended; cryptad tells the engine, which asks arcad for
   the gateway's items after its own TLS check and logs in with them.

So the token moves gateway → courier → Arca → engine, all bound to the one gateway. AnyConnect's
"external browser" mode (a token posted to `localhost:29786`) is not used: no local port
listens for it.

**Dependency.** IdP sign-in pages need their scripts to run, which needs the Navigator's
live-code lane (the engine jail that produces islands, navigator-ui.md §6.3, still to
build). Until then corporate profiles work with forms and certificates only, and SAML
profiles say "needs the Navigator's live pages". This is the one part of the corporate
design that waits on another component.

**Re-authentication.** The gateway's session cookie (AnyConnect `webvpn`, GlobalProtect
`authcookie`) is an Arca bearer item bound to the gateway, class after-first-unlock, deleted
when the gateway ends the session or the user disconnects. Reconnects after roaming or sleep
reuse it without asking; an expired session goes back to the sign-in prompt.

### 9.4 What a gateway may push, and what we do with it

| Push | Handling |
|---|---|
| Addresses, routes, split include/exclude, DNS, domains, MTU | a tunnel proposal, checked by atrium-net (§5.2) |
| "Tunnel all traffic" | honoured in the profile's scope; in app scope it means all traffic *of those apps* (§9.6) |
| Always-on / lockdown | turns on the profile's block-when-not-connected; the user can still disconnect the profile (with a warning that the employer asked for it) |
| Banner / message of the day | shown as plain text in Curia, at most 4 KiB, control and bidi characters stripped, before the tunnel comes up when the gateway requires acceptance |
| Scripts (`vpnc-script` style), client updates, downloaded modules | never run, never downloaded; logged |
| Proxy / PAC | ignored in v1 (would need a per-scope proxy setting for apps); logged |
| Idle and session timeouts, rekey | clamped and followed |

### 9.5 What the gateway learns

- Identity: what the user signed in with.
- Client identification. Gateways expect a client OS and version, and some refuse unknown
  ones. Proposed (decision 14): the OS class field says `Linux` (what OpenConnect sends from
  every non-Linux Unix too; GlobalProtect and AnyConnect have no other suitable value), and
  every free-text version field says Atrium truthfully. Never Windows or macOS.
- A host ID, when the protocol requires one: derived per profile from the machine secret
  (as per-app MACs are, network.md §0), stable for that employer and unlinkable across
  employers. Never a hardware serial, SMBIOS UUID or real MAC.
- Hostname: a profile setting, default a generic name.
- Interface MACs (GlobalProtect asks): the per-network address currently in use, which is
  already random per network (wifi.md decision 8), or none.
- Posture facts: §10, shown to the user first.

### 9.6 Work apps only

Because scope can be per persona (§9.7), a person can send only their work apps (mail, chat, the
Navigator's work persona, a dev workspace) through the corporate VPN, and the rest of the
machine goes direct. The employer then sees work traffic only, and work apps cannot reach
the internet around the tunnel when it is down. This is the recommended default for a
corporate profile owned by a person (decision 10); machine scope is for a company-owned
machine whose policy requires it.

Whether the employer's policy allows split use is the user's business with their employer,
not something the gateway can check; Curia states plainly when the gateway asked for all
traffic and the user scoped the tunnel to some apps.

### 9.7 Personas

Splitting apps between work and personal (a Work Navigator window, Work mail and chat, a
Work workspace terminal beside a Personal one, all clearly labelled) is a system feature of
its own: [personas.md](personas.md). For the VPN it means:

- **A tunnel's scope is a list of personas**, not apps. "Work → Acme VPN" puts every Work
  instance (personas.md §3: each app runs once per persona) and every Work workspace on
  the tunnel's routing table (§5.3); Personal instances stay in FIB 0.
- **Helpers follow their instance**: a Work Navigator window's page, converter, fetcher
  and courier jails, and a Work workspace's consoles, are Work's.
- **Labels** (personas.md §7): the strip chip carries the route, `Work · via Acme VPN`,
  amber `Work · Acme VPN down: no network`; while a VPN is up every other window says
  `Personal · direct`.
- **Tunnel-only links never go direct**: a Work link whose host is under a tunnel DNS
  domain or resolves into the tunnel's routes is never opened outside the tunnel, even when
  the person allows links to cross personas (personas.md §6), because that would only send
  an internal name to the public network.

## 10. Posture checks

### 10.1 Rules

1. **Only true facts.** The report is built from what Atrium's own services report. If a
   fact is unknown or not applicable (antivirus on Atrium), it is left out or reported as
   absent, never invented.
2. **The user sees it.** Curia shows "What <VPN> is told about this machine" with every
   field, before the first report is sent and whenever it changes. Fields the user can
   control (hostname) are editable there.
3. **No gateway code ever runs.** Nothing downloaded from a gateway is executed, in any
   jail.
4. **Facts come over sockets, read-only.** cryptad asks the owning services; the engine
   only formats.

### 10.2 GlobalProtect HIP

GlobalProtect asks after login whether a HIP report is needed, then takes an XML report
(host info, OS, network interfaces, and categories such as antivirus, firewall, disk
encryption, patch management, data loss prevention) and repeats every HIP interval.

| Category | Source | Atrium answer |
|---|---|---|
| OS and version | Opifex / build info | `Linux` class (decision 14), version string Atrium's |
| Host ID, hostname, MACs | cryptad | §9.5 |
| Firewall | atrium-net (pf enabled, default deny for apps) | enabled, product "Atrium pf" |
| Disk encryption | FreeBSD GEOM, kernel mount table and Tessera kernel state | Counts of mounted active GELI providers, authenticated sealed Tessera volumes, and Tessera domains verified with active keys, when available |
| Patch management | Opifex catalog | Last check time and catalog update count, or last check failure |
| Screen lock | Ostiarius active seat and lock marker | Whether the active seat is locked, when known |
| Antivirus / anti-malware | none on Atrium | none listed |

If the gateway's HIP policy requires an antivirus product, the gateway will quarantine or
refuse, and the user sees its message. The way through is the employer adding an Atrium
(or "Linux, no AV") HIP profile, not a fabricated report. Curia's report view says which
categories were empty, so the user can tell their IT department exactly what to allow.

### 10.3 Cisco HostScan / Secure Firewall Posture

The ASA asks the client to download and run a posture program (the "CSD" stub and its
modules) and then post its results. Atrium does not run it (rule 3). `crypta-ac` answers
the posture exchange the way a client without the module does; gateways set to "posture
optional" for the user's group let it through, gateways that require HostScan refuse, and
Curia says so ("this gateway requires Cisco's posture program, which Atrium does not run;
ask your IT team for a policy without it"). OpenConnect's practice of posting a fabricated
"everything fine" report is not followed (rule 1).

Cisco ISE posture (a separate agent talking to ISE, not to the VPN gateway) is out of scope.

## 11. Testing

All without hardware, in the cloud arm64 VM (scripts/cloud-vm.sh) and the Ubuntu AP VM of
the Aura gates (scripts/aura-ap-vm.sh), extended:

- **crypta-wg** against Linux kernel WireGuard in the Ubuntu VM and FreeBSD `if_wg` in a
  second FreeBSD VM: handshake, transport, rekey, roam (the VM's address changes mid
  stream), keepalive behind NAT.
- **Leak gate** with atrium-net's hostile LAN (`atrium-net/sim/lan.py`) extended: rogue
  option 121 routes (TunnelVision), a public prefix as LAN (LocalNet), a spoofed gateway
  name (ServerIP), RAs offering v6 around a v4 tunnel, DNS answers for tunnel domains from
  the LAN, an engine killed mid-stream. Pass means a capture on the LAN side shows no
  packet that should have gone into the tunnel.
- **App scope:** two apps, one assigned; capture shows only the assigned app's traffic in
  the tunnel and only the other's outside; the assigned app has no route when the tunnel
  is down.
- **AnyConnect** against ocserv in the Ubuntu VM (GPL, a test tool only, never shipped;
  LICENSING-POLICY.md allows that): password, OTP, certificate (QEMU's emulated CCID card
  for the Sigillum path), session resume after roam, a gateway pushing hostile config.
- **GlobalProtect:** use OpenConnect 9.21's external `tests/fake-gp-server.py`
  for the control plane (`scripts/crypta-gp-control-check.py`, Python with Flask).
  The fixture is run from its own checkout or release archive. Configure it for TLS
  with ESP disabled. Its TLS tunnel opening deliberately returns HTTP 502.
  Run the Atrium TLS peer (`python3 scripts/crypta-gp-packet-check.py`) for IPv4
  packet transfer, rebind and logout. With `--vm arm64` it builds and stages the
  engine and harness on an already booted FreeBSD VM with QEMU user networking.
  The harness runs the engine in a temporary jail, checks its jail ID and
  capability mode, and pings through a real tun before and after rebind. Its
  control and Arca descriptors are harness fixtures. With `--full-vm arm64` on
  a dedicated, already-booted guest with the simulated LAN on vtnet1, the same
  peer runs against jaild, cryptad, Arca, and atrium-net. Set `CRYPTA_GATE_VSSH`
  to the dedicated guest's SSH wrapper if it uses another port. Eight checks
  passed on FreeBSD arm64. The peer also verified one login, two TLS tunnels
  carrying ICMP echo, and logout. With `--openconnect /path/to/openconnect` the
  peer answers an ICMP echo through OpenConnect's script-tun socket.
- **No real company gateway** (Shankar 2026-11-15): every gate runs on local servers.
  Free public services only add value for WireGuard (for example Proton VPN's free
  plan, which hands out WireGuard configs), as an optional real-internet check.
- **SAML** against Keycloak (Apache-2.0) as the IdP in the Ubuntu VM, with the stand-in
  GlobalProtect gateway and ocserv behind it, once Navigator's live pages exist.
- **IKEv2** (when built) against strongSwan in the Ubuntu VM.
- **Fuzzing:** every engine parser on the host (WireGuard messages, ESP, CSTP framing, the
  XML subset, HTTP heads), and cryptad's `.conf` importer.

## 12. Not in v1

OpenVPN (protocol needs our own implementation; on demand), IKEv2 (§8), DTLS for
AnyConnect (§9.2), Fortinet/Ivanti/F5, proxy/PAC from gateways, inbound connections over a
tunnel, site-to-site, being a VPN server, multi-hop, post-quantum WireGuard (Rosenpass).

## 13. Decisions

1. **One jailed user-space engine per tunnel on `tun`, no new kernel code** (no `if_wg`, no
   kernel IPsec). *Recommend yes.* Alternative: FreeBSD's `if_wg` for WireGuard (faster:
   in-kernel crypto, no per-packet syscalls), with the static key in kernel memory and
   readable by root through `SIOCGWG`. Can be added later as a fast path if measured
   throughput on `tun` is too low.
2. **cryptad separate from atrium-net**; atrium-net keeps every host change and checks
   tunnel proposals. *Recommend yes.*
3. **WireGuard static keys stay in Arca**, used through a new `DH_X25519` op bound to the
   profile. *Recommend yes.* Alternative: release the key to the engine (simpler, a bearer
   item; weaker).
4. **Own Rust WireGuard** (`crypta-wg`) rather than vendoring boringtun. *Recommend yes*
   (small, and §4.3 needs the key outside the protocol state); boringtun as fallback.
5. **Per-tunnel FIBs and per-app scope** through the app's epair FIB, helpers following the
   app they serve. *Recommend yes.*
6. **atrium-net writes the `atrium/vpn` pf anchor** (host egress policy), not atrium-netd.
   *Recommend yes.*
7. **Block when not connected:** default on for machine-scope full tunnels and WireGuard
   privacy profiles, default off for split tunnels; always on when a gateway asks for
   always-on. *Recommend yes.*
8. **IPv6 blocked outside a full tunnel that carries no v6.** *Recommend yes.*
9. **New Aqueduct class `CLASS_VPN` = 19** with `vpn-read`, `vpn-use`, `vpn-configure`
   (restricted, Curia only); no app can move itself in or out of a tunnel. *Recommend yes.*
10. **Corporate profiles owned by a person default to app scope** ("work apps"); machine
    scope for administrators and machine profiles. *Recommend yes.*
11. **Own Rust AnyConnect and GlobalProtect engines**; no OpenConnect (LGPL), no vendor
    clients; OpenConnect's protocol documents and the Internet-Draft as references, not
    its code. *Recommend yes.*
12. **SAML in a Navigator sign-in page with its own jar, token caught by Arca's courier**,
    never by the page or a localhost listener. *Recommend yes*; it waits on Navigator's
    live-code lane.
13. **AnyConnect v1 TLS-only**, DTLS later with a permissive DTLS 1.2 library or our own.
    *Recommend yes.*
14. **Client OS reported as the `Linux` class with Atrium's true version string**, never
    Windows or macOS; host ID derived per profile. *Recommend yes.* Alternative: report an
    `Atrium` OS value and accept that many gateways refuse it.
15. **Posture: true facts only, shown to the user first, no gateway code run, no fabricated
    HostScan answers.** *Recommend yes.*
16. **IKEv2 later, in user space with ESP on `tun`**, only when a server needs it.
    *Recommend yes.*
17. **Name Crypta** (`cryptad`, `crypta-*`). *Recommend yes*; alternative *Cuniculus*
    (tunnel).
18–21. **Moved to [personas.md](personas.md) §9** (instances per persona, indication,
    one persona per Navigator window and workspace, crossing off by default). For the VPN:
    a tunnel's scope is a list of personas (§9.7). *Recommend yes.*

## 14. Build order

| Step | What | Done when |
|---|---|---|
| V0 | `crypta-wg` core on the host (Noise IK, transport, timers, replay window); arcad `WireGuardKey` + `DH_X25519` | test vectors pass; a host-only run handshakes with a Linux WireGuard peer over a socket; fuzzers run clean for an hour |
| V1 | cryptad, engine jail via jaild, `tun`, atrium-net tunnel proposals, machine scope, unbound forward-zones, `atrium/vpn` anchor with block-when-not-connected | cloud VM reaches the Ubuntu VM's WireGuard network; leak gate (§11) passes; killing the engine leaks nothing |
| V2 | Persona scope: per-tunnel FIB, jaild epair FIB, helper inheritance, per-tunnel resolver; per-(app, persona) instances in portcullisd; workspaces with a persona | app scope gate (§11) passes; a Work and a Personal workspace side by side, captures show only Work's traffic in the tunnel. Built 2026-11-19, see below |
| V2b | The route in the persona chip (personas.md P1 first) | frames of Work and Personal windows with the tunnel up, connecting and down. Built 2026-11-19, see below |
| V3 | `CLASS_VPN` + crypta-ctl, Curia VPN page (generate key, import file, apps list, block switch), Forum item | Curia frames of each; an app without `vpn-configure` is refused |
| V4 | `crypta-ac` TLS-only: forms through Arca fields (`TAKE_VPN`), saved passwords, client certificates through Arca and Sigillum, posture answer without HostScan | ocserv gate: password, OTP and certificate sign-ins; roam resumes; hostile pushes refused |
| V5 | `crypta-gp`: portal and gateway login, ESP in UDP, SSL fallback, HIP from true facts with the Curia report view | stand-in gateway gate, the stand-in also accepting OpenConnect's GP client |
| V6 | SAML through a Navigator sign-in page and courier token capture | test IdP gate; needs the Navigator live-code lane |
| V7 | On demand: AnyConnect DTLS, IKEv2, more vendors, the `if_wg` fast path | per need |

**V0 as built (2026-11-18).** `crypta/crypta-wg`: the protocol sans-IO (the engine passes datagrams, packets and the time; it gets back what to send, what to hand to `tun`, and events). Noise_IKpsk2 with every static-key DH through a `StaticKey` trait; mac1 checked before any secret is used, cookies under load (`Device`, 256 handshakes a second, or forced), the 2048-bit replay window (a counter up to 1983 below the newest is still taken once), the timers of the paper's §6 (retry every 5 s plus jitter, give up at 90 s and drop what waited, rekey at 120 s or 2^60 messages, reject at 180 s, keepalive after 10 s, a new handshake after 15 s with data unanswered, everything erased after 9 minutes without new keys), the responder waiting for key confirmation, and initiation timestamps that never go back. Checks: the protocol's initial chaining key and hash as test vectors, 19 host tests, interop with boringtun (dev-dependency only) over UDP sockets both ways with a preshared key, cookies and a rekey, and `scripts/crypta-wg-linux-vm.sh` against Linux kernel WireGuard in an Ubuntu 24.04 VM (we initiate: 20 pings of growing size; Linux initiates: its 5 pings answered; 0 failed). Fuzzers `messages`, `replay` (against a set model) and `device`, each run clean for an hour (497M, 43M and 294k runs) (one peer, real messages changed and replayed, random bytes; nothing delivered that the peer didn't send). arcad: kind `WireGuardKey` (0x104) minted by `MINT`, its public key by `PUBKEY`, and `DH_X25519` (0x0D): `id | point` → `shared`, a low-order point refused, never released or signed with, never "verify each use". In V0 the item is bound to the app that minted it; V1 binds it to `crypta:<profile>` when cryptad exists.

**V1 as built (2026-11-18).** Machine scope, WireGuard, IPv4. Where it differs from §3–§5:

- *cryptad runs as root*, not as `_crypta` in a jail: jaild serves only root (as for `arca-courierd`). It parses only the `.conf` (`crypta/crypta-proto/src/conf.rs`, fuzzed: target `conf`; engine frames: target `frames`) and its own profile files in `/var/db/crypta` (no secrets). Its Arca calls go through a child, `atrium-cryptad --arca-key`, running as uid 65052 `_crypta-key` with no other groups (named `cryptad` in `ARCA_SERVICES`, the dispensatord pattern).
- *Keys live in Arca's machine store* under service `cryptad`: `MINT` or `STORE` (an imported `.conf`'s private key) of a `WireGuardKey`, `PUBKEY`, `DH_X25519`; a preshared key is kind `WireGuardPsk` (9), released to cryptad at `up` and handed to the engine in its start frame. The `crypta:<profile>` binding of §4.3 became: cryptad answers each engine's DH requests only with that tunnel's key. The `.conf`'s private key never touches disk; `PostUp`/`PreDown`/`Table`/`SaveConfig` and the like are ignored and reported.
- *The engine* `atrium-crypta-wg` (`crypta/crypta-engine`) runs in a jail from jaild's new `[crypta_engines]` section (name `system-crypta-<uid>`, uids 61000–61499, routed vnet, read-only `/lib`, `/libexec`, `/usr/lib`, `/usr/local/bin`). It has *no device node*: `tun` comes as fd 3 (opened by atrium-net's spawner, `TUNSIFHEAD`), so the devfs ruleset is the plain app one. Its outbound grant is the gateway's address and port, UDP, with a new jaild `tag` field: only a crypta engine jail may set one, and only `CRYPTA_OUTER`. It connects one UDP socket, then enters capability mode. Allowed-IPs are checked both ways. Named endpoints and IPv6 endpoints are refused at import in V1. cryptad restarts a dead engine after 1 s, doubling to 30 s (reset after a minute up).
- *atrium-net* takes `tunnel-open` (answers with the `tun` fd), `tunnel-routes`, `tunnel-apply` and `tunnel-close` on its root socket, checks them (§5.2), and keeps its own copy of the `tun` descriptor, so the routes outlive an engine. A full tunnel is `0/1` + `128/1` through the tunnel's address, with the gateway's `/32` pinned through the LAN router (re-pinned when the egress changes); while it is up, a lease's classless static routes are not applied (TunnelVision) and probing is off. DNS: tunnel domains by `forward_add` (put back after an unbound restart), all names to the tunnel's servers under a full tunnel. One full tunnel at a time; overlapping networks refused.
- *pf*: `etc/atrium.pf` gains `nat-anchor "atrium/vpn"` before the egress `nat`; the anchor's filter rules ride on the existing `anchor "atrium/*"`. atrium-net's spawner loads it with `pfctl -a atrium/vpn -f -` and flushes the physical interface's states when blocking. Per tunnel: apps' `nat` onto `tunN`, the tagged pass to the gateway, every other packet to the gateway dropped, nothing in from `tunN`. A full tunnel adds: IPv6 out the physical interface only for neighbour discovery (decision 8, with or without the block); with the block, nothing else out at all. DHCP goes by BPF and is unaffected.
- *Not in V1*: the captive-portal exception of §5.6, `crypta-ctl` (V3; cryptad's control socket is root-only, `atrium-cryptad ctl import|up|down|delete|set|list`), app traffic through the tunnel is set up (`nat` on `tunN`) but not yet gated.

Gate: `scripts/crypta-v1-gate.sh run`, amd64 VM on atrium-net's fake LAN (`atrium-net/sim/lan.py`, now with a WireGuard gateway relayed to the Ubuntu VM of `crypta-wg-linux-vm.sh`, a per-destination count of everything the guest sends, and a TunnelVision DHCP mode). 32 checks pass: the key helper's credentials; import (the private key only in Arca, `PostUp` ignored); a split tunnel (HTTP to 10.99.0.1, `corp.test` answered by the tunnel's DNS and other names by the LAN's, the default route untouched, the forward removed at `down`); a full tunnel with the block (HTTP to 203.0.113.80, an address only the Ubuntu side has; all DNS through the tunnel; the LAN sees nothing but the tunnel's datagrams); a TunnelVision lease (option 121 with 128/1, 1/8, 203.0.113.0/24) renewed while blocked and not applied; the engine killed with `kill -9` while traffic is sent (nothing out the LAN, the engine back and shaken hands within seconds, traffic again); a full tunnel without the block still sends no IPv6 out the LAN; and controls that the leak counter sees ordinary traffic, that IPv6 flows once the tunnel is down and that the same TunnelVision lease's routes are applied when no tunnel is up. Found on the way and fixed: atrium-lease never learnt the server's MAC since leases bind after the ARP probe, so every unicast renewal was refused and leases lapsed and were re-acquired (atrium-net's own gate counted the broadcast rebind as a renewal); and lan.py's unicast DHCP answers had a bad UDP checksum. Seen once fixed: the kernel answers a unicast DHCP ACK with a port unreachable (atrium-net takes DHCP by BPF and nothing listens on 68), which pf passes as related to the ACK's state even with the block; the gate counts it as DHCP's. A socket bound to 68 in atrium-net would silence it.

**V2 as built (2026-11-19).** Persona scope. Where it differs from §5.3–§5.5 and §9.7:

- *Route domains.* atrium-net raises `net.fibs` to 9 at start and owns tables 1–8, handed out by name on its root socket (`route-domain name=<profile>` answers `domain name= fib=`); the name is the cryptad profile a persona's policy names (`[personas.work] vpn = "work"`). A domain is given its table whether or not its tunnel is up: down, the table holds only the apps' own /30s, so they reach nothing (the kill switch). A profile is scoped with `atrium-cryptad ctl set name= scoped=1`; a scoped tunnel's `tun` is put in the domain's table before its address (`SIOCSIFFIB`), its routes go there on-link, and it adds nothing to the machine: no unbound forwards, no lease routes withheld, no pf block, the machine's default route untouched. A scoped split tunnel's table also gets the egress's subnet (an `AF_LINK` route, since the egress's addresses are in table 0) and its router, moved with the egress. A machine full tunnel and a scoped one may be up together.
- *jaild.* `AllocateNet` and `NetworkConfig::Routed` take `fib` (1–8, and below `net.fibs`; refused for a crypta engine). The host end is put in the table before its address, a table-0 return route to the /30 is added (replies are un-NATed in table 0), and the app's address joins pf table `atrium_fib_<N>`; all of it goes with the epair.
- *portcullisd.* The persona's table, by `route_domain::for_persona(person, persona)`, for: app instances (jail(8) lane), per-instance providers and `[[workers]]` (the served instance's persona, crossings included), the Nuntius fetcher and the CDM host (the calling app's), and workspaces with a persona, with their registries proxy. The captive-portal fetcher stays in table 0. No table from atrium-net refuses the launch; never the host's table instead.
- *DNS, §5.5 deviation.* No second unbound per tunnel. A jail in a domain gets `nameserver 100.64.0.1` (`jaild::protocol::DOMAIN_RESOLVER`), and `atrium/vpn` (now also an `rdr-anchor`) carries `rdr pass on atrium proto {udp tcp} from <atrium_fib_N> to any port 53 -> <the tunnel's first DNS server>`: every lookup of the persona's apps, to whatever server, goes to the tunnel's DNS. A split tunnel with no DNS of its own sends them to the network's server through its egress copy; a full one with none, nowhere. With no redirect (tunnel down) the resolver address is a host address and the base ruleset drops it. So a Work app's names never reach the LAN's servers or the machine's resolver, and nobody else's reach the tunnel's; but a split tunnel's Work apps send all their names to the employer, not only the tunnel's domains.
- *pf base*: `block out quick on atrium_egress inet from any to 100.64.0.0/16`. Seen in the gate: after `down`, the gateway's late answers to the engine's jail (gone) were un-NATed and followed the default route out the LAN.
- *Not in V2*: the persona chip (V2b), IPv6 in a domain (jails have none), per-tunnel domain-only DNS.

Gate: `scripts/crypta-v2-gate.sh run` (the V1 stand; its setup is shared). Two vnet jails stand in for instances side by side, their networks made as portcullisd makes them (`atrium-portcullisd-jclient persona-net`: the policy, atrium-net's domain, jaild's epair), one in Work (`vpn = "work"`, a scoped full tunnel to the Ubuntu peer), one in Personal. 34 checks pass: Work's network in table 1 and its address in `atrium_fib_1`, Personal's in table 0; tunnel down, Work reaches nothing and the LAN sees nothing of it while Personal reaches the LAN; tunnel up, the machine's default route and pf untouched, Work's HTTP to 203.0.113.80 and 10.99.0.1 through the tunnel and served by the peer, its lookups (to its resolver and to the LAN's server) answered by the tunnel's DNS, the LAN seeing only the tunnel's datagrams; Personal and the machine reach neither tunnel address and the peer sees none of their requests, Personal's traffic and lookups go to the LAN; the engine killed with nothing of Work on the LAN and traffic again after; `down` leaves table 1 empty, the redirect gone and Work reaching nothing; release takes Work's address out of the table. The V1 gate still passes.

**V2b as built (2026-11-19).** The route next to the persona's name, drawn by frescod from two files no app writes: cryptad's `/var/run/atrium/vpn-state` (0644, `<profile> up|connecting machine|persona` per running tunnel, written when it changes) and portcullisd's `/var/run/atrium/persona-vpn` (`<person> <persona> <profile>`, written with the persona colours). The focused window's strip chip and every named persona's window tag read `work - via acme`, `work - connecting`, or `work - acme down: no network` in amber (frame too); a persona without its own VPN, and Personal, show the machine tunnel's route when one runs, otherwise `personal - direct` while any VPN is up. frescod rereads the files every 2 s and redraws when they change. Shown with `-` rather than `·`: the strip's baked face has no middle dot. Check: `scripts/vm-personas-check.sh` (amd64 VM booted with `TCG_CPU=Haswell-noTSX` and the fork QEMU) now also sets Work's `vpn` in the policy, checks portcullisd publishes it, and frames the Work window with the tunnel up (Work's blue), down (amber) and connecting; the Personal window stays unframed. Display names for profiles (`Acme VPN`) come with V3's settings page.

**V3 as built (2026-11-19).** `CLASS_VPN` (19) in the crate `crypta/crypta-ctl`, served by `atrium-crypta-ctl`: it binds `/atrium/sockets/vpn-read`, `vpn-use` and `vpn-configure`, opens cryptad's root-only control socket, then becomes `_crypta-ctl` (65054) and enters capability mode; when cryptad goes it exits and daemon(8) (rc.d `atrium-crypta-ctl`) starts it again to wait. Where it differs from §7.2–§7.4:
- Ops: `STATUS`/`SUBSCRIBE` (each tunnel's name, kind, state, scope), `LIST` (adds the block switch and whether it takes all traffic; it stands for `ListProfiles` and `GetProfile`), `CONNECT`, `DISCONNECT`, then for `vpn-configure` `DETAILS` (public key, server, our addresses, how many networks, DNS, bytes), `IMPORT_WIREGUARD` (the file goes to cryptad on a pipe), `CREATE_WIREGUARD` (`AddProfile` for WireGuard: a new cryptad verb `create` has Arca mint the key, and the answer's public key is what the server's admin needs), `SET` (block, persona or machine scope) and `REMOVE`. `SetApps` is not here: which persona uses a tunnel is the person's policy, so it is portcullisd's `SetPersonaVpn` (Settings only, with a press, refused while any of that persona's apps runs, since an instance's routing table is fixed when it starts) and `PersonaVpns`. `AnswerPrompt` waits for the sign-in prompts (V6). Subscribers hear changes made through the front at once and cryptad's own (a handshake, a lost peer) within 2 s.
- Capabilities `vpn-read`, `vpn-use` (system only) and `vpn-configure` (system only, restricted to Settings), mounted like atrium-netconf's.
- Settings' VPN page: a card per tunnel (state, what it carries, for whom; server, our address and key), Connect or Disconnect, the block switch, whole computer or chosen personas with a button per persona, Remove (asks again); a new tunnel from a WireGuard file through the system picker (Curia now embeds `picker`), or a key made in Arca for a server whose details are typed, after which the public key is shown to give the server's admin. When the VPN service is not running the page says so and offers nothing, and keeps asking.
- The bar's VPN item is the bar's own, as its network icon is, not a separate status-item app: a lock, green with a tunnel up, amber while one is connecting, grey when all are down, shown only while there is a tunnel; its menu connects and disconnects each.
- Check: `scripts/crypta-v3-gate.sh run` 37 checks pass on the amd64 VM against Linux WireGuard (the front's uid and capability mode, what each capability may do, a key made in Arca handshaking with Linux, import with PostUp ignored, cryptad away and back, and three jailed apps: `vpn-read`+`vpn-use` connects but cannot configure, `vpn-configure` makes a tunnel, no capability reaches no socket); `ui` frames Settings' VPN page and the bar's menu.

**V4 as built (2026-11-19).** AnyConnect over TLS only (CSTP, no DTLS), plus AnyConnect's SAML sign-in, which §14 left to V6. Where it differs from §9–§10:
- `crypta/crypta-ac`: a strict, bounded XML parser, HTTP/1.1, the config-auth forms (group choice, opaque data, SSO v2) and CSTP framing and pushes. `atrium-crypta-ac`, one jailed engine per tunnel (jaild `crypta_engines`, TCP only to its gateway through a connector child, the only process that may connect): TLS by rustls against the system roots plus the profile's CA file or a pinned certificate; reconnects with the session cookie after a roam and signs in again when it expires; says goodbye to the gateway on Stop.
- Answers to the gateway's forms: a password or one-time code is typed into Arca's field (credential purpose `VpnSecret`) and reaches the engine only as a handle Crypta's key helper redeems (`TAKE_VPN`, `HOLD_FOR_VPN`); a plain-text secret through `answer` is refused. A saved password is Crypta's Arca item, used again without asking; a wrong one is noticed and asked again. The session cookie lives in Arca for cryptad and is forgotten on down; the username and group are kept in the profile.
- Client certificates: Arca makes the P-256 key (`TlsClientKey`, signs only through `SIGN_DIGEST`), cryptad makes the PKCS #10 request Arca signs, Settings saves it through the file picker and takes the signed certificate back the same way; a certificate for another key is refused. Sigillum is not involved yet (no smart-card certificates).
- Posture: the engine refuses a gateway that requires HostScan rather than answering it; no posture data is sent (§10.1). A truthful CSD answer waits for a gateway that needs one.
- Pushes go through atrium-net's checks (§5.2); a hostile push (routes over the LAN, localhost forwards) closes the tunnel, and the refusal is kept for `list`.
- Settings: an AnyConnect profile (host, CA file or pinned certificate, personas), the gateway's forms, banner and errors, the certificate request; the bar shows a sign-in as connecting.
- Single sign-on (SAML, SSO v2): when the gateway offers `sso-v2-login`, Settings shows "Sign in…", which asks portcullisd (`VpnSignInWindow`, with the press) to start the person's Navigator in the reserved persona `vpn`. That Navigator asks `VpnSignInStatus` for the start and final addresses, fetches every page through Arca's couriers in the profile's own jar (partitions `vpn-<profile>.<gateway site>`, one per sign-in, kept apart from browsing for the next sign-in), sends the page's plain forms as courier POSTs only on the person's press (303 becomes GET, 307/308 after a form refused), keeps no trail, store or download, and closes 3 s after the final address opens or after 10 minutes. The gateway's `acSamlv2Token`, set in that jar, never reaches the page: arcad files it as Crypta's item `<profile>/sso@<host>`; cryptad notices it, the engine takes it once and sends it as `<sso-token>` to its own gateway only. Noscript content is read (Keycloak's SAML auto-post "Continue").
- The IdP's password (added 2026-11-19): in a VPN sign-in window the form's first password field is Arca's field, not a line of the form panel. What is typed stays with arca-ui; the panel sends navigatord Arca's handle (`FormField.arca`), never a value, and only for a POST from a VPN window (the portal window refuses one). navigatord's broker rebinds the handle to that form's endpoint and partition (arcad `W_HANDLE_FOR`, only a pending handle the broker's app holds), and the courier puts the value into the form body (`SOURCE_HANDLE`, form-encoded); a spent handle is never retried.
- The jar goes with its profile: cryptad asks arcad to forget a profile's sign-in jar (`VPN_FORGET_JAR`, Crypta only) when it is created and when it is deleted, so a profile made again under the same name starts signed out at the IdP.
- Not yet: sign-ins that need scripts (Okta, Entra), and security keys, passkeys and fingerprints at the IdP (WebAuthn), wait for Navigator's live code (navigator-islands.md step I3; Shankar's choice 2026-11-19, no VPN-only script lane); one secret field per form.
- Check: `scripts/crypta-v4-gate.sh run` 60 checks pass on the amd64 VM against ocserv 1.2.4 in the Ubuntu VM (password with a group, plain-text refused, roam resumes on a reset connection, saved password, wrong password asked again, password plus TOTP, a certificate from Arca's key, a pinned gateway, a wrong pin refused, hostile pushes refused, the password and token in no state or log) and SAML against Keycloak 26 through a stand-in gateway (`scripts/crypta-sso-stand.sh`: ocserv has no SAML, so a small Python gateway checks Keycloak's signed answer with xmlsec1, mints the token and logs in to ocserv behind it): the token reaches Arca through the courier path, the engine signs on with it once, and a replay is refused. The sign-in window end to end: a real navigatord in the `vpn` persona's mode with real courier jails (a private relay on the LAN, since couriers refuse documentation ranges), driven by `sign-in-probe` (navigator-page example) where the window's panel and page child would be: Keycloak's login form goes with the password from Arca's field, the signed answer goes on to the gateway, the tunnel comes up on the window's token, the password is in no log, a second sign-in needs no password (Keycloak's session in the jar), and after the profile is removed and made again the password is asked again. Not covered by the gate: portcullisd's launch of the window and the drawn panel (unit-tested in navigator-ui). Both stand pieces are test tools on the host and VM only.
- Frames: `scripts/crypta-v4-gate.sh ui` (then `click`, `type`, `frame`) shows Settings' sign-ins under a headless frescod with limend and arca-ui: the gateway's username and group, the password in Arca's field, and the single sign-on card. It found Settings crashing when Arca's field was asked for (a RefCell borrow held across the request), fixed.
- Fuzzing: `crypta/crypta-ac/fuzz` (auth/XML, HTTP head and body read whole and split, CSTP deframing read whole and split); 5 minutes each found one split-stream difference in chunk-size lines, fixed (a 256-byte line limit either way).

**V5 direct gateway slice (2026-11-19).** Administrators can provision a direct
GlobalProtect gateway with `atrium-cryptad ctl create-gp name=work host=vpn.example[:port]`.
Optional `user=HEX`, `pin=HEX`, `ca=B64`, and `scoped=0` have the same meanings as for
`create-ac`. Settings can connect and answer prompts for a provisioned profile. Use the
gateway address itself for direct mode. With `portal=1`, Crypta signs in to the
configured portal, lists external gateways in Settings with their host and port,
and asks the user to choose. A different origin gets a fresh jail restricted to
that gateway. Profiles with a portal pin or client certificate can choose the
same origin, or provision the other gateway as a separate direct profile with its
own trust settings. The portal and selected gateway each ask for credentials.
The portal profile does not save passwords or pass cookies or TLS identity to
another origin. An active portal-profile session stays in engine memory.

- `StartGp` uses the existing corporate TLS executable and jaild policy. Gateway
  prelogin, password login, JNLP session parsing, and getconfig use bounded HTTP/XML
  readers after certificate verification. Client key signing and password handles use
  the existing Arca helper. Arca stores the session for reconnect and engine restart.
- The first transport is TLS with IPv4 on tun. The engine checks HIP requirements,
  opens the same gateway's TLS tunnel, and waits for cryptad's atrium-net acceptance
  before handing packets to tun. It refreshes the tunnel using the pushed timeout and
  tries to log out on disconnect. Required HIP, local-network lockdown and split
  exclusions are refused until Atrium can enforce those policies.
- Portal and gateway password sign-in can each ask for a bounded sequence of
  challenge codes through Arca. Crypta accepts the XML and legacy JavaScript-shaped
  and HTML-wrapped challenge envelopes as data, extracting the literal continuation
  token without evaluating page code. The control check against OpenConnect 9.21
  exercises all three envelopes with `--challenge xml|js|html`, selecting the second
  advertised gateway label. The fixture serves both labels at one origin.
- The direct gateway and two-origin portal integration gates pass on arm64
  FreeBSD. The two-origin run (`crypta-gp-packet-check.py --portal-vm arm64`)
  uses distinct TLS identities and passwords, rejects an unadvertised choice,
  verifies the old jail is gone and the new engine is in capability mode, and
  checks Arca for persisted credentials. The peer verifies that portal
  credentials, cookies and tokens stay at the portal. A single gateway login
  carries IPv4 echo before and after rebind, followed by logout.
  Scripted SSO and passkeys remain parked pending Navigator I3.
- HIP submits a host-info XML report when the gateway requests it. Settings
  shows the destination, VPN user, profile hostname, the assigned IPv4
  address, the kernel's actual OS and release, and the Atrium client name.
  The report also includes the host IPv6 forwarding state from the FreeBSD
  kernel and the pf running flag from atrium-net. For disk encryption,
  cryptad counts mounted providers that FreeBSD GEOM reports as active GELI
  and reads the kernel count of mounted authenticated Tessera sealed volumes
  from `kern.tessera.sealed_mounts`. The separate domain count comes from
  `kern.tessera.verified_encrypted_domains` and includes only records the
  kernel authenticates with active group keys. Locked domains remain
  encrypted while outside that count. For patch status, cryptad reads the
  last check and catalog update count from root-owned Opifex over its local
  socket. A failed check reports the failure and omits the count. This covers
  the Opifex catalog. For screen lock, cryptad reads Ostiarius's active seat
  and its lock marker. A marker naming another session leaves lock state
  unknown. Settings shows the same values sent to the gateway. Unknown facts,
  device paths, seat identities, antivirus, hardware IDs and MAC addresses
  are omitted.
  MD5 is the gateway's report identifier. Approval applies to the exact report
  content for that engine and gateway. The HIP timer runs on the active TLS or
  ESP tunnel independently of renewal. The interval defaults to 600 seconds
  and a gateway-supplied interval is clamped to 5–3600 seconds. Unchanged
  reports refresh without prompting. A changed report pauses packet processing
  while Settings waits for approval and keeps the data connection open.
  A rebind during review cancels that review and refetches posture before the
  rebound data connection opens. Each prompt has its own approval choice.
  Settings and cryptad reject an approval from a superseded review.
- The arm64 FreeBSD full gate passed approval (9/9) and decline (5/5) with
  `crypta-gp-packet-check.py --full-vm arm64 --hip approve|decline`. It
  inspects the Settings prompt data before the tunnel is up, checks the XML
  against the reviewed facts and verifies traffic after approval and rebind.
  Decline produces neither a report nor a tunnel. The refresh case
  (`--hip refresh`) compares the first two reports, changes the test guest's
  IPv6 forwarding sysctl, and checks that the changed report reaches the peer
  only after approval. IPv4 echo passes on the original connection both
  before and after refresh. The test restores the sysctl on exit. The arm64
  gate passed 11/11 on TLS and 11/11 with `--esp`. The `--hip posture` case
  mounts a disposable GELI provider and runs Opifex against a seeded catalog
  check. It changes an isolated Ostiarius lock marker during the tunnel,
  verifies that the reviewed facts match the uploaded XML, and checks that
  the changed report requires approval while echo works on either side.
  The `--hip tessera` case stages the host-built module at
  `CRYPTA_GATE_TESSERA_KMOD` and reuses the sealed-volume tools in
  `/root/sealed` on the dedicated guest. It mounts a sealed image during the
  tunnel and checks that the gateway receives the new count only after
  fresh approval. IPv4 echo passes on the original data connection. The
  arm64 Tessera gate passed 10/10 on TLS and 10/10 with `--esp`.
  The `--hip tessera-domain` case uses the same module and the guest's
  Tessera quota tool. With the tunnel up, it assigns an empty quota domain
  to an authenticated key group, locks the key, and reloads it. The peer
  checks the verified count sequence of 0, 1, 0, 1. Each change waits for
  fresh review, including when a previously approved report recurs. IPv4
  echo resumes on the same TLS connection or ESP association after each
  approval. The arm64 domain gate passed 14/14 on TLS and 14/14 with
  `--esp`.
  The `--hip tessera-rebind` variant requests a rebind during the key-lock
  review. It checks that the old approval choice is rejected, the changed
  count reaches the gateway only after fresh approval, and IPv4 echo
  resumes on the rebound TLS connection or ESP association. The arm64
  rebind gate passed 15/15 on each transport.
  The `--hip tessera-rebind-decline` variant declines the replacement
  review. It checks that the profile and its route stay down for a full
  refresh interval, that the changed count never reaches the peer, and
  that the peer sees neither a second TLS data connection nor any ESP
  packet on the newly offered association. The arm64 decline gate passed
  13/13 on TLS and 13/13 with `--esp`.
  The `--hip tessera-reconnect` variant starts the profile again after
  declining the rebound review. It rejects the declined approval choice
  and verifies the route and changed report wait for the new approval.
  After reconnect, the timer must submit an identical locked-key report
  without prompting. Reloading the domain key then requires fresh approval
  before the unlocked count goes to the gateway. IPv4 echo uses the same
  reconnected TLS connection or ESP association through both refreshes.
  After unlock approval, the gate rebinds again. The gateway must receive
  the identical unlocked report without another review, then carry IPv4
  echo over the new TLS connection or ESP association. Locking the key
  again requires fresh approval before the gateway receives the locked
  count. Echo then resumes on that same rebound transport.
  The extended arm64 gate passed 24/24 on TLS and 24/24 with `--esp`.
  The `--hip tessera-relock-decline` variant declines the second lock
  review while the rebound tunnel is active. It verifies that the changed
  count remains unsent, the engine stops, the route is removed, and IPv4
  echo fails after decline. It passed 23/23 on TLS and 23/23 with `--esp`.
  The `--hip tessera-relock-reconnect` variant then explicitly reconnects
  with the key still locked. It rejects the declined choice and verifies
  that the locked report, tunnel route and IPv4 echo wait for fresh approval.
  After the second reconnect, the timer must resend the identical locked
  report without prompting. Unlocking the key again requires fresh approval
  before sending the changed count. Echo after both refreshes uses the
  same reconnected TLS connection or ESP association.
  After approving the unlock, the gate rebinds the second reconnect and
  verifies that the peer receives the identical report without prompting
  before IPv4 echo on the new TLS connection or ESP association.
  The gate then locks the key on that rebound tunnel. It checks that the
  repeated locked count waits for fresh approval and IPv4 echo resumes on
  the same TLS connection or ESP association.
  The extended arm64 case passed 34/34 on TLS and 34/34 with `--esp`.
  The `--hip tessera-final-lock-decline` variant declines that last
  review on the active tunnel. It checks that the locked count remains
  unsent, the tunnel and route go down, and IPv4 echo fails. It passed
  33/33 on arm64 TLS and 33/33 with `--esp`.
  The `--hip tessera-cycle` variant continues after that decline in one
  run per transport. It checks that the declined choice is rejected on
  reconnect, the locked report and traffic wait for fresh approval, the
  identical locked report refreshes without prompting, unlock requires
  fresh approval, and rebind sends the unchanged unlocked report before
  echo resumes on the new data connection.
  The fixture uses the same approval checks for report submission, TLS
  tunnel setup, and ESP traffic. On reconnect and rebind, the fixture
  requires a HIP submission for the newly offered ESP association before
  accepting its first packet.
  The batched arm64 gate passed 42/42 on TLS and 42/42 with `--esp`.
  With `--hip tessera-cycle --drop-esp-after-initial`, the fixture carries
  initial IPv4 echo over ESP and drops authenticated probes on each later
  connection. It checks that each reconnect and rebind falls back to TLS
  while unchanged reports refresh without prompting and changed reports
  wait for fresh approval. The mixed arm64 gate passed 42/42 in one run.
  With `--hip tessera-cycle --recover-esp-on-rebind`, the fixture drops
  probes on the three reconnects and answers probes on their rebinds.
  It verifies IPv4 echo over TLS on reconnect and over the new ESP
  association on rebind, with the same HIP approval rules.
  The arm64 recovery gate passed 42/42 in one run.
  With `--hip tessera-cycle --drop-esp-on-rebind`, the peer answers ESP
  probes on reconnects and drops them on the three rebinds. The gate checks
  IPv4 echo on ESP after reconnect and on TLS after rebind, with the same
  HIP approval rules.
  The arm64 rebind fallback variant passed 42/42 in one run.
  With `--hip esp-loss`, the peer answers the first ESP echo and then
  drops ESP packets. The gate checks automatic TLS recovery on the same
  login, unchanged HIP submission and refresh without prompting, and
  fresh approval before sending changed IPv6 forwarding posture. IPv4
  echo continues on the recovered TLS tunnel. The arm64 run passed 14/14.
  With `--hip esp-loss-decline`, the gate declines the changed posture
  review on the recovered TLS tunnel. It verifies the changed report
  remains unsent, the profile and route go down, and IPv4 echo stops.
  The arm64 decline run passed 13/13.
  With `--hip esp-loss-reconnect`, the gate starts the declined profile
  again. The old approval choice is rejected, the changed report and
  traffic wait for fresh approval, and echo returns on a new ESP association.
  On restored ESP, the timer resends the identical report without
  prompting. Restoring IPv6 forwarding then requires fresh approval before
  the original report can be sent again. Rebind sends that unchanged
  report without another prompt, before IPv4 echo on the new association.
  The extended arm64 reconnect gate passed 21/21.
  The `--hip esp-loss-rebind-decline` variant changes IPv6 forwarding
  again on rebound ESP and declines that review. It checks that the
  changed report stays unsent, the profile and route go down, and
  IPv4 echo stops. The arm64 decline run passed 21/21.
  With `--hip esp-loss-rebind-reconnect`, the gate starts the profile
  after that second decline. It rejects the stale approval choice and
  verifies that the changed report and IPv4 echo wait for fresh approval.
  The peer receives the approved report before the first packet on the
  new ESP association, then receives an identical timer refresh without
  another prompt. IPv4 echo passes on the same association after refresh.
  Restoring IPv6 forwarding on that association prompts for fresh
  approval and withholds the restored report until approval. Echo then
  passes on the same association. The final rebind sends the identical
  restored report without prompting, before traffic on a new ESP
  association.
  The arm64 gate passed 30/30 using the shared Cargo cache.
- GlobalProtect IPv4 ESP uses UDP/4501 on the same peer as HTTPS. The first
  suite is AES-128-CBC with HMAC-SHA1-96, directional associations, random
  IVs and a 64-packet replay window. The engine probes the authenticated path
  before sending tun traffic. Rebind and tunnel renewal get new associations
  from the existing TLS control session. An absent or unsupported offer, a
  failed probe, or a lost ESP path falls back to TLS. The jail grant and
  tagged egress exception cover only TCP to the configured HTTPS port and
  UDP/4501 to that same gateway.
- `crypta-gp-packet-check.py --full-vm arm64 --esp` checks ESP echo before and
  after rebind, with a fresh association for each connection.
  `--full-vm arm64 --drop-esp` drops authenticated probes and checks echo
  through the TLS fallback on both connections. `--esp --openconnect PATH`
  independently checks OpenConnect against the same UDP packet peer. Both
  FreeBSD arm64 gates passed 9/9 and OpenConnect 9.21 carried ESP echo.

V0–V3 give Atrium a complete, leak-tested WireGuard VPN with per-app scope; V4 brings AnyConnect (with its SAML sign-in) in under the same walls; V5–V6 bring GlobalProtect and script-driven sign-ins.

### Questions for Shankar

- Which corporate VPN matters more, AnyConnect or GlobalProtect? That sets V4/V5 order.
  (Answered 2026-11-15: no real company gateway; test locally, §11.)
