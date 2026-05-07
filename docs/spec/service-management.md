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

### 4.7 Resource control (CPUWeight, MemoryMax) — (P) portcullisd, (P/S) rctl path

systemd has cgroup-backed resource controls in unit files. Atrium
has rctl(8) per jail / login class / user.

**Decision: portcullisd applies rctl rules; jaild path is open
question for v2.**

- Manifest field: `[resources] cpu_share = 50, memory_max = "2GiB"`
  etc. portcullisd translates these to rctl(8) rules and applies
  them at jail-create time.
- v1: portcullisd shells out to `rctl(8)` directly (root, simple).
- v2 question: should jaild grow an rctl FFI so it's the only
  privileged broker? Open. Slight pro: tighter privilege story.
  Slight con: jaild grows in scope; rctl rules are
  capability-related but not "jail creation."
- Defer the v1/v2 split to when we actually wire resources;
  mark this as **TODO: revisit when implementing rctl**.

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

## 6. Watch list

These are the features where the temptation to drop "just one
more thing" into portcullisd will be strongest. Pre-bound here
so the decision is already made:

| Feature                       | Goes in     | Why not portcullisd                                   |
|-------------------------------|-------------|-------------------------------------------------------|
| Service log aggregation       | atrium-log  | Different domain; integrity-isolation from policy     |
| Manifest-driven timers        | atrium-timer| Different domain; portcullisd is reactive, not active |
| Network configuration         | atrium-net? | Different domain; not a portcullisd concern at all    |
| DNS resolution                | (use base)  | unbound is fine; not a portcullisd concern            |
| Time sync                     | (use base)  | ntpd / chronyd; not a portcullisd concern             |
| User account management       | (use base)  | pw(8); per-user state is portcullis-policy crate, no daemon scope creep |
| Hardware hotplug              | atrium-devevents (extended) | Already a separate daemon; add domains there |
| Boot splash / progress        | (none)      | Out of scope                                          |
| Crash dump processing         | atrium-log  | Logs are logs                                         |

## 7. References

- `docs/spec/portcullis.md` — Portcullis full spec (manifests,
  jail builder, lifecycle).
- `docs/spec/login-handoff.md` — boot-to-session protocol.
- `docs/spec/jaild-policy.md` — jaild allow-list schema.
- `docs/LANGUAGE-POLICY.md` — Rust by default, smallest-TCB
  carve-out.
- `scratch/jail-smoke/` — privsep model validation.
