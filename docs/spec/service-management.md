# Service-management architecture — decomposition principle + gap analysis

**Status:** spec, 2026-05-07
**Owner:** D2.5 Portcullis + cross-cutting

This document records the architectural principle that Atrium's
service-management is **explicitly decomposed across several small
daemons** rather than concentrated in one PID-1-equivalent, the
gap analysis vs systemd that motivated locking it in, and a
binding rule for where future "we should add X" features land.

## 1. Principle

> **No single Atrium daemon shall become the systemd-equivalent.**
> Service management is the responsibility of `init` (rc), `jaild`,
> `portcullisd`, `aqueduct`, plus dedicated single-purpose
> daemons (`atrium-log`, `atrium-timer`, `atrium-authd` …) when a
> new domain warrants one. Each daemon's scope is justified in its
> top-level README, audited against this document, and resisted
> from creeping.

This is the architectural choice systemd's critics wanted Linux
to keep. We get to keep it because we started fresh.

## 2. The systemd backdrop

systemd legitimately solved real problems over sysv-init: socket
activation, parallel boot, cgroup-based supervision, declarative
sandboxing, structured logging, declarative resource control,
unit timers, templating, programmatic API. The technical wins are
real and worth taking seriously.

Where systemd drew justified criticism was scope: absorbing udev,
logind, networkd, resolved, timesyncd, hostnamed, machinectl,
homed turned a service manager into a Linux userspace base. The
architectural objection — "PID 1 is doing too much" — is the one
this document binds Atrium against repeating.

The systemd technical wins are addressed in §4 below; each is
mapped either to an existing Atrium component, to a planned
extension of one, or to a new dedicated daemon. None goes into
"portcullisd, the all-purpose service mega-daemon."

## 3. Current decomposition

```
init (FreeBSD)
  └── rc(8)
       ├── ssh, syslog, networking, …  (FreeBSD base; not ours)
       └── atrium-jaild                 (rc starts ONE Atrium thing)
            │
            └── jaild creates child jails on portcullisd's request:
                 ├── portcullisd          (policy + lifecycle)
                 ├── atrium-authd         (auth helper, deferred v1.5)
                 ├── frescod              (scenegraph compositor)
                 ├── atrium-devevents     (input reader)
                 ├── atrium-log           (structured logging, future)
                 ├── atrium-timer         (manifest-driven timers, future)
                 ├── vestibulum@<seat>    (login screen, per seat)
                 └── user-N-supervisor    (per-session)
                      └── apps            (per-user, per-launch)
```

rc starts exactly **one** Atrium thing: `atrium-jaild`. Everything
else is a child or grandchild jail under jaild, created by
portcullisd at boot or on demand.

Domain ownership is fixed:

| Domain                  | Daemon          | Rationale                                                          |
|-------------------------|-----------------|--------------------------------------------------------------------|
| Privileged jail creation| `jaild`         | Sole `jail_set(2)` caller; smallest TCB; OpenSSH-style privsep    |
| Policy + lifecycle      | `portcullisd`   | atrium.toml interpretation, sessions, procdesc EVFILT_PROCDESC    |
| IPC fabric              | `aqueduct`      | One service-discovery + framing layer                              |
| GPU + scenegraph        | `frescod`       | One compositor, owns `/dev/atrium-gpu0`                            |
| Input                   | `atrium-devevents` | One reader of `/dev/input/event*`; broadcasts via aqueduct      |
| Auth                    | `atrium-authd`  | OpenSSH-style auth helper (deferred to v1.5 per `login-handoff.md`)|
| Logging                 | `atrium-log`    | Single structured-log collector (future; see §4.1)                 |
| Timers                  | `atrium-timer`  | Manifest-driven scheduling (future; see §4.4)                      |

## 4. Gap analysis vs systemd, with placement decisions

For each capability systemd offers that's currently weaker or
missing in Atrium, this section binds where the fix lands. The
options are:

- **(P)** in portcullisd
- **(S)** in a separate dedicated daemon
- **(N)** no work — adequate as-is

### 4.1 Cross-service log queries — (S) `atrium-log`

systemd has `journalctl -u svc1 -u svc2 --since=…`. Atrium today
writes per-service logfiles under `/var/log/atrium/<svc>.log`.
Cross-service queries are awkward.

**Decision: separate daemon, `atrium-log`.**

- Fundamentally different domain from policy + lifecycle.
- jaild's exec path can connect a service's stdout/stderr to a
  pipe owned by atrium-log instead of a logfile (same shape as
  journald's `StandardOutput=journal`).
- atrium-log persists with metadata (jail name, jid, pid, time,
  priority) into a structured store (sqlite or append-only log
  file). Query CLI: `atrium-log query --jail vestibulum --since 1h`.
- portcullisd does NOT touch logging. If portcullisd is
  compromised, log integrity is not affected; if atrium-log is
  compromised, lifecycle is not affected.
- Defer until D3 unless an earlier need surfaces.

### 4.2 Parallel boot — (P) portcullisd batch-launch

rc starts services sequentially. systemd parallelises within
dependency constraints.

**Decision: portcullisd.**

- rc only starts atrium-jaild — the rc-side bottleneck is
  one process. Not a real issue at that layer.
- portcullisd then launches every Atrium system service at boot
  by reading `/etc/atrium/services.d/*.toml`. There's no reason
  to do these sequentially when they have no dependencies.
- portcullisd grows a topological launcher: parse manifests,
  compute dependency graph (`[depends_on]` field in atrium.toml
  manifests), launch independent batches in parallel via
  concurrent `CreateJail` requests to jaild.
- This is intrinsic to portcullisd's orchestration role; doesn't
  break the principle.

### 4.3 Live introspection — (P) portcullisd + (CLI) portcullis-cli

systemd has `systemctl status`. Atrium needs the same shape.

**Decision: portcullisd holds the data; `portcullis-cli` queries it.**

- portcullisd already owns: sessions, jail list, procdesc states,
  per-jail metadata. The query API is just an aqueduct service
  on portcullisd's admin socket.
- `portcullis-cli status [--jail name | --session id | --user N]`
  reads the API and prints. No new daemon.
- This is the reverse of scope creep — it's exposing what
  portcullisd already knows. Allowed.

### 4.4 Manifest-driven timers — (S) `atrium-timer`

systemd has timer units (`OnCalendar=`, `OnUnitActiveSec=`,
persistent timers). Atrium today has cron(8) for system-level
recurring tasks but no manifest equivalent.

**Decision: separate daemon, `atrium-timer`.**

- cron is sufficient for system-level recurring tasks; don't
  replace it. atrium-timer is purely for *manifest-declared*
  per-app timers ("run my-backup-app every Monday at 03:00").
- atrium-timer is a tiny scheduler:
   1. Reads `[timer]` blocks from per-app atrium.toml manifests
      (which it gets from portcullisd's API or a shared dir).
   2. Sleeps until the next deadline.
   3. Asks portcullisd to launch the relevant app via the same
      aqueduct call any other client uses.
- portcullisd doesn't need to know about scheduling. atrium-timer
  doesn't need to know about jails or capabilities. Clean
  separation.
- Defer indefinitely — cron handles real needs today. Land when
  the first manifest-driven timer use case shows up.

### 4.5 Templating (`vestibulum@seat0`, `app@instance-N`) — (P) portcullisd

systemd has unit templates. Atrium needs to express "one manifest,
N instances".

**Decision: portcullisd.**

- This is intrinsic to manifest interpretation, which is
  portcullisd's domain.
- Manifest gets a `[template] instance_param = "seat"` declaration.
  Launch requests carry an instance value. portcullisd builds the
  `CreateJail` spec by substituting (jail name `vestibulum-seat0`,
  argv includes `--seat seat0`, etc.).
- jaild policy stays unaware; it just sees concrete jail names
  matching its allowlist patterns.
- No new daemon.

### 4.6 Self-restart policies (`Restart=on-failure`) — (P) portcullisd

systemd has fine-grained restart policies. Atrium needs
equivalent for "if my supervisor crashes, restart it; if my
log daemon crashes, restart it 3 times then back off."

**Decision: portcullisd.**

- portcullisd already holds the procdesc fd and processes
  `EVFILT_PROCDESC NOTE_EXIT`. The "what to do on exit" decision
  is small additional logic.
- Per-manifest field: `[supervision] restart = "on_failure" |
  "always" | "never"`, `restart_after = "5s"`,
  `max_restarts_per_minute = 5`. portcullisd applies it on the
  EVFILT_PROCDESC handler.
- No new daemon. Doesn't break the principle — portcullisd
  *already* tracks lifecycle; this is the obvious extension.

### 4.7 Resource control (CPUWeight, MemoryMax) — (S) dedicated daemons + rctl

systemd has cgroup-backed resource controls in unit files. Atrium
has rctl(8) per jail / login class / user.

**Resolved (2026-06-24, when the memory path was implemented).** The
*static* per-jail caps still come from the manifest
(`[resources] cpu_share = 50, memory_max = "2GiB"`), translated to
rctl(8). But **memory pressure** turned out to be a reactive control
loop — a PSI signal, a reclaim cascade, a weighted water-fill — not a
set of static rules. By the binding rule (§1, "a dedicated daemon when
a new domain warrants one"), it gets **dedicated single-purpose
daemons, not portcullisd**: `atrium-memfed` (proactive — water-fills
RAM across jails by weight, sets each jail's `memoryuse` cap, boosts a
thrashing jail by per-jail PSI `full`) and `atrium-memoryd` (reactive —
PSI-`full`-gated cascade that sheds the lowest lifecycle tier, sparing
the foreground). The kernel side is rank-0 (PSI in `kern_pressure.c`,
RCTL, the `atrium-zram` compressed-swap kmod, the `/dev/pressure`
kqueue edge). Spec: `atrium-memory-pressure.md`.

- **Placement.** Siblings of `atrium-log`/`atrium-timer` in the jaild
  tree; started by rc.d (`atrium-zram` → `atrium-memoryd` /
  `atrium-memfed`), gated like frescod.
- **Privilege (the v1/v2 split that was deferred here).** v1 bring-up
  = host-side privileged daemons (memoryd must signal across jail PID
  namespaces; memfed sets rctl on jails) — the same shape frescod's rc
  script has today. v2 = the OpenSSH/qmail privsep this document
  favours: **jailed policy** (`_memoryd`/`_memfed`, rank 3, reading the
  global pressure telemetry as a granted capability — cross-jail stall
  is TCB-sensitive, same trust posture as frescod seeing all pixels) +
  **jaild-brokered mechanism** (jaild grows `set_rctl(jail, …)` and
  `reap(jail, app, sig)`, policy-gated — so it stays the *only*
  privileged broker, the "tighter privilege story" pro). For clean
  jailing, the per-jail pressure detail should move onto `/dev/pressure`
  (read/ioctl) so the governor needs only that one device in its devfs
  ruleset, no host sysctl. See `atrium-memory-pressure.md` §9.
- **Source of weights/registry.** The manifest `[resources]` (weight =
  lifecycle tier = lmkd tier; floor = `memory_min`) and portcullisd's
  session table (which app, which jail, which tier) — not a hand-edited
  file. portcullisd feeds the governors; it does not *be* them.

### 4.8 Programmatic API / D-Bus equivalent — (N) aqueduct as-is

systemd has D-Bus. Atrium has aqueduct.

**Decision: no work.**

- aqueduct is already the IPC fabric. Service-discovery via
  socket paths under `/var/run/aqueduct/`. No bus daemon needed.
- Clients query portcullisd, atrium-log, atrium-timer, etc. via
  their respective aqueduct sockets directly.

### 4.9 Socket activation — (N) better than systemd, already

systemd's socket-activation hack: PID 1 holds the socket, daemon
starts on first connect. Useful when daemons are slow to boot.

**Decision: no work; we already do better.**

- portcullisd creates the aqueduct socket directory at boot and
  binds the per-service sockets *before* the service starts.
- Services receive their listen-fd inherited via SCM_RIGHTS from
  jaild's exec path.
- Socket exists when any client looks for it; the service may not
  yet be ready (it might be loading state) but that's a runtime
  retry concern, not a boot-order concern.
- This is structurally cleaner than systemd's late-binding model.

### 4.10 Sandboxing primitives (PrivateTmp=, ProtectSystem=) — (N) jaild covers it

systemd has declarative sandbox knobs. Atrium has jails per
service + jaild policy + atrium.toml capabilities.

**Decision: no work.**

- Jails are stronger than namespaces. jaild already enforces
  mount allow-lists, devfs ruleset gating, exec path allow-lists.
- atrium.toml capabilities express the equivalent declarative
  intent ("this app gets `home`, `network=loopback`, no `gpu`").
- The systemd directives that don't already have an Atrium
  equivalent (`SystemCallFilter=`, `RestrictAddressFamilies=`)
  are Linux-specific or covered by jail isolation more strongly.

### 4.11 GUI configuration of system state (Network Manager / settings panels) — (S) per-domain mediator daemons

systemd absorbed networkd, resolved, timesyncd, hostnamed,
homed, etc. so GUI configuration tools could reach them via
D-Bus. Atrium apps run in jails and cannot shell out to
`ifconfig`, `pw`, `ntpdate`, etc. directly — they need a
privileged mediator.

**Decision: per-domain mediator daemons (NOT portcullisd).**

- These are different domains (Q2 in §5 checklist, STOP). A
  compromise of network-config code shouldn't compromise
  jail-creation policy; an account-management bug shouldn't
  reach into the manifest interpreter.
- Each mediator gets its own jail, root within that jail, narrow
  surface, own audit boundary. See §6 for the full table:
  `atrium-net`, `atrium-time`, `atrium-accounts`, `atrium-hwctrl`.
- portcullisd's role: **capability-routing only**. atrium.toml
  gains `[capabilities] service.atrium-net = "configure"` etc.;
  portcullisd mounts the relevant aqueduct sockets into the
  requesting app's jail at launch. portcullisd never executes
  the operations.

This is the OpenBSD-privsep pattern at platform scale: many
small privileged components, each owning one domain, each with
its own audit boundary. systemd's mistake was *not* "having one
privileged daemon" but rather "having one privileged daemon that
also did networking and DNS and time and login and …". We avoid
this by keeping portcullisd a *router*, not an *implementer*.

## 5. Decision rule for future features

Whenever someone proposes a new feature with a question of
"should this go in portcullisd?" run it through the following
checklist. **Answers must be written down** in the feature's
proposal commit; this doc gets updated when a recurring pattern
emerges.

```
Q1. Does this feature operate on portcullisd's existing data
    (manifests, sessions, procdesc-tracked processes,
    capabilities)?
        Yes → portcullisd is a candidate.
        No  → strong signal it's a separate daemon.

Q2. Does this feature involve a fundamentally different domain
    (logging, scheduling, hardware, networking)?
        Yes → separate daemon. STOP.
        No  → continue.

Q3. If we put it in portcullisd, how much does it grow
    portcullisd's surface (LoC, new privileges, new external
    dependencies)?
        Small + same privilege class → portcullisd is fine.
        Large or new privilege       → separate daemon.

Q4. Is there a clean aqueduct interface where another daemon
    could provide this feature, with portcullisd as a client?
        Yes → strong signal for a separate daemon.

Q5. Would a portcullisd compromise then compromise this feature's
    integrity, in a way that wouldn't happen if it were separate?
        Yes → separate daemon (defense in depth).
```

The default answer for any borderline case is **separate daemon**.
We err on the side of more daemons, smaller scopes. The cost of
adding a daemon is bounded; the cost of un-doing portcullisd
scope creep later is unbounded.

## 6. GUI-mediator daemons (the NetworkManager pattern)

**Important distinction missed in earlier drafts:** "use base
FreeBSD" works for *system-level* operations (dhclient, ntpd, pw
on an admin CLI) but NOT for *GUI configuration tools*. An
Atrium Network Manager GUI runs in its own jail with `ifconfig`
not in its mount allow-list, `PF_ROUTE` not in its capability
set, and raw sockets denied. To configure the network, it has to
talk to *something* privileged.

That something is **not portcullisd**. portcullisd is policy +
lifecycle; it doesn't know `ifconfig` semantics. The right shape
is a **per-domain mediator daemon** that wraps the privileged
operations behind an aqueduct service interface, with portcullisd
mediating capability grants ("this GUI is allowed to talk to
atrium-net") but never touching the operations themselves.

This is the OpenBSD-privsep pattern at platform scale: many
small privileged daemons, each owning one domain, each with its
own audit boundary. portcullisd is a *capability router*, not a
*capability implementer*.

```
GUI Network Manager (in jail "app-network-manager-1")
  │ atrium.toml: [capabilities] service.atrium-net = "configure"
  │ ↓ aqueduct
atrium-net (its own jail, root-privileged, narrow scope)
  │ ↓ ifconfig / route / wpa_supplicant / resolvconf
kernel
```

portcullisd's contribution to this flow:

- At app install: prompt "this app wants to configure network —
  allow?" (already in the Portcullis spec).
- At app launch: mount `/var/run/aqueduct/atrium-net.sock` rw
  into the app's jail (existing capability-translation work).
- At runtime: nothing. portcullisd doesn't see the operations.

### The mediator-daemon table

| Domain                                   | Daemon            | What it wraps                                       | Why a separate daemon                                                    |
|------------------------------------------|-------------------|-----------------------------------------------------|--------------------------------------------------------------------------|
| Networking (interfaces, routing, WiFi, VPN) | `atrium-net`     | `ifconfig`, `route`, `wpa_supplicant`               | Largest domain; would dwarf portcullisd. Compromise of net code shouldn't compromise jail policy. |
| DNS resolver config                      | `atrium-net`*     | `/etc/resolv.conf`, `resolvconf(8)`                 | Folded into atrium-net (same domain).                                    |
| Time + timezone                          | `atrium-time`     | `ntpd`/`chronyd` config, `/etc/localtime`, `tzsetup`| Tiny, but distinct threat model from networking. |
| User / group accounts                    | `atrium-accounts` | `pw(8)`, `/etc/master.passwd` (write side)          | Auth-adjacent; can create root accounts if compromised; needs its own audit boundary. |
| Display / audio / power                  | `atrium-hwctrl`   | backlight, mixer, `acpiconf`                        | Hardware-control domain; different surface from policy.                  |
| Service log aggregation                  | `atrium-log`      | journald-equivalent                                 | Different domain; integrity-isolation from policy.                       |
| Manifest-driven timers                   | `atrium-timer`    | systemd timer-unit equivalent                       | Different domain; portcullisd is reactive, not active.                   |
| Per-jail volume allocation (DB data, app rootfs, tmpfs) | `atrium-volumes` | per-volume backend (Tessera default, ZFS / plain alternatives); see `docs/spec/storage.md` | Different domain (storage lifecycle); integrity-isolation from policy; pluggable backends; quota / snapshot / dedup features per-backend. |
| Per-jail pf rule lifecycle (network policy enforcement)  | `atrium-netd`     | `/dev/pf` anchors derived from manifest `[capabilities] network.*`; see `docs/spec/atrium-netd.md` | Different domain (kernel firewall lifecycle); narrow `pf` capability via devfs ruleset; event-driven reconciler vs portcullisd-daemon's request-driven RPC; independent failure isolation. |

\* DNS is a sub-domain of networking; doesn't justify a separate
daemon today. Could split out as `atrium-resolved` if a
DNS-only GUI ever needs more than `atrium-net` exposes.

### When system services use base FreeBSD directly

Several base FreeBSD daemons run *as the system itself*, not via
mediators, and are configured via files written by the
mediators. The mediator daemons sit *between* GUI clients and
these base daemons:

| Base FreeBSD daemon | Configured by    | Read by                                |
|---------------------|------------------|----------------------------------------|
| `dhclient`          | `atrium-net`     | (FreeBSD itself)                       |
| `ntpd` / `chronyd`  | `atrium-time`    | (FreeBSD itself)                       |
| `unbound`           | `atrium-net`     | (FreeBSD itself; system DNS resolver)  |
| `cron`              | (none)           | Atrium uses cron directly for system schedules; per-app timers go via `atrium-timer` |
| `syslogd`           | (none)           | (FreeBSD itself; only base services log via syslog) |

This keeps the base FreeBSD service set unchanged. We're adding
a userspace platform on top, not replacing FreeBSD's daemons.

## 7. Other watch-list features

Not GUI-mediator daemons; pre-bound for completeness:

| Feature                       | Goes in           | Why not portcullisd                                  |
|-------------------------------|-------------------|------------------------------------------------------|
| Hardware hotplug              | `atrium-devevents` (extend) | Already a separate daemon; add domains there |
| Boot splash / progress        | (none)            | Out of scope                                         |
| Crash dump processing         | `atrium-log`      | Logs are logs                                        |
| Per-user persistent state     | (filesystem)      | Lives under `/var/db/atrium/users/<N>/`; no daemon needed |
| Inter-app clipboard           | `atrium-clipboard` (future, separate) | Different domain (data-flow), needs careful permission story |
| Notifications                 | `atrium-notifyd` (future, separate)   | Different domain; user-visible IPC channel |

## 8. References

- `docs/spec/portcullis.md` — Portcullis full spec (manifests,
  jail builder, lifecycle).
- `docs/spec/login-handoff.md` — boot-to-session protocol.
- `docs/spec/jaild-policy.md` — jaild allow-list schema.
- `docs/spec/storage.md` — per-jail volume allocation
  (atrium-volumes), backend model, static + dynamic mount
  lifetime.
- `docs/spec/network.md` — jail-side networking model.
- `docs/spec/atrium-volumes.md` — the volume-allocation broker
  daemon design.
- `docs/spec/atrium-pkg.md` — Atrium package format and install
  path.
- `docs/LANGUAGE-POLICY.md` — Rust by default, smallest-TCB
  carve-out.
- `scratch/jail-smoke/` — privsep model validation.
