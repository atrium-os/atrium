# Fast boot: parallel startup with readiness

**Status:** accepted 2026-11-18 (all §10 recommendations). B0 measured 2026-11-18 (§2.0).
**Owner:** portcullisd-bootstrap (launcher), jaild (descriptor passing), rc.d
scripts of each host daemon.
**Related:** `service-management.md` (§4.2 parallel boot, §4.9 socket
activation), `login-handoff.md`, `atrium-net.md`, `lyra-v1.md`.

## 1. Goal

Boot fast, and never let a program crash, spin or give up because a service it
needs has not started yet.

What we measure (all from the kernel's `kern.boottime`, so firmware and loader
are counted separately):

| Name | Ends when |
|---|---|
| **T-login** | the login screen is drawn (ostiarius has a window on frescod) |
| **T-desktop** | after a login, the bar and dock are drawn |
| **T-settled** | every enabled service is ready (audio, network, mail, Arca …) |
| **pre-kernel** | firmware + loader, from power-on to the kernel's first line |

Targets, to be set from the first measurement: T-login no longer waits on the
network, on audio, or on any fixed `sleep`. T-settled stops having multi-second
floors that come from polling.

The rule this spec adds: **startup order is a property of the launcher, not of
luck.** A client that connects to a service at boot either succeeds at once or
is told by the supervisor why the service is not there. No fixed sleeps on the
boot path, in rc scripts or in daemons.

## 2. Where boot time goes today

### 2.0 Measured (B0, 2026-11-18, Mac dev VM, `boot-trace.sh`, 3 reboots)

What was measured: the dev VM as it is, the stock 16.0-CURRENT **GENERIC**
kernel (WITNESS on) on UFS, 4 vCPUs under HVF. rc enables jaild, volumes,
portcullisd-daemon and dispensatord; **atrium-portcullisd-bootstrap is not
enabled**, so no jailed service (frescod, ostiarius, lyrad, Arca, Sigillum …)
starts at boot. So this measures the FreeBSD + host-daemon part only; T-login
cannot be measured on this VM until B1 starts the jailed tree.

Reboot request → login prompt on the serial console, run 1 (runs 2, 3 within
±0.5 s; rc stage within 40 ms):

| Stage | Time | Cause |
|---|---|---|
| shutdown | 5.5 s | each Atrium rc stop function waits in `sleep 1` steps, one after another (portcullisd-daemon, dispensatord, volumes, jaild: 1 s each), then the syncer 1 s |
| UEFI firmware | 5.1 s | ≈ 5 s lost on "Image type X64 can't be loaded on AARCH64": the firmware's boot order tries a wrong entry first |
| loader | 9.7 s | `autoboot_delay` unset → FreeBSD's 10 s countdown |
| kernel to root mount | 0.13 s | |
| rc | 6.5 s | **netif 4.3 s**, devd 0.7 s, every other script 7–30 ms |
| **total** | **≈ 27 s** | |

netif's 4.3 s: ≈ 2.3 s printing 52 WITNESS backtraces to the serial console
(`uma_zalloc_debug` with the vtnet core lock held, `if_vtnet.c:1572`, a
GENERIC-debug-only cost but a real vtnet locking bug), then ≈ 2 s silent, which
matches netif's IPv6 DAD sleep (`ifconfig_DEFAULT="DHCP inet6 accept_rtadv"`).
DHCP itself took 24 ms (DHCPREQUEST → bound) and defaultroute 15 ms.

The crash case seen in the logs: under a manual bootstrap run, **ostiarius
exited every 15.05 s (status 256) and was restarted forever**: its 15 s budget
in `wait_for_frescod` ran out because frescod was not running, so the
supervisor's restart policy turned a missing dependency into a crash loop.
Every bootstrap shutdown also logs "Device busy" unmounting each service's
`usr/local/bin` nullfs: a mount left behind, to fix with B5.

Rough budget after the fixes below: shutdown < 1 s, firmware ≈ 0.1 s, loader
1 s, rc < 1 s on a non-debug kernel: **≈ 3 s instead of 27 s** for this VM,
before the jailed tree is added.

### 2.0a From reading the code

Boot today has three stages: rc(8) runs every rc.d script one after another in
the foreground; some of those start Atrium host daemons; one of them,
`atrium-portcullisd-bootstrap`, then starts every jailed service in
`/etc/atrium/services.d/` one after another in file-name order.

### 2.1 Things that block rc (everything after them waits)

| Where | What | Cost |
|---|---|---|
| loader | `autoboot_delay` (FreeBSD default 10 s; r9700 `5`, devroot `2`; RUNBOOK says keep `1`) | 1–10 s |
| `/etc/rc.d/netif` | sleeps `net.inet6.ip6.dad_count + 1` s (≈ 2 s) whenever an interface has IPv6, waiting for DAD | ≈ 2 s |
| `/etc/rc.d/defaultroute` | with any `ifconfig_*="DHCP"` (the dev VM image has `ifconfig_vtnet0="DHCP"`, `build-zfs-root.sh`), waits up to `defaultroute_delay` = 30 s for dhclient's default route; no cable stops it after the 5 s carrier delay | 1–3 s with a lease, 5 s unplugged, 30 s for a cable with no DHCP server |
| `/etc/rc.d/local_unbound` | `poststart` polls `unbound-control status` once a second, up to 5 s | ≥ 1 s |
| `/etc/rc.d/ntpd` on the r9700 box | `ntpd_sync_on_start=YES` runs a blocking `ntpd -q -g` before ntpd starts | up to the NTP timeout; long with no network |
| `atrium-netconf` start | waits for atrium-net's socket, `sleep 1` up to 20 times (`atrium-net/etc/atrium-netconf:50`) | ≥ 1 s; **20 s when atrium-net has no port yet** (it waits for one before making its socket) |
| `atrium-portcullisd-bootstrap` start | waits for atrium-volumes' socket, `sleep 0.5` up to 20 times | ≥ 0.5 s, up to 10 s |

### 2.2 Ordering that puts the desktop behind the network

`atrium-portcullisd-bootstrap` REQUIREs `portcullisd`, whose script REQUIREs
`NETWORKING DAEMON` (`portcullis/portcullisd/install/portcullisd`). rcorder
orders by REQUIRE whether or not a script is enabled, so **every jailed service
(frescod, ostiarius, lyrad, Arca, Sigillum, …) starts only after netif,
defaultroute, local_unbound and the rest of NETWORKING** — the 2.1 waits are
all on the path to the login screen. Nothing the jailed services do needs the
network up at start; atrium-net already follows ports that come and go.

### 2.3 The launcher is serial and has no readiness

`portcullisd/src/bin/bootstrap.rs` sends one `CreateJail` per manifest, in
lexicographic order, and moves on as soon as jaild answers. The `NN-` prefixes
are the only dependency language. "Launched" is not "ready": a service binds
its socket some time after its process starts.

### 2.4 How clients cope today (the crash risk)

Each daemon invented its own answer, and some have none:

| Client → service | Today | Effect at boot |
|---|---|---|
| choragusd → lyrad | one `connect`, exit on failure (`choragus/src/daemon.rs:1367`) | launched right after lyrad (35 → 36); if lyrad has not bound yet choragusd exits, the supervisor restarts it 1 s later; 5 such exits a minute and it is parked for 30 s with **no audio** |
| ostiarius → frescod | polls `connect` every 500 ms, 15 s budget, then gives up | up to 0.5 s added to T-login; a slow GPU init past 15 s means no login screen |
| forum-bar → network, audio | polls every 3–5 s | icons appear up to 5 s late |
| sigillum-ui, tactusd | retry every 2 s | up to 2 s late |
| atrium-net's rc → its port | `sleep 3` until a port exists | (in its own background loop, fine) |

`service-management.md` §4.9 says portcullisd binds every service's socket
before the service starts, so this cannot happen. **That was never built**:
every service binds its own socket (`lyra-service.sh` even makes lyrad's
directory writable so lyrad can bind there).

### 2.5 Shutdown

Each rc stop function waits for its daemon in `sleep 1` steps (up to 5 s
each), one script after another. Shutdown cost adds up the same way.

## 3. Design

Three changes, in order of payoff:

1. **Take the desktop off rc's serial path** (§4): start the launcher right
   after `FILESYSTEMS` and jaild, and make every remaining rc wait asynchronous.
2. **The supervisor binds every service's sockets before starting anything,
   then starts everything at once** (§5). A client's `connect` always
   succeeds; its first request waits in the listen backlog until the service
   accepts. Order stops mattering for anything that talks over a socket — which
   is nearly everything.
3. **Explicit readiness and dependencies for what sockets don't cover** (§6):
   a readiness descriptor and `after =` edges in the manifest.

And one cleanup that makes the rest hold: **one client-side connect helper**
(§7) for the services that are not supervisor-bound yet (host rc daemons), so
no daemon polls with a fixed sleep.

This keeps `service-management.md`'s principle: no PID-1 replacement. rc stays
rc. Parallel launch lives in portcullisd-bootstrap as §4.2 already decided;
descriptors ride jaild's exec path as §4.9 intended.

## 4. rc stage

### 4.1 Start the Atrium tree early

- `atrium-portcullisd-bootstrap`: `REQUIRE: FILESYSTEMS atrium_jaild
  atrium_volumes` — drop `portcullisd` (the old multi-tenant daemon script,
  whose REQUIRE pulls in NETWORKING DAEMON). Add `BEFORE: netif` so the jailed
  tree, frescod and the login screen start while rc does the network.
- That also puts the tree ahead of `/etc/rc.d/devfs` (it REQUIREs
  mountcritremote, after NETWORKING), which is what loads the devfs rulesets.
  jaild refuses a jail whose ruleset has no rules, so the `atrium_jaild` rc
  script loads every file in `devfs_rulesets` before jaild starts, and Atrium
  hosts set `devfs_load_rulesets="NO"`: rc.d/devfs reloads by deleting and
  re-adding each ruleset, and a jail mounted in that gap would see the host's
  whole /dev. `service atrium_jaild devfsrules` reloads after the rules file
  changes. (Found by the first B1 gate run: every service was refused with
  `devfs_ruleset.not_loaded`.)
- Host daemons that do not need the network move before it too:
  `atrium_dispensatord` (needs devd — fine, devd is early), `atrium_zram`,
  `atrium_memoryd`, `atrium_memfed` (today REQUIRE `sshd` for bring-up
  safety; keep that only on dev images, via an rc.conf knob).
- The duplicate launch paths go: frescod and ostiarius have both an rc.d script
  and a services.d manifest. The manifest wins; the rc.d scripts stay only as
  dev tools, disabled by default.

### 4.2 No waiting in rc

- **Network ports belong to atrium-net, not dhclient.** Images ship without
  `ifconfig_*="DHCP"` (atrium-net takes ports with no line or `up`), so
  `defaultroute` returns at once. IPv6: no `inet6 accept_rtadv` lines in
  rc.conf (atrium-net does SLAAC), so netif has no DAD sleep.
- **local_unbound**: its poststart polls once a second (≥ 1 s). After §4.1
  this no longer delays T-login, only T-settled; atrium-net already re-sets
  forwarders whenever unbound (re)starts, so the wait could go. Decided after
  measuring (§8).
- **ntpd**: no `ntpd_sync_on_start`; ntpd with `-g` slews/steps once the
  network is there, in the background.
- **atrium-netconf**: no wait loop. atrium-net gets its control socket bound
  by its rc script before atrium-net starts (the §5 trick, done in sh with
  a tiny `atrium-bind` helper that execs the daemon with the fd), so netconf
  can connect at once; until then netconf's connects queue.
- **bootstrap → atrium-volumes**: the same: atrium-volumes' rc binds the
  socket first. bootstrap's wait loop goes.
- **firmware (VMs)**: fix the UEFI boot order in each VM's `vars.fd` so the
  first entry is the FreeBSD disk, not the "X64 image" entry that costs 5 s.
- **stop functions**: poll the pid every 0.1 s instead of `sleep 1` first
  (each Atrium rc stop costs a full second even when the daemon exits at once),
  and stop host daemons that don't depend on each other together.
- **loader**: `autoboot_delay="1"` and `beastie_disable="YES"` on every image
  (RUNBOOK's guidance, applied everywhere, including the dev VM).

### 4.3 Kernel candidates (measure first)

`hw.usb.no_boot_wait`, `kern.cam.boot_delay`, root-mount hold time, kmods
loaded by the loader vs `kldload` later. Only acted on if §8 shows them.

## 5. Supervisor-bound sockets

### 5.1 Manifest

```toml
[[sockets]]
path  = "/atrium/sockets/lyrad/lyrad.sock"
owner = 65016          # _lyra
group = 65017          # _choragus
mode  = "0660"
```

A service may list several (choragusd has one per audio capability).

### 5.2 What the launcher does

At boot, before launching anything:

1. For every enabled manifest, for every `[[sockets]]`: unlink a stale file,
   `socket` + `bind` + `chown` + `chmod` + `listen(backlog 128)` in the host
   namespace. The path is already under a directory the jail null-mounts, so
   the jail sees the same socket.
2. Launch every service at once (each `CreateJail` on its own thread, or
   pipelined on the jaild connection). The listening fds ride the request
   (SCM_RIGHTS, as `ExecSpec.stdio` already does) and land as fd 3, 4, … with
   `ATRIUM_LISTEN_FDS=n` and `ATRIUM_LISTEN_NAMES=lyrad.sock:…` in the
   environment.
3. Keep its own copy of every listening fd. When a service exits and is
   restarted, the new process gets the same fds. Clients that connected in
   between are waiting in the backlog, not refused.

jaild's policy gains a rule: a listening fd may be passed only when its bound
path lies under one of that service's own `rw_nullfs` socket mounts, and only
to services, not apps. jaild checks with `getsockname`.

### 5.3 What a service does

One small function in `libatrium` (and the same few lines in C for the C SDK):
`listener(name)` returns the inherited fd if `ATRIUM_LISTEN_FDS` names it,
else binds the path itself (so the service still runs by hand, in tests and on
macOS). Services keep checking peers with `getpeereid` per connection; that does
not change.

### 5.4 Why this is the main mechanism

- Most "X needs Y" edges at boot are "X connects to Y's socket". With the socket
  already listening, the edge disappears: choragusd's first `connect` to lyrad
  succeeds even if lyrad is still opening the sound card; the first request
  waits until lyrad accepts.
- All services start together; the slowest one no longer delays the others.
- A crash and restart is invisible to clients except for requests in flight
  (they see the connection close, as today).
- It is what `service-management.md` §4.9 already promised.

What it does not cover: a service that must finish something before another
may *start* (mount a volume, configure zram), and services that are not
sockets. §6.

## 6. Readiness and dependencies

### 6.1 Readiness descriptor

```toml
[ready]
notify = true     # the service writes "READY\n" to $ATRIUM_READY_FD
timeout_secs = 30
```

The launcher passes the write end of a pipe. A service with `notify = false`
(the default) is ready when it has been started and all its sockets are bound
(always true under §5). A service that does not report ready within the timeout
is logged and counted as failed for its dependents; it is not killed for that.

### 6.2 Dependency edges

```toml
after = ["atrium-volumes", "atrium-arcad"]   # wait for these to be READY
```

The launcher builds the graph from every enabled manifest, refuses a cycle
(naming it: every service in the cycle counts as failed, the rest boot), and
starts each service as soon as everything it is `after` is ready. An `after`
name that is not an enabled manifest (a host rc daemon such as atrium-volumes)
is ignored with a warning. Services with no edges start at once. File-name order (`NN-`) stops
meaning anything; the numbers stay as a readable hint.

If a dependency fails or gives up, its dependents are **held, not started**,
and the boot status (`/var/run/atrium/boot-status`, one line per service,
rewritten on every change) says "held, waiting for atrium-arcad (failed: …)". Nothing
crash-loops on a missing dependency.

Expected use is small: most edges disappear under §5. Likely real edges:
memoryd/memfed after zram; anything with `[[volumes]]` after atrium-volumes
(the launcher resolves volumes itself today, so this is implicit).

### 6.3 Boot classes

```toml
class = "login"      # on the path to the login screen: frescod, ostiarius, vestibulum
class = "system"     # default
class = "idle"       # started once T-login is reached: opifex-sheetd, monstratord …
```

`login` services are launched first on their own threads; `idle` ones wait
until ostiarius reports the login screen drawn (its readiness), so they do not
compete for CPU and disk with the critical path. A client that connects to an
`idle` service before then just waits in the backlog (§5).

### 6.4 Host rc daemons

Host-side daemons that rc starts (atrium-net, netconf, volumes, dispensatord,
memoryd, memfed) keep rc for now. Their rc scripts bind their socket first with
`atrium-bind` (§4.2), then exec the daemon with the fd. So clients get the same
"connect always succeeds" behaviour without rc having to wait.

## 7. Clients

- **Supervisor-bound services**: nothing to do; connect once.
- **Everything else** (and as a safety net during migration): one helper,
  `libatrium::service::connect(path, deadline)`, that waits on the socket's
  directory with kqueue `EVFILT_VNODE` instead of sleeping, then connects.
  Replaces ostiarius' 500 ms loop, sigillum-ui's and tactusd's 2 s retries.
- **Long-lived subscribers** (forum-bar's network and volume icons): subscribe
  and get pushed changes, reconnecting through the helper when the service
  restarts, instead of polling every 3–5 s.
- A client that still cannot reach a service after its deadline shows that
  ("Sound is starting…", "Network service not running"), never exits.

## 8. Measuring

`scripts/boot-trace.sh` (host side, dev VM) reboots the VM N times and keeps:

- `kern.boottrace.log`: FreeBSD's boottrace (on with
  `kern.boottrace.enabled=1` in loader.conf; rc.subr then logs every rc.d
  script's start and end with timestamps, no other change);
- the birth time of every Atrium socket: when each service could first answer;
- every process's start time and jail; `/var/log/atrium/*.log`;
- the serial console with host timestamps (firmware + loader time);
- the guest's rc.conf, loader.conf and rcorder order.

The script adds the two loader tunables and removes them again at the end.
It runs on the shared Mac VM, so it needs a VM slot and Shankar's OK. The same
script works against a cloud VM (TCG), where only the order and the fixed waits
are meaningful, not the durations.

Gate for each milestone below: three boots, median T-login and T-settled, plus
"no service exited during boot" from the bootstrap log.

## 9. Milestones

| | What | Gate |
|---|---|---|
| **B0** | Measure today's boot (§8). Write the numbers into §2. | `boot-trace.sh` 3 runs on the dev VM |
| **B1** | rc stage: bootstrap enabled and BEFORE netif, no DHCP/accept_rtadv lines (atrium-net owns ports), no sync-ntpd, loader delay 1, VM boot order, fast stop functions, netconf and bootstrap wait loops gone (§4) | T-login no longer includes netif/defaultroute/local_unbound in boottrace |
| **B2** | Supervisor-bound sockets: manifest `[[sockets]]`, jaild fd passing + policy, `libatrium` `listener()`, lyrad/choragusd/frescod/ostiarius converted first | choragusd never exits at boot over 20 boots; restart of lyrad does not drop choragusd |
|  | *B2 status 2026-11-18: on main. VM gate run 3: lyrad's and choragusd's sockets bound ahead of launch, lyrad.sock keeps its inode across a restart and choragusd stays up, ostiarius waits for frescod. Boot to ssh 14–17 s (was 27 s); rc 6.5 s on GENERIC, 6.5 s on ATRIUM (WITNESS), 6.0 s on ATRIUM-NODEBUG; netif still ~4.3 s. Runs 2–3 found start-up gaps the old late start hid (lyrad's peer by name in a jail with no /etc/passwd; app- and courier-registry wiped from /var/run), now fixed. frescod's own sockets still to convert.* | |
| **B3** | Parallel launch + readiness fd + `after` + boot classes (§6) | all services launched within one jaild round-trip window; `portcullis status` shows held services and why |
|  | *B3 status 2026-11-18: built (portcullisd `boot_graph`: `after`, classes, cycles, held dependents, unit-tested; jaild `ExecSpec::ready` passes a pipe at the fd after the sockets as `ATRIUM_READY_FD`; `atrium_listen::ready()`; the supervisor launches from the graph, waits on readiness pipes with a timeout, writes boot-status and logs "boot settled"). frescod, ostiarius and tactusd are `login`; opifex-sheetd and monstratord `idle`; ostiarius reports ready once vestibulum is launched. Launches stay one jaild round trip each (jaild is single-threaded); the gain is that nothing waits for what it does not need. On main; VM gate run 4: 0 failed, all five services launched within 9 ms (ostiarius first), none exited, boot-status correct; ostiarius times out at 60 s on the dev VM because it has no frescod.* | |
| **B4** | Clients: `service::connect`, forum-bar push subscriptions, remove fixed sleeps (§7) | grep finds no `sleep` in a connect path; bar icons right at first frame |
|  | *B4 status 2026-11-18: on main; VM gate run 5 passed (the bootstrap's shutdown took 97 ms for 5 services, including an ssh round trip). `connect` lives in `atrium-listen` (`connect(path, deadline)` waits on the socket's directory with kqueue EVFILT_VNODE, `wait_for`, `Backoff`: 100 ms doubling to a cap, reset after a 10 s session), not libatrium. ostiarius, sigillum-ui and tactusd's Bluetooth HID use it; forum-bar's media, audio-control, sticks and network loops reconnect with `Backoff` instead of fixed 2–5 s sleeps. Push subscriptions in forum-bar are still to do: the follow loops already stream, so the gain left is the first frame.* | |
| **B5** | Host rc daemons bind first via `atrium-bind` (§6.4); parallel shutdown in bootstrap | T-settled and shutdown time measured; shutdown has no serial 5 s waits |
|  | *B5 status 2026-11-18: on main; VM gate run 5 passed (atrium-volumes, started through atrium-bind, holds its socket at fd 3 with ATRIUM_LISTEN_FDS=1). netif is still 4.3 s of the 6.6 s rc on the dev VM: its rc.conf has `ifconfig_DEFAULT="DHCP inet6 accept_rtadv"`, so dhclient waits for a lease inside rc.d/netif. Gate run 6 (NETIF_TRIM=1, Shankar's OK: `ifconfig_DEFAULT="DHCP"` and `background_dhclient=YES` on the dev VM): rc 4.4 s (was 6.6 s), netif 2.2 s, ssh back 13.2 s; limend bound ahead of launch and ready. The 2.2 s left in netif is not the lease; it is not the WITNESS backtraces either: gate run 7 (QUIET_WITNESS=1, debug.witness.trace=0) cut the serial log from ~1300 to 333 lines but netif only to 2.1 s. The cause: network.subr's `_ifconfig_getargs` falls back to `$ifconfig_DEFAULT` for an interface's `_ipv6` lookup too, so with any `ifconfig_DEFAULT` every port counts as IPv6-configured and rc.d/netif sleeps dad_count + 1 s for DAD. Gate run 8 (vtnet0 on its own `ifconfig_vtnet0="DHCP"`, no `ifconfig_DEFAULT`): netif 83 ms, rc 2.3–2.5 s, ssh back after a reboot 5.1 s. Atrium hosts must not set `ifconfig_DEFAULT` (it also hands every port to rc instead of atrium-net). Run 7 also set the dev VM's firmware Timeout to 0 (FIRMWARE_TIMEOUT=0; it waited 5 s before the loader): ssh back after a reboot 9.2 s (27 s at B0). frescod's fresco, fresco-wm and fresco-activation sockets are now supervisor-bound too (not yet run: the dev VM has no frescod). `atrium-bind --path SOCK [--mode --owner --group] -- DAEMON` binds, hands the socket over at fd 3 with `ATRIUM_LISTEN_FDS`/`PATHS` and execs (it execs anyway if binding fails, so the daemon binds as before). atrium-volumes and atrium-net take the inherited listener; their rc scripts use atrium-bind when it is installed. The bootstrap's shutdown closes every procdesc at once, then unmounts and removes jails with one shared 2 s budget for busy mounts.* | |

## 10. Open questions for Shankar

1. **Start the jailed tree before the network** (B1). Recommended: yes. Nothing
   in it needs the network at start, and it takes the network waits off the
   login path entirely.
2. **Supervisor-bound sockets as the main readiness mechanism** (§5), with the
   readiness fd only where sockets don't cover it. Recommended. The
   alternative, every service sends READY and the launcher starts dependents in
   order, is simpler to explain but keeps services waiting on each other and
   does nothing for restarts.
3. **`idle` boot class** (§6.3): delay non-essential services until the login
   screen is up. Recommended, with the list kept short.
4. **Dev images**: keep `REQUIRE: sshd` for kmods and the memory daemons on
   dev images only (a crashing kmod must leave ssh reachable), off on product
   images?
5. **Update `service-management.md` §4.9**, which says socket pre-binding
   exists, to point here.
