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

## 11. Install-time capability prompt

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

## 12. Open questions / future work

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

## 13. Implementation order

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

## 14. References

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
