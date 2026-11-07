# Per-jail system call filter (sysfil)

**Status:** design note and first implementation, 2026-11-07. Step 2 of the FreeBSD kernel
security review ([2026-11-07-freebsd-kernel-security-rust.md](../reviews/2026-11-07-freebsd-kernel-security-rust.md)
§9, §9.1; decision D5, accepted by Shankar 2026-11-07). Kernel patch on the fork's
`atrium-os` branch (`sys/kern/kern_jail_sysfil.c`); tests in `tests/sys/kern/jail_sysfil.c`.
Written to be offered upstream, so the kernel side contains nothing Atrium-specific.

**Requirements from Shankar (2026-11-07):** measure candidate profiles against the past
advisories first (§2); build and test it well enough that FreeBSD could accept it (§6, §7);
and do not give up the jail's near-native performance (§4).

## 1. What it is

A jail limits what a process can *name* (files, other processes, addresses). It does not
limit what the process can *call*: every jailed app can reach every system call, ioctl and
socket option in the kernel, and the review found 44 kernel advisories from 2020–2026 that
an ordinary Atrium app jail could have reached, 26 of them memory-safety bugs (review §6).

sysfil gives a jail an allowlist, set when the jail is created:

| jail parameter | value | meaning |
|---|---|---|
| `sysfil.mode` | `0` off, `1` log, `2` deny, `3` kill | log allows but records and logs; deny fails the operation; kill fails it and sends `SIGSYS` |
| `sysfil.syscalls` | `read,write,openat,…` (names from `syscallnames[]` or numbers) | system calls the jail may make |
| `sysfil.ioctls` | `0x4004667f,…` | ioctl(2) commands |
| `sysfil.sockopts` | `0xffff:4097,6:1,…` (`level:name`) | setsockopt/getsockopt options |
| `sysfil.families` | `unix,inet,inet6` (names or numbers) | address families for socket(2)/socketpair(2) |
| `sysfil.denied` | read-only counter | operations denied, or in log mode that would have been |
| `sysfil.seen.{syscalls,ioctls,sockopts,families}` | read-only lists | what was denied (log mode: what an app would need) |

A list that is not given leaves that class unrestricted. An empty list allows nothing in
that class. The lists read back with `jls -n` or `jail_get(2)`, with `*` for an
unrestricted class.

It is an ordinary jail feature: `jail.conf` can use it, `jls` shows it, and it needs no
new system call.

## 2. Measured against the past advisories

**Method.** For each of the 44 kernel advisories marked A (reachable by an app jail) in
[the review's data file](../reviews/data/2026-11-07-freebsd-sa-2020-2026.csv), I read the
advisory text (fetched from the freebsd-doc repository) and wrote down what an app must be
able to call to reach the bug:
[2026-11-07-sysfil-gates.csv](../reviews/data/2026-11-07-sysfil-gates.csv). A bug counts as
closed by a profile if the profile denies at least one thing the exploit needs (for
example SA-26:32 needs both `procctl(2)` and a setuid `execve(2)`, so denying either closes
it). [`scripts/sysfil/score-profiles.py`](../../scripts/sysfil/score-profiles.py) does the
counting, and writes the per-advisory matrix
[2026-11-07-sysfil-matrix.csv](../reviews/data/2026-11-07-sysfil-matrix.csv).

| profile | kernel A rows closed | memory-safety A rows closed |
|---|---|---|
| no filter (today) | 0 / 44 | 0 / 26 |
| syscall bitmap only, no argument filters | 18 / 44 | 9 / 26 |
| GUI app, renderer, decoder, document worker, networked app (§5, as shipped) | **27 / 44** | **17 / 26** |
| the same, plus no core dumps (`RLIMIT_CORE=0` set by jaild) | **30 / 44** | **20 / 26** |
| the same, if the filter started at the app's `execve` (v2, §3.6) | 32 / 44 | 21 / 26 |
| terminal (§5: exec, tty ioctls, `copy_file_range`, POSIX timers allowed) | 23 / 44 | 14 / 26 |
| terminal, plus no core dumps | 26 / 44 | 17 / 26 |

What the numbers say:

- **The argument filters are half the value.** A syscall bitmap alone closes 9 of the 26
  memory bugs; the ioctl, socket-option and family lists add 9 more. Most 2026
  use-after-frees sat behind a call every app makes (`ioctl`, `setsockopt`) with an argument
  almost no app uses: `TIOCSCTTY` and `TIOCNOTTY` (SA-26:62, SA-26:10), `IPV6_MSFILTER`
  (SA-26:29), `IPV6_2292PKTOPTIONS` (SA-20:20), `SO_ACCEPTFILTER` (SA-21:09), the largepage
  configuration ioctl (SA-26:63), and a routing socket (SA-26:05).
- **The non-terminal classes score the same on this data.** Nothing in the A rows separates
  a renderer from a networked app: the advisories that need the network stack are all behind
  socket options no class needs. The classes differ in what stays reachable (a renderer has
  no inet sockets at all, so future TCP and IPv6 bugs stay out of reach), which the history
  cannot score.
- **Core dumps are a separate, free win.** Three memory bugs (SA-26:55, SA-22:09, SA-20:03)
  are in the ELF core dump writer, reached by crashing rather than by a call. jaild sets
  `RLIMIT_CORE` to 0 for app jails; no kernel change is needed.
- **What stays open (14 for the app classes):** the two `execve` bugs (SA-26:39,
  SA-26:13), because every v1 profile has to allow `execve` (§3.6), and bugs behind calls every app makes:
  `unix` sockets with `SCM_RIGHTS` (SA-26:57, SA-26:04, SA-26:66), `poll`/`select` racing
  `close` (SA-26:19), `unlinkat` (SA-26:42), `mmap`/`fork` VM bugs (SA-22:11, SA-21:08),
  `getdirentries` (SA-21:01), the `copyin` fault path (SA-21:11). Three more need finer
  argument filters that v1 does not have: `_umtx_op(UMTX_OP_SHM)` (SA-24:14), the
  `kern.ttys` sysctl (SA-24:02) and IPv6 hop-by-hop options sent as `sendmsg` control data
  (SA-20:24). They are listed in §8 as candidates for v2.

**Caveat: this is hindsight.** The profiles were written after reading the bugs, so the
score is an upper bound on what they would have done in advance. What keeps it honest is
that every denial is of something the app class does not use: the argument for the filter
is that unused kernel code reachable from an app is where bugs keep turning up, and the
2026 advisories (most of them found by AI-assisted audits of old code) are consistent with
that. The profiles in §5 are checked by running the apps in log mode, not by this table.

## 3. Mechanism

### 3.1 Where it is checked

| check | location | errno when denied |
|---|---|---|
| syscall number | `syscallenter()` (`kern/subr_syscall.c`), after argument fetch and the ptrace stop, before the Capsicum check | `ENOSYS` |
| ioctl command | start of `kern_ioctl()` (`kern/sys_generic.c`) | `EPERM` |
| socket option | `kern_setsockopt()` and `kern_getsockopt()` (`kern/uipc_syscalls.c`) | `EPERM` |
| address family | `kern_socket()` and `kern_socketpair()` | `EAFNOSUPPORT` |
| largepage shm object | `kern_shm_open2()` with `SHM_LARGEPAGE`: allowed only if `FIOSSHMLPGCNF` is in the ioctl list | `EPERM` |

The errnos are chosen so that well-behaved software falls back instead of failing:
`ENOSYS` is what libc and Rust probes already treat as "not supported" (a
`copy_file_range` or `kqueuex` probe falls back to the old call), and `EAFNOSUPPORT` is
what an app sees on a kernel without IPv6. `EPERM` for ioctls and socket options says what
happened; `ENOTTY` would have been mistaken for the wrong descriptor type.

In kill mode the process also gets `SIGSYS`, as it does today from `nosys()`.

`syscall(2)` and `__syscall(2)` are filtered by the real call number: the architecture's
`sv_fetch_syscall_args` has already unwrapped them when the check runs. A process running a
foreign ABI (`COMPAT_FREEBSD32`, the Linux ABI) in a filtered jail is denied every call,
because the filter is written against the native table and cannot speak for another.

### 3.2 Inherit and narrow

- The filter is fixed at `JAIL_CREATE`. A `JAIL_UPDATE` that names any `sysfil.*` parameter
  fails with `EINVAL`, whether the jail has a filter or not. A filter that could be added to
  a running jail would leave processes whose credentials predate it, and one that could be
  changed would break the next rule.
- A child jail's filter is its parent's intersected with what the child asked for, and its
  mode is the stricter of the two. A child that names no `sysfil.*` parameter gets an exact
  copy of its parent's filter. So every descendant of a filtered jail is filtered at least as
  tightly, and a jailed root cannot make a looser child to escape into.
- Prison 0 (the host) never has a filter.

### 3.3 How a process is marked

`do_jail_attach()` sets `CRED_FLAG_SYSFIL` in the new credential when the jail has a
filter. Credentials are copied on fork and on every credential change (`crcopy()` copies
`cr_flags`), so the flag follows the process through `fork`, `setuid` and `exec`. It is
never cleared: by §3.2 a process can only attach to jails below its own, and those are all
filtered. A check that finds the flag set but no filter on the prison fails closed.

### 3.4 Data and locking

The filter (`struct sysfil`, about 600 bytes plus the sorted ioctl and socket-option
arrays) hangs off `struct prison` in the field that was `pr_sparep`, so the structure keeps
its size and layout and modules built against the old one keep working. It is also stored
in an OSD slot, so it is freed with the prison by the OSD destructor; parameter handling
uses the `PR_METHOD_CHECK`, `PR_METHOD_CREATE` and `PR_METHOD_GET` hooks the same way
`kern_jailmeta.c` and SysV IPC do. `kern_jail.c` itself gains four lines.

The filter is written once, in `PR_METHOD_CREATE`, before the prison is alive and before any
process can attach, and never changes after that. The checks therefore read it without a
lock. Only the record of what was denied is written later: the seen-syscall and
seen-family bitmaps with atomic bit sets, the short seen-ioctl and seen-sockopt lists under
a per-filter mutex. Those writes happen only on a denial, never on an allowed call.

### 3.5 Log mode and profile generation

In log mode every operation the filter would deny is allowed, counted, recorded in the
`sysfil.seen.*` parameters, and logged through `log(9)` at most
`security.jail.sysfil_log_rate` times a second per jail (default 5; 0 turns logging off).
To build a profile: start the app in a jail with `sysfil.mode=1` and the candidate lists,
exercise it, read back `sysfil.seen.*`, and add what is missing.
[`scripts/sysfil/learn.sh`](../../scripts/sysfil/learn.sh) does this on a test machine:
it resolves a class from `etc/jaild.policy.toml`
([`resolve-profile.py`](../../scripts/sysfil/resolve-profile.py)), runs a binary in a jail
with that filter in log mode (or deny mode, `MODE=2`), and prints what was denied.
[`check-profiles.py`](../../scripts/sysfil/check-profiles.py) checks every syscall name in
the profiles against the kernel's table (`kern/syscalls.c`), so a misspelt or renamed call
(the POSIX timer calls are `ktimer_*` in the kernel) is caught before a jail is made.

### 3.6 Why every profile allows `execve`

jaild's launcher creates the jail, attaches to it, and only then drops privileges and
runs the app: `setgroups`, `setgid`, `setuid`, `execve`, all inside the filtered jail.
So every profile has to allow those calls, and `execve` stays reachable by the app
itself afterwards. This costs two advisories (SA-26:39, SA-26:13). Two ways to close them,
both v2 (§8): a "filter starts at exec" flag, where the filter takes effect for a process
at its first `execve` after attaching (the launcher's own `execve` is the last call made
unfiltered), or a launcher that attaches through a pre-opened descriptor and `fexecve`
with the app's credential already set. The scorer's "v2" row is the first.

## 4. Performance

Shankar's requirement: jails are prized for running at near-native speed, and the filter
must not change that.

**Design.** No BPF interpreter and no per-call policy evaluation. For a process outside a
filtered jail, which is every process today, the whole cost is one test of a credential
word that `syscallenter()` already reaches for capability mode, and a branch that is
predicted not taken: one load and one test per system call, and nothing in `kern_ioctl()`,
the sockopt paths or `socket()` beyond the same test. A process inside a filtered jail pays
two more dependent loads (prison, filter) and a bit test per system call, and a binary
search over a few dozen entries per ioctl or socket option.

**Gate (review §9.1).** `lat_syscall`-style null-call latency within noise (under 1%) with
the patched kernel against the unpatched one, and with a filtered jail against an
unfiltered one; Navigator, Fresco and Tessera benchmarks unchanged with the filter on and
off; arm64 and amd64.

**Measurements.** See §7. The cloud VM runs under TCG, where a system call costs
microseconds and the noise is several percent, so its numbers can show a gross regression
but cannot prove "under 1%". The 1% gate is measured on hardware virtualisation: the Mac's
HVF VM (arm64) and KVM on the R9700 box (amd64).

## 5. Profiles by app class

### 5.1 The classes

| class | who | syscalls beyond the base | ioctls | families | sockopts |
|---|---|---|---|---|---|
| `renderer` | Fresco renderers, decoders, Tier-2 executor, Navigator layout/JS workers (`document worker`) | none | base | `unix` | `SOL_SOCKET` basics |
| `app` (default) | ordinary Pergola apps | none | base + `TIOCGETA`/`TIOCGWINSZ` | `unix` | `SOL_SOCKET` basics |
| `networked` | Nuntius fetchers, apps with the `network` capability | `sendto`/`recvfrom` family, `accept4`, `listen` | as app | `unix,inet,inet6` | basics + TCP/IP options apps use |
| `terminal` | the shell and what it runs | `execve`, `fexecve`, `copy_file_range`, POSIX timers, `setsid` | as app + tty control | `unix,inet,inet6` | as networked |

"Base" is what Rust std, libc, rtld and libthr call at runtime. Never in any app class:
`ktrace`, `ptrace`, `setcred`, `jail_*`, `kqueuex`, `sigqueue`, `thr_kill2`, `procctl`,
`aio_*`/`lio_listio`, `sendfile`, `mount` and the other privileged calls; the tty
control ioctls outside `terminal`; `IPV6_MSFILTER` and the other multicast filters,
`IPV6_2292PKTOPTIONS`, `SO_ACCEPTFILTER`, `SO_REUSEPORT_LB`, KTLS options; `route`, `key`,
`netlink`, `divert` and `netgraph` sockets; largepage shm objects.

The concrete lists live in the `[sysfil]` section of
[`etc/jaild.policy.toml`](../../etc/jaild.policy.toml) as profiles `base`, `renderer`,
`app`, `networked` and `terminal`; a profile can `include` others (resolved by jaild, with
a cycle check and a depth limit of 8). They are checked with log mode (§3.5).

### 5.2 Where the class comes from

Portcullis decides, not the app. The manifest's `[capabilities] syscalls = "<class>"` names
the class; `network` implies `networked`; `terminal` is a restricted capability that only
system bundles and user-approved apps get, like `window-management`. System bundles
(renderers, Nuntius, Navigator workers) name theirs in their own manifests.

### 5.3 jaild

`CreateJailRequest.syscall_profile` names a class. jaild looks the class up in its profile
file, refuses an unknown name, and passes the lists as `sysfil.*` parameters to
`jail_set(2)` along with the jail's other parameters, so the filter exists before the first
process attaches. It also sets `RLIMIT_CORE` to 0 in the child before exec.
`policy.sysfil.required = true` makes jaild refuse to create an app jail without a profile,
and `policy.sysfil.mode` (default 2) lets a development box run everything in log mode.
The request validator rejects an unknown profile (rule `sysfil.profile`) and a missing one
when required (rule `sysfil.required`). The lists are built in `ffi::IovBuilder::add_sysfil`
for both the routed and the loopback-only jail paths. On a kernel without sysfil,
`jail_set(2)` rejects the unknown parameters, so jaild fails closed rather than creating
an unfiltered jail; `required = false` (the default today) is what lets stock kernels run
app jails without a profile.

## 6. Upstream

The kernel patch is written to be offered to FreeBSD as it is:

- **Shape.** One new file (`kern_jail_sysfil.c`), small hooks in `subr_syscall.c`,
  `sys_generic.c`, `uipc_syscalls.c`, `uipc_shm.c` and `kern_jail.c`, a new `cr_flags` bit,
  and `pr_sparep` renamed. No new system call, no change to the jail(2) ABI, no change to
  `struct prison`'s size, and nothing Atrium-specific.
- **Tests.** `tests/sys/kern/jail_sysfil.c`, 13 ATF cases (§7), run by kyua like the rest of
  `tests/sys/kern`.
- **Documentation still to write before submitting:** a `jail(8)` section for the
  parameters, `jail.conf(5)` examples of profiles built with variables, and the sysctl.
- **Questions reviewers are likely to ask, with our answers:**
  - *Why not MAC?* A MAC policy has no hook at system call entry, and `mac_*_check_ioctl`
    hooks exist only for some objects. The check has to be in `syscallenter()` to be cheap
    and complete.
  - *Why not extend Capsicum?* Capsicum's per-call flag is global (one table for every
    process), and capability mode is all-or-nothing. sysfil is per jail and composes with
    Capsicum: a capability-mode process in a filtered jail passes both checks.
  - *Why strings for the lists?* So `jail.conf` and `jls` work with no libjail change. The
    kernel parses them once, at jail creation; nothing is parsed on the call path.
  - *Why a credential flag?* It makes the unfiltered path one test of a word already in
    cache. Testing `cr_prison->pr_sysfil` instead would add a dependent load to every system
    call on the system.
  - *Why can't it be changed on a running jail?* §3.2. Narrowing a running jail is possible
    (publish a new filter and free the old one after an epoch) and can come later if anyone
    needs it.

## 7. Status and test results

| item | state |
|---|---|
| design note and advisory scoring | this document |
| kernel mechanism (syscalls, ioctls, sockopts, families, largepage, log/deny/kill, inherit-and-narrow, read-back) | done; atrium-os/freebsd-src `atrium-os` 93437440e; amd64 GENERIC-NODEBUG builds with `-Werror` |
| ATF tests | 13/13 pass under kyua on the patched amd64 kernel (cloud VM, TCG) |
| stock jail tests (`tests/sys/kern/jail*`, `usr.sbin/jail`) on the patched kernel | see below |
| jaild + policy | done: 81 jaild unit tests and 8 jaild-policy tests pass in the VM, plus the `sysfil_reaches_the_kernel` round trip (jaild creates a filtered jail, `jls` reads the filter back) |
| profiles against real binaries | `app` in deny mode runs a Rust std probe (threads, files, sockets, timers, env, process) with 0 denials; `renderer` in deny mode runs `sysfil-bench` with 0 denials. Real Pergola apps, Fresco renderers and Nuntius: pending, through `learn.sh` |
| Portcullis manifest `[capabilities] syscalls` to class | pending (jaild takes the class; Portcullis does not yet pass one) |
| arm64 build and VM run | pending |
| overhead, cloud TCG (sanity) | done, below |
| overhead under 1%, HVF (Mac, arm64) and KVM (R9700, amd64) | pending |
| Navigator, Fresco, Tessera benchmarks with the filter | pending |
| jail(8) man page section | pending |

**Unfiltered path, by instruction count.** On amd64 the patch adds exactly three
instructions to `amd64_syscall` for a process outside a filtered jail (from the
disassembly of the built kernel): `mov 0x188(%rbx),%rax` (td_ucred), `testb $0x4,0x2c(%rax)`
(cr_flags), and a not-taken `jne`. `td_ucred` is already in cache, read a few instructions
later by the capability-mode test.

**TCG latency (sanity only).** `scripts/sysfil/sysfil-bench.c`, best of 7 runs of 2 million
calls, three rounds each on the same VM, unpatched kernel and patched kernel booted in turn.
Best round, ns per call:

| | null | nullio | ioctl | sockopt | socket |
|---|---|---|---|---|---|
| unpatched, host | 6906 | 10122 | 8730 | 8332 | 29356 |
| unpatched, open jail | 6882 | 10261 | 8826 | 8497 | 29767 |
| patched, host | 7050 | 9906 | 8764 | 8391 | 29103 |
| patched, open jail | 7002 | 9995 | 8759 | 8480 | 29483 |
| patched, filtered jail (`renderer`-style lists allowing all five) | 7344 | 10196 | 9583 | 9398 | 30829 |

Under TCG a system call costs about 7 µs, and the same configuration varies by up to 4%
between rounds (unpatched open jail null: 6882 to 7140 ns), so these numbers show no gross
regression and cannot resolve 1%. Patched against unpatched is within that noise in both
directions (null +2%, nullio -2%). The filtered jail is 4 to 11% slower than the open jail
here; under TCG every added load goes through the software MMU, so this overstates the
cost on hardware, but it is the number to check first on HVF and KVM.

## 8. Not in v1

- **`sendmsg(2)` control messages** (SA-20:24: IPv6 options as ancillary data). A
  `cmsg` level/type allowlist checked in `sosend` would close it.
- **`_umtx_op` sub-operations** (SA-24:14, `UMTX_OP_SHM`). A per-op bitmap; `_umtx_op` is
  hot, so the test must sit only on the filtered path.
- **sysctl reads** (SA-24:02, `kern.ttys`). A per-jail sysctl allowlist is a bigger design.
- **No setuid transitions on exec in a filtered jail**, as capability mode already does
  (`kern_exec.c`). It would close SA-26:39, SA-26:32 and SA-24:06 even for `terminal`.
  Mounting app roots `nosuid` does the same from user space; jaild does not do that yet.
- **Filter starts at exec** (§3.6): closes SA-26:39 and SA-26:13 for every app class.
- **Narrowing a running jail** (§6).
- **Socket-option level 0 is ambiguous.** `SOL_LOCAL` and `IPPROTO_IP` are both 0, so
  allowing `0:LOCAL_PEERCRED` (1) also allows `IP_OPTIONS` (1) on an inet socket. The
  profiles that allow inet sockets do not rely on the distinction today; a v2 check could
  key level 0 on the socket's domain.
