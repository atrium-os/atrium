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
| Disk encryption | storage (Tessera / GELI volume status) | encrypted volumes, true state |
| Patch management | Opifex (last update check, pending updates) | true state |
| Screen lock | Vestibulum policy | true state |
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
- **GlobalProtect:** no open-source server exists, and PAN-OS VM-Series is paid. A small
  Rust stand-in gateway (portal, gateway, HIP and ESP endpoints) built from the protocol
  notes. So the stand-in is not just our own reading checked against itself, OpenConnect's
  GlobalProtect client (`openconnect --protocol=gp`, a test tool only, never shipped) must
  connect to it too: that client is proven against real gateways. The same cross-check
  applies to ocserv with our AnyConnect engine.
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
| V2 | Persona scope: per-tunnel FIB, jaild epair FIB, helper inheritance, per-tunnel resolver; per-(app, persona) instances in portcullisd; workspaces with a persona | app scope gate (§11) passes; a Work and a Personal workspace side by side, captures show only Work's traffic in the tunnel |
| V2b | The route in the persona chip (personas.md P1 first) | frames of Work and Personal windows with the tunnel up, connecting and down |
| V3 | `CLASS_VPN` + crypta-ctl, Curia VPN page (generate key, import file, apps list, block switch), Forum item | Curia frames of each; an app without `vpn-configure` is refused |
| V4 | `crypta-ac` TLS-only: forms through Arca fields (`TAKE_VPN`), saved passwords, client certificates through Arca and Sigillum, posture answer without HostScan | ocserv gate: password, OTP and certificate sign-ins; roam resumes; hostile pushes refused |
| V5 | `crypta-gp`: portal and gateway login, ESP in UDP, SSL fallback, HIP from true facts with the Curia report view | stand-in gateway gate, the stand-in also accepting OpenConnect's GP client |
| V6 | SAML through a Navigator sign-in page and courier token capture | test IdP gate; needs the Navigator live-code lane |
| V7 | On demand: AnyConnect DTLS, IKEv2, more vendors, the `if_wg` fast path | per need |

**V0 as built (2026-11-18).** `crypta/crypta-wg`: the protocol sans-IO (the engine passes datagrams, packets and the time; it gets back what to send, what to hand to `tun`, and events). Noise_IKpsk2 with every static-key DH through a `StaticKey` trait; mac1 checked before any secret is used, cookies under load (`Device`, 256 handshakes a second, or forced), the 2048-bit replay window (a counter up to 1983 below the newest is still taken once), the timers of the paper's §6 (retry every 5 s plus jitter, give up at 90 s and drop what waited, rekey at 120 s or 2^60 messages, reject at 180 s, keepalive after 10 s, a new handshake after 15 s with data unanswered, everything erased after 9 minutes without new keys), the responder waiting for key confirmation, and initiation timestamps that never go back. Checks: the protocol's initial chaining key and hash as test vectors, 19 host tests, interop with boringtun (dev-dependency only) over UDP sockets both ways with a preshared key, cookies and a rekey, and `scripts/crypta-wg-linux-vm.sh` against Linux kernel WireGuard in an Ubuntu 24.04 VM (we initiate: 20 pings of growing size; Linux initiates: its 5 pings answered; 0 failed). Fuzzers `messages`, `replay` (against a set model) and `device`, each run clean for an hour (497M, 43M and 294k runs) (one peer, real messages changed and replayed, random bytes; nothing delivered that the peer didn't send). arcad: kind `WireGuardKey` (0x104) minted by `MINT`, its public key by `PUBKEY`, and `DH_X25519` (0x0D): `id | point` → `shared`, a low-order point refused, never released or signed with, never "verify each use". In V0 the item is bound to the app that minted it; V1 binds it to `crypta:<profile>` when cryptad exists.

V0–V3 give Atrium a complete, leak-tested WireGuard VPN with per-app scope; V4–V6 bring the
corporate VPNs in under the same walls.

### Questions for Shankar

- Which corporate VPN matters more, AnyConnect or GlobalProtect? That sets V4/V5 order.
  (Answered 2026-11-15: no real company gateway; test locally, §11.)
