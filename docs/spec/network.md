# Atrium network architecture

**Status:** spec, 2026-05-08 (revised; supersedes the
single-mode-enum design from earlier drafts).
**Owner:** atrium-jaild + atrium-netd + future `atrium-net` GUI
mediator daemon.

How Atrium-jailed services participate in networking. Locks in
the jail-side capability model (composable flags, not one-of
modes), the address-allocation policy (curated ranges that
never collide with the user's LAN), the pf rule lifecycle
(synthesized + reconciled by `atrium-netd`), and the role of the
future `atrium-net` GUI-mediator daemon in user-facing network
configuration.

Companion specs:
- [`portcullis.md`](portcullis.md) — manifest schema; the
  `[capabilities] network.*` keys land here.
- [`jaild-policy.md`](jaild-policy.md) — privileged-broker
  allow-list; gates which network capabilities jaild may apply.
- [`atrium-netd.md`](atrium-netd.md) — the daemon that synthesizes
  pf rules from manifest network capabilities and applies them
  to `/dev/pf`.
- [`storage.md`](storage.md) — sibling architecture (mounts +
  storage; capability = mount, just as capability = pf rule
  here).
- [`service-management.md`](service-management.md) §6 —
  `atrium-net`'s place in the GUI-mediator daemon table.

## 1. Principle

> **A jail's network access is a capability the user grants
> per-app at install time. The default is no access. There is
> no host-shared networking; the network is always policy-
> enforced.**

Three immediate consequences:

1. **No `ip4=inherit`** in any production manifest. Inheriting
   the host's network stack defeats isolation; the host's
   firewall rules, listening ports, and routing become attack
   surface for the jailed service.
2. **Default-deny.** A jail without explicit network
   capabilities has no network at all (no IP addresses, no
   routing, no `socket(AF_INET, ...)`). Every connection a jail
   makes must be authorized by a manifest entry.
3. **Default-deny includes inter-jail traffic.** Even with two
   jails on the same lo0 range, neither can reach the other
   unless explicitly authorized via `network.peer_jails` in one
   of their manifests. The kernel pf engine (driven by
   `atrium-netd`) enforces; failure-closed when atrium-netd is
   down.

## 2. Architecture: two daemons + jaild

Three components interact:

| Component | Responsibility |
|---|---|
| **`atrium-jaild`** | Privileged broker: allocates lo0/atrium0 aliases, calls `ifconfig`, applies `ip4.addr` to the jail iovec, tears down on jail removal. |
| **`atrium-netd`** | Event-driven reconciler: reads manifests + jaild state, synthesizes pf anchors from `[capabilities] network.*` declarations, applies via `/dev/pf`, tails `/dev/pflog0` for block events. |
| **`atrium-net`** *(future)* | GUI-mediator: user-facing wifi/VPN/DNS configuration via aqueduct RPC. Owned by D3 / Forum. **Different daemon from `atrium-netd`** (the kernel-pf machinery is event-driven; user-facing config is request-driven; decomposition principle keeps them apart). |

```
┌─── per-app manifest declares [capabilities] network.* ────┐
│                                                            │
└─────┬──────────────────────────────────────────────────────┘
      │
      ▼
┌─── atrium-jaild ──┐    ┌─── atrium-netd ──┐    ┌── /dev/pf ─┐
│ allocates IP      │    │ reads manifests  │    │ kernel     │
│ ifconfig alias    │    │ synthesizes      │───▶│ enforces   │
│ jail_set ip4.addr │    │ pf anchors       │    │ rules      │
└───────┬───────────┘    │ tails pflog0     │    └────────────┘
        │                └──────────────────┘
        ▼
┌─── jail ──────────────────────────────────────────────┐
│  IP alias on lo0 (127.10.x.x) and/or atrium0 (100.64) │
│  pf default-denies everything not explicitly allowed  │
└───────────────────────────────────────────────────────┘
```

`atrium-jaild` and `atrium-netd` are separate daemons in
separate jails (decomposition principle: different domains).
They don't talk over RPC — `atrium-netd` watches `atrium-jaild`'s
state file via kqueue + watches the manifest directory; that's
the entire "interface."

## 3. Address space

Two reserved ranges, both safe-by-default, both
operator-overridable.

### 3.1 lo0 sub-range (`127.10.0.0/16`)

Lives in `127.0.0.0/8` (RFC 1122 reserved loopback). **Cannot**
collide with any external network — loopback is, by definition,
local-only.

Used for: jail-to-jail intra-host communication. A jail with
`network.lo0_alias = true` gets a unique address from the range
(e.g., `127.10.0.5`). Multiple jails with lo0 aliases can reach
each other via lo0; the kernel routes locally between them
(when pf rules permit).

Capacity: 65 K addresses. We will not exhaust this on a single
host.

### 3.2 atrium0 sub-range (`100.64.0.0/16`)

Lives in `100.64.0.0/10` (RFC 6598, reserved for ISP
carrier-grade NAT). Specifically chosen because it's:

- **Not loopback** (so it can be a source IP for outbound
  packets that leave the host)
- **Reserved by RFC** (cannot legitimately appear on home or
  corporate networks)
- **Not 10.x or 192.168.x** (which would clash with most
  consumer routers)

`atrium0` is a **cloned interface** dedicated to jail aliases —
not connected to any physical network, just a routing target.
At boot, atrium-netd ensures it exists:

```sh
ifconfig atrium0 create
ifconfig atrium0 up
ifconfig atrium0 inet 100.64.0.1/16    # gateway address
```

Used for: outbound traffic + LAN-visible inbound. Jails with
`network.host_alias = true` get an alias on `atrium0` (e.g.,
`100.64.0.5`); pf NATs outbound packets to host's real IP on
the way out.

### 3.3 Operator policy

`/etc/atrium/jaild.policy.toml` gains a `[network]` section:

```toml
[network]
# Curated jail address ranges. Defaults are usually safe;
# override only when they collide with host's existing networks
# (atrium-netd refuses to start in that case).
lo0_jail_range          = "127.10.0.0/16"     # default
atrium_outbound_range   = "100.64.0.0/16"     # default

# The cloned interface for atrium0 aliases.
atrium_outbound_iface   = "atrium0"

# Host's outbound interface (NAT source). Auto-detected from
# the default route if unset.
host_outbound_iface     = "em0"

# Whether to inject a static /etc/hosts mapping for known jail
# names into each jail's mount.
inject_host_entries     = true
```

### 3.4 Conflict detection

At startup, atrium-netd:

1. Calls `getifaddrs(3)` to enumerate host interfaces +
   addresses.
2. Reads the kernel routing table via `sysctl net.route...`.
3. For each Atrium range, verifies no host route covers any
   address in it.
4. On conflict: refuses to start; logs a clear error pointing
   at `/etc/atrium/jaild.policy.toml`'s `[network]` section.

Sample failure:

```
ERROR atrium-netd: configured atrium0 range 100.64.0.0/16
overlaps host route 100.64.0.0/24 dev em1.
This host appears to be on a CGNAT segment.

Action: edit /etc/atrium/jaild.policy.toml and set
  [network]
  atrium_outbound_range = "172.31.0.0/16"
or another non-conflicting range; restart atrium-netd.
```

Detection runs once at startup. Runtime route changes (operator
adds a VPN later) are not auto-detected; operator restarts
atrium-netd if needed.

### 3.5 Per-jail address allocation

When jaild creates a jail with `network.lo0_alias = true`:

1. atrium-jaild picks the lowest unused address in the lo0
   sub-range from its in-memory allocation table.
2. `ifconfig lo0 inet 127.10.0.42/32 alias`.
3. The jail's `ip4.addr` iovec entry includes `127.10.0.42`.
4. atrium-jaild persists the allocation in
   `/var/db/atrium/jaild.state.toml` under `[[lo0_allocations]]`.

Symmetric for `network.host_alias = true` — alias on `atrium0`,
allocate from `100.64.0.0/16`.

On jail removal: ifconfig `-alias`, drop the allocation entry.

atrium-netd watches jaild's state file; when allocations
change, it regenerates the affected pf anchors (see §5).

## 4. Capability schema

Manifest's `[capabilities]` block declares network access via
**composable flags**, not a single mode. Defaults to nothing
(no key set = no access).

### 4.1 The full schema

```toml
[capabilities]

# Address allocation
# (jaild allocates the alias; atrium-netd applies pf rules)
network.lo0_alias        = true                # gets 127.10.x.x address
network.host_alias       = true                # gets 100.64.x.x address (atrium0)

# Reachability — outbound
# (atrium-netd writes pf rules)
network.outbound         = "any"                            # NAT to anywhere
network.outbound         = ["github.com:443",               # whitelist destinations
                            "registry.atrium.dev:443"]      # (host:port pairs)

# Reachability — inbound
network.inbound          = [22]                             # listen on these ports
                                                            # (across all aliases)

# Inter-jail peering
network.peer_jails       = ["dev-*"]                        # patterns of jails this
                                                            # may dial
network.peer_ports       = { "dev-*" = [22],                # per-peer port restrictions
                             "atrium-pkg-cache" = [8080] }

# LAN-visible IP (rare; jail looks like a separate device on the LAN)
network.lan_alias        = true                # alias on host's real interface
                                                # (em0 etc.); needs operator-
                                                # configured allow-list

# Inbound port forwarding from host's external IP to a jail port
network.expose           = [{ external = 2222, internal = 22 },
                            { external = 8443, internal = 443 }]
                                                # exposes host:2222 → jail:22
                                                # via pf rdr; needs lan-side
                                                # capability OR host's existing
                                                # public IP, plus user prompt

# mDNS / Bonjour participation (V2 reserved)
network.mdns             = false               # V1: must be false; reserved
                                                # for V2 capability that lets
                                                # jail join host's mDNS group
                                                # for LAN-device discovery

# Specialty: full vnet (rare; for VPN clients, multi-tenant)
network.vnet             = { bridge = "atrium-vnet0" }
```

### 4.2 Field semantics

#### `network.lo0_alias = true`

Allocates a unique 127.10.x.x address; jaild aliases on lo0;
jail's `ip4.addr` includes the alias. The jail can `bind(2)` on
this address. Other jails can connect to it (subject to the
sender's `network.peer_jails` allowing this destination).

Cost: ~zero. lo0 is shared across non-vnet jails; no hardware
involvement; effectively free.

#### `network.host_alias = true`

Allocates a unique 100.64.x.x address; jaild aliases on atrium0;
jail's `ip4.addr` includes the alias. Required for outbound
(jail's source IP must be a non-loopback address) and for LAN-
exposed inbound.

Cost: small (atrium0 alias add); ~zero compared to vnet.

#### `network.outbound = "any" | [...]`

Authorizes outbound NAT. atrium-netd emits pf rules:

```pf
pass out from <jail's atrium0 alias> to ! 100.64.0.0/16 nat-to (em0)
```

`"any"` allows any destination. The list form whitelists by
hostname:port (atrium-netd resolves at rule-synthesis time;
pattern updates trigger anchor regeneration).

Requires `network.host_alias = true` (NAT needs a non-loopback
source). atrium-netd rejects manifests with `network.outbound`
set but no `network.host_alias`.

#### `network.inbound = [ports...]`

Authorizes inbound. atrium-netd emits pf rules allowing inbound
to the listed ports on the jail's aliases.

```pf
pass in proto tcp from any to <jail's lo0 alias> port { 22 }
pass in proto tcp from any to <jail's atrium0 alias> port { 22 }
```

The "from any" is then narrowed by `network.peer_jails` (the
sender's manifest must allow connecting here).

#### `network.peer_jails = [patterns...]`

Glob patterns matching other jail names this jail may dial.
`fnmatch(3)`-style globs (V1: `*`, `?`; V2: character classes
if needed).

When a new jail matching the pattern is created, atrium-netd
re-evaluates and regenerates this jail's anchor automatically.

Without this key, the jail cannot connect to *any* other jail —
not even ones on the same lo0 range. Default-deny.

#### `network.peer_ports = { jail = [ports...] }`

Per-peer port restrictions. Key matches a name or pattern from
`peer_jails`; value is the allowed-ports list.

```toml
network.peer_jails = ["dev-*", "atrium-pkg-cache"]
network.peer_ports = {
    "dev-*"             = [22],
    "atrium-pkg-cache"  = [8080]
}
```

Default if a key is missing from `peer_ports` is "all ports
allowed within the matched peer." Restrict explicitly when the
peer should only be reached for specific services.

#### `network.lan_alias = true`

Allocates a unique IP on the host's *real* interface (`em0`,
typically), making the jail visible as a separate device on the
user's LAN. Other LAN devices can ARP for and connect to the
jail's IP directly — no NAT, no port forwarding.

Cost: real LAN IP consumed; ARP traffic; LAN-visible attack
surface.

Required when: a jail is meant to be reachable from other
machines on the LAN as a service in its own right (a media
server reachable as `atrium-jellyfin.lan`, a Samba share, etc.).

Operator gates this in `jaild.policy.toml`:

```toml
[network.lan_alias]
allowed_iface_addrs = [
    { iface = "em0", range = "192.168.1.200/29" },
]
```

The range is a small slice of the host's network the operator
carves out for jail-LAN aliases. Without this allow-list,
manifests requesting `lan_alias` are rejected at install time.

Most jails will not need this; default-deny in policy is
correct.

#### `network.expose = [{ external, internal }, ...]`

Inbound port forwarding from host's external IP (or `lan_alias`
if the jail has one) to a port inside the jail. atrium-netd
emits a pf `rdr` rule:

```pf
rdr on em0 proto tcp from any to (em0) port 2222 -> 100.64.0.42 port 22
```

Allows the dev jail's sshd (port 22 inside) to be reached as
`<host's-external-IP>:2222` from anywhere reachable. Useful
for: external SSH into a dev jail, public-facing web service
without LAN-visible jail IP, etc.

Each entry is `{ external: u16, internal: u16, proto?: "tcp"|"udp" }`.
`proto` defaults to "tcp."

Validation:

- Requires `network.host_alias = true` OR `network.lan_alias = true`
  (must have somewhere to forward FROM; without an alias, the
  rdr has no consistent destination).
- External ports must be in `policy.network.expose.allowed_external_ports`
  range (operator-curated; default empty = no expose allowed).
- Multiple manifests can't claim the same external port; conflict
  detection at parse time.

Capability prompt at install time shows the full mapping:

```
Network — port forwards:
  host port 2222 (TCP) → vscode:22
  host port 8443 (TCP) → vscode:443
```

User explicitly approves; the prompt can't hide a port mapping.

#### `network.mdns = true` (**V2 reserved; must be false in V1**)

Future capability that allows the jail to participate in the
host's mDNS / Bonjour multicast group. atrium-netd would forward
the jail's UDP socket on port 5353 onto the host's multicast
group; the jail can advertise services AND discover external
LAN devices (printers, AirPlay receivers, smart-home gear).

V1: the field is reserved. atrium-netd rejects manifests with
`network.mdns = true` until the V2 implementation lands.

Why a capability and not just open by default: mDNS exposes
service advertisements to the LAN, which can leak app behaviour
(an app named "scribe-recorder" advertising itself is visible
information). User opts in per app at install time.

#### `network.vnet = { bridge = "..." }`

Specialty escape hatch for jails that genuinely need their own
network stack (VPN clients with their own routes; multi-tenant
isolation). Vnet jails own their internal pf rules; atrium-netd
does **not** synthesize anchors for them. Use sparingly.

### 4.3 Validation rules

atrium-netd validates manifests at load time AND when the
manifest directory changes:

| Check | On failure |
|---|---|
| `outbound` set without `host_alias` | reject manifest; log; jail launches without the rule (effectively no outbound) |
| `inbound` set without `lo0_alias` or `host_alias` | reject (must have somewhere to listen) |
| `inbound` ports overlap with another jail's on the same alias type | reject (one jail per port per alias) |
| `peer_jails` pattern matches own name | warn + ignore that entry |
| `peer_ports` references a peer not in `peer_jails` | warn + ignore that entry |
| `vnet.bridge` doesn't match a configured bridge in jaild policy | reject |

Validation failures surface to the operator at install time
via portcullisd's capability-prompt UI (D3) and via
`atrium-netd-cli reconcile` output for command-line operators.

## 5. PF rule synthesis algorithm

This is the heart of atrium-netd. Each manifest is translated
to one pf anchor; anchors are loaded into the kernel's `atrium/`
namespace.

### 5.1 Anchor naming

```
atrium/jail-<name>          # one per jail, auto-generated
atrium/global               # cross-jail rules (table refs etc.)
```

Host's `/etc/pf.conf` (operator-managed) references the tree:

```pf
# /etc/pf.conf
anchor "atrium/*"
load anchor "atrium/global" from "/var/run/atrium/pf-global.conf"
```

### 5.2 Per-jail anchor template

Pseudo-code, with substitutions for the jail's allocated
addresses + manifest declarations:

```
# anchor "atrium/jail-{name}"
# DO NOT EDIT — auto-generated from /etc/atrium/services.d/{name}.toml
# Last regenerated: <timestamp>
# Manifest hash: <sha256>

# === Outbound NAT (if network.outbound is set) ===
{% if outbound == "any" %}
pass out log (to atrium_block_log) from {atrium0_alias} to ! 100.64.0.0/16 nat-to ({host_outbound_iface})
{% else %}
{% for dest in outbound %}
pass out proto tcp log from {atrium0_alias} to {dest_ip} port {dest_port} nat-to ({host_outbound_iface})
{% endfor %}
{% endif %}

# === Outbound to peer jails (if network.peer_jails is set) ===
{% for peer in resolve_pattern(peer_jails) %}
{% set ports = peer_ports.get(peer.name, "any") %}
pass out proto tcp from {lo0_alias} to {peer.lo0_alias} port {ports}
{% endfor %}

# === Inbound (if network.inbound is set) ===
{% if inbound %}
{% for port in inbound %}
pass in proto tcp from any to {lo0_alias}    port {port}
pass in proto tcp from any to {atrium0_alias} port {port}
{% endfor %}
{% endif %}

# === Default deny (always last in the anchor) ===
block in  log (to atrium_block_log) from any to {lo0_alias}
block in  log (to atrium_block_log) from any to {atrium0_alias}
block out log (to atrium_block_log) from {lo0_alias}    to any
block out log (to atrium_block_log) from {atrium0_alias} to any
```

The `log (to atrium_block_log)` clauses route blocked packets
to a named pflog interface that atrium-netd tails (§7).

### 5.3 Loading anchors atomically

atrium-netd uses `pfctl -a anchor_name -f -` to atomically
replace one jail's rules in a single transaction:

```
pfctl -a "atrium/jail-vscode" -f - <<EOF
[rendered anchor body]
EOF
```

Per-jail loads are **independent** — a syntax error in vscode's
anchor doesn't disturb dev-myproj's. atrium-netd validates the
rendered text via `pfctl -a anchor_name -nf -` (parse-only)
before the live load; if parse fails, the previous anchor
stays in place.

### 5.4 Pattern resolution

`peer_jails = ["dev-*"]` resolves at synthesis time:

1. atrium-netd reads jaild's state file for the current jail
   roster: `[(name, lo0_alias, atrium0_alias)]`.
2. For each pattern, `fnmatch(name, pattern)` to find matches.
3. Generate one `pass out` rule per matched peer's lo0 alias.

When the roster changes (jail added/removed), atrium-netd:

1. Re-resolves all patterns across all manifests.
2. For each manifest whose pattern result changed: regenerate
   that jail's anchor.
3. Atomic per-anchor reload.

Manifests whose pattern result didn't change are skipped (no
unnecessary reloads).

### 5.5 Rule precedence and ordering

Per pf semantics, rules within an anchor are evaluated
top-to-bottom; the last matching rule wins (unless `quick` is
used — atrium-netd doesn't, to keep the rules composable with
operator-side global rules in `pf.conf`).

The template produces "pass" rules first, "block" rules last —
a packet matching a pass goes through; one matching no pass
hits the trailing default-deny.

The host's `/etc/pf.conf` (operator-managed) wraps Atrium
anchors with a permissive default — operator's local rules can
add policy on top (e.g., outbound rate limiting, host-level
firewall, etc.). atrium-netd's anchors enforce the per-jail
slice; the operator's outer rules enforce host-wide policy.

### 5.6 Host pf.conf baseline (fail-closed default)

The pf rule architecture has a subtle race: between
atrium-jaild creating a jail (with its IP allocated and
ifconfig'd) and atrium-netd noticing the state-file change +
loading the per-jail anchor, there's a brief window — typically
microseconds, but worst-case milliseconds during high load —
where the jail exists with no anchor. If host pf.conf is
permissive by default, the jail has unrestricted network access
during that window.

**Mitigation: a fail-closed baseline rule in host pf.conf**
that denies all traffic to/from Atrium jail address ranges
unless an anchor explicitly overrides:

```pf
# /etc/pf.conf — operator-owned, ships with Atrium baseline included

# Atrium baseline: deny all traffic to/from jail address ranges.
# atrium-netd's per-jail anchors layer "pass" rules on top of
# this; without an anchor (e.g., during the create race window
# OR if atrium-netd is offline), the deny stands.
table <atrium_jail_addrs> { 127.10.0.0/16, 100.64.0.0/16 }

block log on any from <atrium_jail_addrs> to any
block log on any from any to <atrium_jail_addrs>

# Per-jail anchors — populated by atrium-netd. Pass rules in
# anchors override the baseline blocks for explicit allows.
anchor "atrium/*"

# Operator's other host-level rules below (rate limiting,
# external firewall, etc.).
```

Properties this gives us:

- **Fail-closed by construction.** atrium-netd offline →
  baseline deny applies → no jail traffic flows. Operator
  notices via service status, can't quietly leak traffic.
- **Race-free jail create.** Anchor not yet loaded → baseline
  deny → jail has no network until atrium-netd catches up.
  Apps fail-startup gracefully (and supervisor restarts them);
  no traffic escapes during the gap.
- **Explicit override only.** Per-jail anchors must
  affirmatively `pass` to override the deny. An empty anchor =
  jail has no network. There's no "default open" path.

The baseline rules ship in a recommended `/etc/pf.conf.atrium`
fragment that operators include (or have included
automatically by an `atrium-pf-baseline` package). The
operator can add their own rules above (host-level firewall)
and below (NAT for the host itself, etc.); the baseline lives
in the middle, atrium-netd's anchor reference comes after.

### 5.7 Anchor isolation across atrium-netd reloads

A loaded anchor stays loaded until atrium-netd explicitly
reloads or removes it. Implications:

- Crashing atrium-netd does not drop existing anchors. Existing
  jails keep their network policy; new jails (created during
  the outage) get default-deny via the baseline (§5.6) until
  atrium-netd restarts and reconciles.
- A misconfiguration in one jail's manifest doesn't affect
  other jails' anchors. Per-jail loading is the unit of
  failure containment.
- Operator can `pfctl -a "atrium/*" -F all` to flush every
  Atrium anchor manually (emergency); atrium-netd recreates
  them on next reconciliation pass (if running) or on its
  next start.

## 6. Lifecycle: jaild + atrium-netd choreography

The two daemons coordinate through the jaild state file (no
RPC). Five events worth tracing:

### 6.1 Jail create

```
1. portcullisd-bootstrap reads manifest
2. portcullisd-bootstrap → atrium-jaild: CreateJail with
   NetworkAlloc { lo0_alias: true, host_alias: true, vnet: None }
3. atrium-jaild: allocates 127.10.0.42 + 100.64.0.42
4. atrium-jaild: ifconfig lo0 inet 127.10.0.42/32 alias
   ifconfig atrium0 inet 100.64.0.42/32 alias
5. atrium-jaild: jail_set with ip4.addr=[127.10.0.42, 100.64.0.42]
6. atrium-jaild: writes new entry to state.json (atomic replace)
7. atrium-netd: kqueue notifies state.json changed
8. atrium-netd: reads new jail entry; reads its manifest
9. atrium-netd: synthesizes anchor "atrium/jail-{name}";
   pfctl -a -f - ...
10. atrium-netd: re-resolves patterns; regenerates any anchor
    in another manifest that referenced the new jail by pattern
    (e.g., vscode's dev-* now matches a new dev-myproj)
```

Step 10 is what makes pattern-based grants work transparently.

### 6.2 Jail destroy

```
1. portcullisd → atrium-jaild: RemoveJail
2. atrium-jaild: kills jail + removes alias entries
3. atrium-jaild: writes new state.json (without this jail)
4. atrium-netd: kqueue notifies
5. atrium-netd: removes anchor "atrium/jail-{name}"
   pfctl -a -F all ...
6. atrium-netd: re-resolves patterns; regenerates affected
   peer anchors (vscode's anchor loses the dev-myproj rule)
```

### 6.3 Manifest edit

```
1. operator/portcullisd-bootstrap modifies a manifest
2. atrium-netd: kqueue notifies manifest dir changed
3. atrium-netd: re-reads modified manifest;
   compares manifest_hash in state file
4. atrium-netd: regenerates anchor with new rules
5. atrium-netd: atomic pfctl reload
```

Manifest changes that affect peer_jails patterns also trigger
re-resolution across all peer anchors (same as 6.1 step 10).

### 6.4 atrium-netd restart

```
1. atrium-netd starts; reads its persistent state file
   /var/run/atrium/netd.state.toml
2. compares with current manifests + jaild state
3. for any drift: regenerates the affected anchors
4. running anchors that no longer correspond to a manifest
   get removed
```

If atrium-netd crashes between steps 2 and 4, the kernel
anchors stay loaded. Restart picks up where it left off.

### 6.5 atrium-jaild restart

atrium-jaild's state.json contains the current allocations.
On atrium-jaild restart it re-reads the state and re-applies
ifconfig aliases for any jail still alive (per the existing
jaild reconciliation logic). atrium-netd's anchors are
unaffected; they reference IPs, not state file pointers.

### 6.6 Hot-reload semantics for in-flight TCP

When atrium-netd reloads an anchor (manifest edit, peer pattern
re-resolution, jail roster change), pf's rule replacement is
atomic — but **existing TCP connections are not killed**.
This is pf's standard behavior, and it's the right default:

- A new pf rule that *would* have blocked an in-flight
  connection doesn't terminate it; the connection stays open
  until either side closes it.
- A new pf rule that *would* have allowed a connection
  doesn't apply retroactively — packets in flight at the
  moment of the rule swap follow the old rules; from the next
  packet onward, the new rules apply.

For the typical Atrium use case, this is right:

- Editing vscode's manifest to add a new dev-* peer doesn't
  disrupt vscode's existing connections to other peers.
- Removing a peer from `peer_jails` doesn't kill in-flight
  connections to that peer; they drain naturally as TCP
  sessions end. New connection attempts will fail.
- A jail being destroyed has its IP de-aliased; in-flight
  connections still associated with that IP get RST from the
  kernel (no destination); apps see EHOSTUNREACH on their
  next read/write.

If an operator needs to *kill* in-flight connections (security
incident, etc.), `pfctl -k` does it explicitly. atrium-netd
does **not** issue per-anchor-reload kills; the operator opts
in to that behavior when needed.

This matches what operators expect from any pf-driven system;
no novel semantics here, but worth documenting because the
"manifest edit" UX implies an immediate effect that's only
true for *new* connections.

### 6.7 Operator-driven manifest reload

An operator changing a manifest while a jail is running:

```sh
$ vim /etc/atrium/services.d/vscode.toml      # add a new peer pattern
$ # save and exit
```

atrium-netd's kqueue notices `NOTE_WRITE` on the directory.
After a 250ms debounce, it re-reads the modified manifest,
verifies it parses + validates, regenerates the affected
anchor, atomically reloads via `pfctl -a -f -`. The operator
sees no command output — the change just takes effect. To
verify:

```sh
$ atrium-netd-cli show vscode
[... regenerated anchor with new peer pass-rule ...]
```

If the manifest fails validation, atrium-netd logs the error
and leaves the previous anchor in place. The operator notices
via:

- `atrium-netd-cli reconcile` (forces re-read; reports errors)
- `tail /var/log/atrium/netd.log` (structured error entries)
- `sysctl kern.atrium.netd.last_error` (most-recent-failure ring)

## 7. Block events: pflog → atrium-notify

When pf blocks a packet matched by `block ... log`, the kernel
writes a record to `pflog0` (BPF-format tap). atrium-netd:

1. Opens `/dev/pflog0` for reading via BPF ioctls.
2. Parses each record (rule label encodes the originating
   jail name, the action, the addresses, the port).
3. Categorizes: "vscode → atrium-pkg-cache:8080 blocked
   (vscode's manifest doesn't grant peer_jails for that name)".
4. Emits an aqueduct event on a future `atrium-notify` channel
   (V2 — when D3 / Forum lands).
5. V1 fallback: writes structured records to
   `/var/log/atrium/network-blocks.log` and a kqueue-readable
   ring buffer at `kern.atrium.netd.blocks_recent`.

The user-facing message goal:

```
🔒 vscode tried to connect to atrium-pkg-cache (10.42.0.7:8080)
   but isn't allowed.

   Add atrium-pkg-cache to vscode's network.peer_jails
   capability if this is intended.
```

Block events are advisory — they tell the user why something
isn't working. The pf rule already enforced the block; the
notification is so the user can fix it.

## 8. DNS / hostname resolution

Each jail needs to resolve names: `github.com`, peer jails by
short name (`dev-myproj`), maybe `localhost`. Three tiers:

### 8.1 Static jail-name → IP mapping (V1)

atrium-netd injects a `/etc/hosts.atrium` file into each jail
at create time, bind-mounted at `/etc/hosts`:

```
127.10.0.5    vscode             # this jail (self)
127.10.0.42   dev-myproj         # peer in our peer_jails
127.10.0.43   dev-otherproj      # peer
100.64.0.42   dev-myproj.lan     # alternative on atrium0
```

Auto-generated; updated when jail roster changes.

The mount lives at `/etc/hosts` inside the jail; the jail's
glibc/musl/whatever resolves name lookups through this file
first.

### 8.2 Host's `/etc/resolv.conf` (V1)

For external resolution (cargo fetch, pkg install), each jail's
`/etc/resolv.conf` is bind-mounted from host's read-only. Host
runs `unbound` or whatever the operator chose; jails inherit.

### 8.3 atrium-resolved (V2; future)

A jailed resolver service that other jails point to via
`nameserver 127.10.0.4`. Provides per-jail DNS scoping (one
jail can be configured to use a different upstream than
another), DNS-over-TLS support, jail-name → IP resolution
without `/etc/hosts` injection. Out of V1 scope; spec'd in
its own future doc.

## 9. atrium-net (future GUI mediator)

Distinct from atrium-netd. atrium-net is the user-facing
network configuration daemon: WiFi pickers, VPN clients, DNS
settings. Lives in service-management.md §6's mediator-daemon
table.

### 9.1 Domain ownership

`atrium-net` owns:
- Wireless association (`wpa_supplicant` config, scan, join)
- DHCP client lifecycle (`dhclient` per interface)
- Static IP configuration for host interfaces
- Default gateway and routing
- `/etc/resolv.conf` write side
- VPN tunnel lifecycle (OpenVPN, WireGuard)

`atrium-net` does NOT own:
- pf rule lifecycle for jails (that's atrium-netd)
- jail address allocation (that's atrium-jaild)
- per-jail mount syscalls (that's atrium-jaild)

Two daemons because: atrium-netd is event-driven reconciliation
of system state from manifests; atrium-net is request-driven
RPC for human-in-the-loop changes. Different domains;
decomposition.

### 9.2 Wire protocol sketch

aqueduct CLASS_NET (= 8, future allocation):

```rust
pub enum Request {
    Ping,

    // Read-side (widely allowed)
    ListInterfaces,
    ListWifiNetworks,
    GetCurrentRouting,
    GetActiveVpns,

    // Write-side (capability-gated per atrium.toml)
    JoinWifi { ssid: String, password: SecretString },
    SetStaticIp { iface: String, addr: String, gateway: Option<String> },
    StartDhcp { iface: String },
    SetDnsResolvers { resolvers: Vec<String> },
    AddVpnTunnel { config: VpnConfig },
    RemoveVpnTunnel { name: String },
}
```

Per-app capability fields:

```toml
[capabilities]
network_read       = true     # ListInterfaces, GetCurrentRouting
network_wifi       = true     # JoinWifi
network_static_ip  = false    # SetStaticIp / StartDhcp
network_vpn        = false    # AddVpnTunnel
network_dns        = false    # SetDnsResolvers
```

D3 / Forum will ship the capability-prompt UI; until then,
atrium-net is a future spec (file exists; daemon doesn't).

## 10. jaild protocol additions

Replaces the old `NetworkConfig` enum with a richer
`NetworkAlloc` that captures the cap-flag model:

```rust
pub struct CreateJailRequest {
    // ... existing ...
    #[serde(default)]
    pub network: NetworkAlloc,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NetworkAlloc {
    /// Allocate a 127.10.x.x address on lo0; jail's ip4.addr
    /// includes it.
    #[serde(default)]
    pub lo0_alias: bool,

    /// Allocate a 100.64.x.x address on atrium0; jail's
    /// ip4.addr includes it.
    #[serde(default)]
    pub host_alias: bool,

    /// Reserved for vnet (rare; full network namespace).
    /// None = not vnet.
    #[serde(default)]
    pub vnet: Option<VnetSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VnetSpec {
    pub bridge: String,             // operator-named bridge
    pub addr:   Option<String>,     // optional static IP
    pub gateway: Option<String>,    // optional default route
}
```

`pf rule application` lives in atrium-netd, not jaild — the
`NetworkAlloc` field tells jaild what to *allocate*, not what
the jail can *reach*. Reachability is the manifest's
`[capabilities] network.*` keys, consumed by atrium-netd.

### 10.1 jaild policy gating

`/etc/atrium/jaild.policy.toml`:

```toml
[network]
allow_lo0_alias    = true    # default-on
allow_host_alias   = true    # default-on for outbound jails
allow_vnet         = false   # default-off; opt-in per deployment

[network.vnet]
allowed_bridges = ["atrium-vnet0"]   # only used if allow_vnet = true
```

A `NetworkAlloc` requesting `vnet = Some(...)` against a policy
with `allow_vnet = false` is rejected at jaild's validator.
Symmetric for the other flags.

## 11. Observability and logging

atrium-netd surfaces three classes of operator-facing signal:

### 11.1 Structured log file

`/var/log/atrium/netd.log` — JSON Lines, one record per event:

```jsonl
{"ts":"2026-05-08T12:34:56Z","level":"info","event":"anchor_loaded","jail":"vscode","manifest_hash":"sha256:abc..."}
{"ts":"2026-05-08T12:35:01Z","level":"info","event":"jail_added","name":"dev-myproj","lo0":"127.10.0.42"}
{"ts":"2026-05-08T12:35:01Z","level":"info","event":"pattern_resolved","jail":"vscode","pattern":"dev-*","matches":["dev-myproj"]}
{"ts":"2026-05-08T12:35:01Z","level":"info","event":"anchor_reloaded","jail":"vscode","reason":"pattern_added","took_ms":3}
{"ts":"2026-05-08T12:36:14Z","level":"warn","event":"block","jail":"vscode","src":"127.10.0.5","dst":"127.10.0.99","port":80,"protocol":"tcp"}
{"ts":"2026-05-08T12:37:02Z","level":"error","event":"manifest_invalid","path":"/etc/atrium/services.d/atrium-experimental.toml","detail":"network.outbound set without network.host_alias"}
```

Rotation: standard newsyslog(8) policy. Operator configures.

### 11.2 sysctls

For low-overhead operator inspection without parsing logs:

```
kern.atrium.netd.anchors_active        # u32; current anchor count
kern.atrium.netd.anchors_total         # u64; lifetime count of loads
kern.atrium.netd.reloads_total         # u64; lifetime count of reloads
kern.atrium.netd.blocks_total          # u64; lifetime count of pf blocks
kern.atrium.netd.blocks_recent         # ring buffer (text dump):
                                       #   "vscode→atrium-pkg-cache:8080 (denied)"
                                       #   "vscode→evil.example.com:443 (denied)"
                                       #   ... (last 64 entries)
kern.atrium.netd.last_error            # most recent error: string
kern.atrium.netd.last_reconcile_ms     # last reconciliation pass duration
kern.atrium.netd.uptime_seconds        # since atrium-netd started
```

Same shape as the existing tessera observability sysctls.

### 11.3 atrium-netd-cli

CLI for richer queries:

```
$ atrium-netd-cli list
JAIL                  LO0           ATRIUM0       ANCHOR  STATUS
vscode                127.10.0.5    100.64.0.5    loaded  active (3 peer rules)
dev-myproj            127.10.0.42   100.64.0.42   loaded  active
atrium-pkg-cache      127.10.0.7    100.64.0.7    loaded  active

$ atrium-netd-cli show vscode
# anchor "atrium/jail-vscode"
# generated 2026-05-08T12:35:01Z from
#   /etc/atrium/services.d/vscode.toml (sha256:abc...)
#
pass out from 127.10.0.5  to ! 127.10.0.0/16   nat-to (em0)
pass out from 100.64.0.5  to ! 100.64.0.0/16   nat-to (em0)
pass out proto tcp from 127.10.0.5 to 127.10.0.42 port 22  # dev-myproj
pass in  proto tcp from any        to 127.10.0.5  port { 22 }
block in  log from any to 127.10.0.5
block in  log from any to 100.64.0.5
block out log from 127.10.0.5  to any
block out log from 100.64.0.5  to any

$ atrium-netd-cli why-blocked vscode
[2026-05-08T12:36:14Z] vscode → 127.10.0.99:80
  reason: 127.10.0.99 doesn't match any of vscode's network.peer_jails
          patterns (currently: dev-*, atrium-pkg-cache)

[2026-05-08T12:37:48Z] vscode → 8.8.8.8:53
  reason: vscode has network.outbound = ["github.com:443",
          "registry.atrium.dev:443"]; 8.8.8.8:53 not whitelisted.

$ atrium-netd-cli stats
Anchors active:          7
Reloads (lifetime):      42
Reloads (last hour):     3
Blocks (lifetime):       18
Blocks (last hour):      2
Last reconcile:          2026-05-08T12:35:01Z (took 4ms)
Uptime:                  6d 2h 14m

$ atrium-netd-cli ranges
[network]
lo0_jail_range           = 127.10.0.0/16   (allocated: 7/65534)
atrium_outbound_range    = 100.64.0.0/16   (allocated: 5/65534)
atrium_outbound_iface    = atrium0          (up; bridge gateway 100.64.0.1)
host_outbound_iface      = em0              (up; 192.168.1.42/24)

$ atrium-netd-cli reconcile
re-reading manifests...   done (12 manifests; 0 errors)
re-resolving patterns...  done (7 anchors checked, 1 needed reload)
loading anchors...        done (atrium/jail-vscode reloaded)
```

`atrium-netd-cli` reads atrium-netd's state file and pf state
directly; doesn't need RPC to atrium-netd.

### 11.4 What we deliberately don't expose

- **Per-jail traffic byte/packet counters.** pf supports them
  (label rules with `tag` or `count`); atrium-netd doesn't
  enable by default. Counters cost CPU on each packet. V2
  operator-opt-in via a `kern.atrium.netd.count_traffic =
  true` sysctl. For now, operators wanting per-jail accounting
  use `pfctl -ss` directly.
- **Connection-table inspection.** Same — `pfctl -ss`
  authoritative; atrium-netd doesn't duplicate.
- **Historical block-event analytics.** netd.log is the
  source of truth; analytics is the operator's call (Loki,
  ELK, whatever). atrium-netd doesn't ship a query engine.

The principle: atrium-netd surfaces *what its own state is*
(anchors, blocks, errors) and delegates everything kernel-side
to existing FreeBSD tools (`pfctl`, `tcpdump`, `netstat`).

## 12. Install-time capability prompt

When portcullisd validates an app's manifest at install time,
the network capabilities surface in the prompt UI alongside
graphics, clipboard, filesystem grants:

```
Install vscode?

  Graphics:           Fresco
  Clipboard:          read + write
  Notifications:      yes
  Filesystem:         ~/code (read/write)
                      ~/.ssh (read)
  Network:            outbound (any)
  Network — peers:    dev-* (any port)
                      atrium-pkg-cache (port 8080)

  [ Allow ]  [ Deny ]
```

Pattern-based grants (`dev-*`) are explicit — the user sees
"this app may dial any of your dev jails." If they're
uncomfortable, they Deny.

Once granted, the pattern remains; new jails matching the
pattern are auto-included without re-prompting (atrium-netd
regenerates the anchor automatically per §6).

V1 prompt UI is V0 stub — capability-prompt is D3 / Forum work.
For V0, capability validation happens at manifest-install
(text output of `atrium-pkg install`); user accepts via CLI
flag.

## 13. Open questions / future work

1. **IPv6.** This spec is IPv4-centric. Real IPv6 means ULA
   range allocation (`fc00::/7`), separate pf rules, separate
   ranges (no equivalent of 100.64.0.0/10). Symmetric work,
   separable concern; punt to V2.
2. **Captive portal handling.** When DHCP returns a captive
   portal, how does the user sign in? atrium-net opens a
   small browser jail with the portal's origin? UX design
   open.
3. **Per-jail traffic accounting.** pf counters per anchor;
   atrium-netd could expose them as sysctls. Future
   `atrium-monitor` daemon territory.
4. **VPN-as-default-route.** A user enabling a VPN via
   atrium-net should make its tunnel the default route for
   everything. UX + atrium-net + atrium-netd interaction
   open.
5. **Hostname-based outbound whitelist.** `network.outbound =
   ["github.com:443"]` requires DNS resolution at rule-
   synthesis time. What if the IP changes? Refresh on TTL
   expiry? Currently atrium-netd resolves once at synthesis
   and caches; explicit reconcile to refresh. Could add
   periodic refresh in V2.
6. **`network.peer_jails` ABI for prompts.** When the user
   creates a new dev-* jail, vscode's pattern silently picks
   it up. Is that the right UX? Some users might want a
   notification ("vscode will now be able to reach
   dev-newproj — accept?"). V2 if real friction emerges.
7. **Multi-interface hosts.** Laptop with WiFi + ethernet,
   docking-station scenario. Default route changes; outbound
   NAT source changes. atrium-netd needs to react to host
   route changes (V2).

## 14. Implementation order

When work resumes, network is a real engineering arc. Roughly:

| Stage | Goal | Estimate |
|---|---|---|
| 1 | jaild `NetworkAlloc` field + lo0_alias/host_alias allocation; in-memory tracking. | 4 days |
| 2 | atrium0 cloned interface bring-up + idempotent address assignment. | 2 days |
| 3 | jaild policy gates for `[network]` section. | 1 day |
| 4 | atrium-netd: skeleton, manifest reader, state-file watch via kqueue. | 3 days |
| 5 | atrium-netd: pf anchor synthesis (the heart); per-template rendering. | 1 week |
| 6 | atrium-netd: pattern resolution + reconciliation on jail roster changes. | 3 days |
| 7 | atrium-netd: pflog0 reader + structured block log. | 4 days |
| 8 | DNS / hostname injection per-jail. | 2 days |
| 9 | atrium-netd-cli (operator inspection). | 2 days |
| 10 | rc.d service script; ordering with the rest. | 1 day |
| 11 | End-to-end VM smoke: vscode↔dev jail SSH; deny path; manifest-edit reload. | 4 days |

Total: ~5-6 weeks focused. Most weight in stages 5 and 7.

The atrium-net GUI mediator is a separate work item (D3 /
Forum); not included above.

## 15. References

- [`atrium-netd.md`](atrium-netd.md) — the daemon that owns pf
  rule lifecycle (§§3-7 of this spec).
- [`portcullis.md`](portcullis.md) — manifest schema where
  `[capabilities] network.*` lives.
- [`jaild-policy.md`](jaild-policy.md) — operator policy file
  for `[network]`.
- [`storage.md`](storage.md) — sibling architecture (capability
  = mount; this spec's analogue is capability = pf rule).
- [`service-management.md`](service-management.md) §6 —
  atrium-net's mediator-daemon role.
- [`LANGUAGE-POLICY.md`](../LANGUAGE-POLICY.md) — Rust policy.
- RFC 1122 — `127.0.0.0/8` reserved loopback.
- RFC 6598 — `100.64.0.0/10` reserved for carrier-grade NAT.
- FreeBSD `pf(4)`, `pfctl(8)`, `pflog(4)`.
- FreeBSD `if_clone(9)` — interface cloning (atrium0).
