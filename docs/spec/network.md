# Atrium network architecture

**Status:** spec, 2026-05-08
**Owner:** D2.5 jaild + future `atrium-net` GUI mediator daemon

How Atrium-jailed services participate in networking. Locks in
the jail-side network capability model, the manifest schema, the
jaild protocol extensions, and the role of the future
`atrium-net` GUI-mediator daemon in user-facing network
configuration.

Companion specs:
- `docs/spec/portcullis.md` — capability manifest
- `docs/spec/jaild-policy.md` — privileged-broker allow-list
- `docs/spec/storage.md` — companion architecture
- `docs/spec/service-management.md` §6 — atrium-net's role as a
  GUI-mediator daemon

## 1. Principle

> **A jail's network access is a capability the operator grants
> at deployment time + the user grants per-app via Portcullis.
> No jail gets host-shared networking; isolation is mandatory.**

Three immediate consequences:

1. **No `ip4=inherit`** in any production manifest. Inheriting
   the host's network stack defeats the isolation; the host's
   firewall rules, listening ports, and routing become attack
   surface for the jailed service.
2. **The default is `disable`.** A jail without explicit network
   capability has no network at all (no IP addresses, no routing
   table, no `socket(AF_INET, ...)` allowed). Override
   per-manifest.
3. **GUI configuration goes through `atrium-net`.** A user's
   Network Manager GUI app cannot call `ifconfig` or write
   `/etc/resolv.conf` directly — it talks to `atrium-net` over
   aqueduct, with a capability-gated permission model.

## 2. Network capability classes

The manifest declares one of these modes per service:

| Mode | What it gives the jail | Use case |
|------|------------------------|----------|
| `disable` (default) | Nothing. No `socket(AF_INET, ...)`. | Most services that don't need the network (e.g., a renderer talking to frescod via aqueduct only). |
| `lo0_alias` | A specific 127.x.x.x address aliased on the host's lo0; jail can bind/connect on it. | Per-jail loopback addresses for inter-jail TCP (e.g., mysqld at `127.10.0.5:3306`). Most common. |
| `vnet` | Own VNET network stack with virtual interface bridged to a host bridge. | Full network isolation: own routing table, own firewall, own listening ports. For services that genuinely need it (router daemons, VPN endpoints). |
| `host_alias` | A real host-IP address aliased on a designated interface. | Service exposes itself on the LAN. Tightly capability-gated. |

The manifest schema:

```toml
[network]
mode = "lo0_alias"
addr = "127.10.0.5/32"

# OR
[network]
mode = "vnet"
bridge = "atrium-bridge0"
addr   = "192.168.42.5/24"
gateway = "192.168.42.1"

# OR
[network]
mode = "host_alias"
interface = "em0"
addr      = "192.168.1.50/24"
```

`mode = "disable"` is the implicit default if `[network]` is
omitted.

## 3. lo0_alias — the common case

The most-used mode for system services. Each service gets a
specific 127.10.x.x address; no two services share an address;
any service that has the right capability can connect to any
other.

### 3.1 Address allocation

`atrium-net` (or, V0, the operator manually) maintains a
per-deployment allocation table:

```toml
# /var/db/atrium/lo0-allocations.toml

[allocations]
"atrium-frescod"      = "127.10.0.1/32"
"atrium-devevents"    = "127.10.0.2/32"
"vestibulum-seat0"    = "127.10.0.3/32"
"mysqld"              = "127.10.0.5/32"
"postgres"            = "127.10.0.6/32"
```

Manifests can either hardcode an address (operator edits
`50-mysqld.toml` to set `addr = "127.10.0.5/32"`) or request "any
free address in this range" via a future `addr = "auto"` shape.
V0 = explicit only.

### 3.2 jaild's role

When a jail with `mode = "lo0_alias"` is created:
1. jaild's pdfork-child runs `ifconfig lo0 inet 127.10.0.5/32 alias`
   to add the alias on the host.
2. jaild's iovec to `jail_set` includes `ip4.addr = 127.10.0.5`.
3. The jail boots; its sole IPv4 address is `127.10.0.5`.
4. Inside the jail, processes can bind to `127.10.0.5:N` and
   connect to other `127.10.0.x` addresses (subject to host
   firewall + the kernel routing).

When the jail is destroyed:
1. jaild runs `ifconfig lo0 inet 127.10.0.5/32 -alias` to remove.
2. The address is free for re-allocation.

### 3.3 Inter-jail connectivity

Two jails on different lo0 aliases can talk to each other —
that's the whole point. Examples:

- Browser jail at `127.10.0.99` connects to mysqld jail at
  `127.10.0.5:3306`. Just works; both addresses are aliased on
  the host's lo0; routing is direct.
- Atrium GUI app uses unix socket via mounted `/var/run/aqueduct/
  some-service.sock` — usually preferable to TCP for
  Atrium-native IPC.

If you don't want a service to be reachable from other jails,
firewall it via pf:

```
# /etc/pf.conf, written and reloaded by atrium-net (future)
table <atrium_lo0> { 127.10.0.0/24 }
block in quick on lo0 from <atrium_lo0> to 127.10.0.5 port != 3306
pass  in quick on lo0 from <atrium_lo0> to 127.10.0.5 port = 3306
```

`atrium-net` owns generating + reloading `pf.conf` based on
inter-service capability declarations. V0 = manual operator
config.

## 4. vnet — full network isolation

Each VNET-jail has its own complete network stack: own routing
table, own firewall rules, own listening ports, own loopback.
Used when:

- The service is a network daemon that genuinely needs to own a
  network namespace (router, VPN, DHCP server, NAT).
- Multiple instances must listen on the *same* port number
  without conflict.
- You want per-jail firewall rules without operator-side pf
  scripting.

### 4.1 Bridge setup

VNET jails need a bridge interface on the host. Operator (or
`atrium-net`) sets this up at install time:

```sh
ifconfig bridge create name atrium-bridge0
ifconfig atrium-bridge0 addm em0 up         # if bridging to LAN; or
ifconfig atrium-bridge0 inet 192.168.42.1/24 up   # for NAT'd local
```

### 4.2 jaild's role

For `mode = "vnet"`:
1. Create a `epair` interface pair on the host:
   `ifconfig epair create` → returns `epairNa` and `epairNb`
2. Add `epairNa` to the bridge: `ifconfig atrium-bridge0 addm epairNa`
3. `ifconfig epairNa up`
4. Pass `epairNb` to the jail via `jail_set` parameters
   `vnet=new`, `vnet.interface=epairNb`.
5. Inside the jail, `epairNb` is the only NIC; jail configures
   it (typically the manifest's exec or an init script does
   `ifconfig epairNb 192.168.42.5/24` + `route add default
   192.168.42.1`).

When the jail dies:
1. jaild destroys the epair: `ifconfig epairNa destroy`
2. Bridge auto-cleans up its membership.

### 4.3 jaild policy gating

VNET requires `allow.mount.zfs`-class privilege escalation in
some failure modes; the policy file gates which manifests can
request it:

```toml
# /etc/atrium/jaild.policy.toml

[network]
allow_disable     = true
allow_lo0_alias   = true
allow_vnet        = true
allow_host_alias  = false   # extra-conservative; flip per deployment

[network.vnet]
allowed_bridges = ["atrium-bridge0"]
```

A manifest requesting `mode = "vnet"` against a policy with
`allow_vnet = false` is rejected at manifest-validate time.

## 5. host_alias — the rare case

Aliases a real LAN-routable IP onto a designated host interface,
binds the jail to it. Useful for "this jail is the SMTP server
for the LAN; people connect to 192.168.1.50:25." Aggressive use
of this mode reduces jail isolation (other LAN traffic might
target the host's IPs); default policy refuses it.

If allowed, jaild's mechanic is the same as `lo0_alias` but on
the operator-named interface (`em0`, etc.) and with the
capability-gated allow-list of host IP ranges.

## 6. jaild protocol changes

`CreateJailRequest` grows a `network` field:

```rust
pub struct CreateJailRequest {
    // ... existing ...
    #[serde(default)]
    pub network: NetworkConfig,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NetworkConfig {
    #[default]
    Disable,
    Lo0Alias { addr: String },
    Vnet     { bridge: String, addr: String, gateway: Option<String> },
    HostAlias { interface: String, addr: String },
}
```

jaild's validator:

- `Disable`: always permitted.
- `Lo0Alias`: addr must match an entry in
  `policy.network.allowed_addrs_on_lo0` (CIDR check).
- `Vnet`: requires `policy.network.allow_vnet = true`; bridge
  must be in `policy.network.vnet.allowed_bridges`.
- `HostAlias`: requires `policy.network.allow_host_alias = true`;
  interface + addr must match an allow-list (per
  `policy.network.host_alias.allowed_iface_addrs`).

jaild's pdfork-child applies the network setup before
`jail_set+attach`:
- `Disable`: nothing to do; default is `ip4=disable`.
- `Lo0Alias`: `ifconfig lo0 inet <addr> alias`, then jail iovec
  carries `ip4.addr=<addr>`.
- `Vnet`: epair create, bridge add, jail iovec `vnet=new
  vnet.interface=epairNb`.
- `HostAlias`: `ifconfig <iface> inet <addr> alias`, then jail
  iovec `ip4.addr=<addr>`.

On destroy, jaild's existing per-jail teardown logic gains a
network-cleanup step (de-alias, destroy epair).

## 7. DNS resolution

A jail with network access still needs DNS to resolve names.
Three options:

1. **Mount the host's `/etc/resolv.conf` ro into the jail.**
   Simple. Works as long as the host's resolver works. Default
   for V0.
2. **Run a per-jail caching resolver.** Each jail gets its own
   `unbound-anchor` listening on the jail's lo0. More work,
   isolates DNS state. V1 if needed.
3. **Atrium-wide resolver (`atrium-resolved`).** A jailed
   resolver service that other jails point to via `nameserver
   127.10.0.4` or whatever. Future `atrium-net` mediator
   territory.

For V0: option 1. Per-app `[capabilities]` declares
`dns = "host_resolver"` (which mounts /etc/resolv.conf), or
`dns = "atrium-resolved"` (which writes the manifest's
nameserver line in the jail's resolv.conf at jail-create time).
Default `dns = "none"` (no resolution; service must use IP
addresses or its own resolution).

## 8. atrium-net — the GUI mediator daemon

Per `service-management.md` §6, network configuration tools
(WiFi pickers, VPN clients, DNS settings) run jailed and cannot
call `ifconfig` directly. They reach `atrium-net` over aqueduct,
which performs the privileged operations.

### 8.1 Domain ownership

`atrium-net` owns:
- Wireless association (`wpa_supplicant` config, scan results,
  joining networks).
- DHCP client lifecycle (`dhclient` per interface).
- Static IP configuration for host interfaces.
- Default-gateway and routing.
- `/etc/resolv.conf` write side (when not using
  `atrium-resolved`).
- Generating + reloading `pf.conf` based on inter-jail
  capability rules.
- VPN tunnel lifecycle (OpenVPN, WireGuard).
- Per-jail lo0 alias allocation table at
  `/var/db/atrium/lo0-allocations.toml`.

`atrium-net` does NOT own:
- Per-jail mount syscalls (jaild does, including
  `AttachMount` for runtime-attached network filesystems like
  NFS — see `storage.md` §6.2).
- Routing table reads (any process can `netstat -r`).
- Firewall *enforcement* (pf is in the kernel; atrium-net only
  manages config).

### 8.2 Why a separate daemon

Per the decomposition checklist (`service-management.md` §5):
- Different domain from policy + lifecycle (Q2: yes → separate).
- Network configuration is a privileged operation (run as root,
  jailed); separate-daemon containment limits blast radius.
- A user-facing GUI Network Manager needs a target daemon to
  talk to; reaching portcullisd would scope-creep portcullisd.

### 8.3 atrium-net's wire protocol (sketch)

```rust
pub enum Request {
    Ping,
    // Read-side; widely allowed
    ListInterfaces,
    ListWifiNetworks,
    GetCurrentRouting,

    // Write-side; gated by atrium.toml capability
    JoinWifi { ssid: String, password: SecretString },
    SetStaticIp { iface: String, addr: String, gateway: Option<String> },
    StartDhcp { iface: String },
    SetDnsResolvers { resolvers: Vec<String> },
    AddVpnTunnel { config: VpnConfig },
    RemoveVpnTunnel { name: String },

    // Per-jail allocations (called by portcullisd, not by user GUIs)
    AllocateLo0Alias { jail_name: String } -> { addr: String },
    ReleaseLo0Alias  { jail_name: String },
}
```

Per-app capability fields (in atrium.toml, validated by
portcullisd before allowing the connection to atrium-net):

```toml
[capabilities]
network_read       = true     # ListInterfaces, GetCurrentRouting
network_wifi       = true     # JoinWifi
network_static_ip  = false    # SetStaticIp / StartDhcp
network_vpn        = false    # AddVpnTunnel
network_dns        = false    # SetDnsResolvers
```

A user's GUI Network Manager gets `network_read +
network_wifi`; a generic app gets nothing; a VPN client gets
`network_read + network_vpn`. portcullisd prompts the user
before granting any write-side capability.

## 9. Inter-jail connectivity policy

Even with `lo0_alias`, you may not want every jail to be able
to reach every other jail. Per-pair connectivity is
operator-configurable in `atrium-net` policy:

```toml
# /etc/atrium/network.policy.toml

[allow_inter_jail]
"vestibulum-seat0" -> "atrium-frescod"   # vestibulum can render
"vestibulum-seat0" -> "atrium-devevents" # vestibulum can read input
"user-N-supervisor" -> "atrium-frescod"
"user-N-supervisor" -> "mysqld"          # user can use shared DB

# Default: deny all not listed
default = "deny"
```

`atrium-net` translates these rules into pf:

```
# generated by atrium-net at boot + on policy change
pass in quick on lo0 from 127.10.0.3 to 127.10.0.1 port 0:65535  # vestibulum → frescod
pass in quick on lo0 from 127.10.0.3 to 127.10.0.2 port 0:65535
pass in quick on lo0 from 127.10.0.7 to 127.10.0.1 port 0:65535  # supervisor → frescod
pass in quick on lo0 from 127.10.0.7 to 127.10.0.5 port 3306     # supervisor → mysqld
block in quick on lo0 to 127.10.0.0/24
```

Default-deny posture; explicit allow per pair. V0 is "operator
hand-edits"; V1 is "atrium-net manages atomic reload."

## 10. Jaild policy file additions

Network-capability gating belongs in `/etc/atrium/jaild.policy.toml`:

```toml
[network]
allow_disable     = true     # always permit; default
allow_lo0_alias   = true
allow_vnet        = false    # off by default; opt-in
allow_host_alias  = false    # off by default; opt-in
allowed_addrs_on_lo0 = [
    "127.10.0.0/24",          # the per-jail allocation range
]

[network.vnet]
allowed_bridges = ["atrium-bridge0"]
# allowed_addr_ranges = ["192.168.42.0/24"]   # if we constrain VNET addrs

[network.host_alias]
allowed_iface_addrs = []     # empty = no host_alias; populate per deployment
```

The jaild validator extends to check the network field per these
rules. A manifest requesting `vnet` against a policy with
`allow_vnet = false` is rejected at manifest-validate time
(install time), with a clear message.

## 11. atrium-net implementation discipline

Per `LANGUAGE-POLICY.md` smallest-TCB carve-out:

- Rust (`portcullis/atrium-net/`).
- `#![deny(unsafe_code)]` at root; localised `mod ffi` for the
  syscall wrappers (`getifaddrs`, `ioctl SIOCSIFADDR`, etc.) or
  shell-out to `ifconfig(8)` / `wpa_supplicant(8)` /
  `dhclient(8)` (a layered v0 implementation).
- No async runtime. Single-threaded blocking accept loop.
- Aqueduct service at `/var/run/aqueduct/atrium-net.sock`.
- Run jailed (yes, the network daemon runs jailed too — its
  jail has the `[network]` capability `host_alias` to manage
  any interface, but no filesystem write outside its own
  config dir, no other jail's data, etc.).

Persistent state at `/var/run/atrium/atrium-net.state.toml`:
WiFi credentials (encrypted), VPN configs, lo0 allocations,
inter-jail policy. Atomic-replace.

## 12. Open questions / future work

1. **IPv6.** Spec is IPv4-centric; IPv6 should be added as a
   parallel pathway (`addr6`, `gateway6`, etc.). Not blocking
   V0; punt to V1.
2. **DNS-over-TLS / DoH.** Once `atrium-resolved` exists,
   default-on DoT to a configurable upstream. Operator policy.
3. **Per-jail traffic accounting.** ipfw/pf counters per lo0
   alias; could feed a future `atrium-monitor` daemon. Out of
   scope here.
4. **VPN-as-default-route.** A user enabling a VPN GUI app
   should make its tunnel the default route for everything. UX
   design open.
5. **Captive portal handling.** When DHCP returns a captive
   portal, how does the user sign in? atrium-net opens a small
   browser jail with the portal's origin? Future.

## 13. Implementation order

When work resumes:

1. **jaild `[network]` extension** (½ day): new
   `NetworkConfig` enum in `CreateJailRequest`; validator
   against jaild policy; ffi to apply via `ifconfig` shell-out
   (V0) or `ioctl` (V1); jaild policy schema for
   `[network.*]`.
2. **Manifest schema in service manifests** (½ day): teach
   `system_services::ServiceManifest` about `[network]`;
   teach `to_create_request` to fill the new field.
3. **lo0 alias allocation in V0** (manual; operator edits
   `/var/db/atrium/lo0-allocations.toml`; portcullisd or jaild
   reads).
4. **atrium-net daemon V0** (3 days): implements the read-side
   plus AllocateLo0Alias / ReleaseLo0Alias for portcullisd.
   Write-side (JoinWifi, SetStaticIp, …) is V1.
5. **pf.conf generation from inter-jail policy** (V1 atrium-net).

## 14. References

- `docs/spec/portcullis.md` — manifest schema (where `[network]`
  goes)
- `docs/spec/jaild-policy.md` — where `[network]` policy gating
  goes
- `docs/spec/storage.md` — sibling architecture (mounts +
  storage)
- `docs/spec/service-management.md` §6 — atrium-net's place in
  the GUI-mediator daemon table
- `docs/LANGUAGE-POLICY.md` — Rust + smallest-TCB carve-out
