# Scoping: FreeBSD kernel and base security, and whether to port to Rust

**Status:** scoping study, 2026-11-07; work since tracked in §9. Shankar 2026-11-07: incremental path (§8.3) taken, no rewrite (D1); D2 decided 2026-11-10 (Rust by default for new kernel code); step 4 phase 1 (on-disk decoders in Rust, D3) built 2026-11-10, see [rust-decoders.md](../../atrium-tessera/docs/rust-decoders.md); new rule D9 (user space first for hostile-input parsers) agreed. D3–D8 accepted as recommended (Shankar 2026-11-07), including the per-jail syscall filter (§9.1), offering it upstream, and measuring profiles against past advisories first. Upstream-quality bar: built and tested carefully enough that FreeBSD could accept it.
**Question (Shankar):** how stable and secure are the FreeBSD kernel and its core? What do
the CVEs say, especially memory bugs that come from C? Does it make sense to port it to
Rust? A port would be a very large commitment, so the analysis should be good enough now to
save time later.
**Data:** every FreeBSD security advisory from 2020-01 to 2026-10-21 (188 advisories,
FreeBSD-SA-20:01 to SA-26:73), read from freebsd.org and classified by hand in
[`data/2026-11-07-freebsd-sa-2020-2026.csv`](data/2026-11-07-freebsd-sa-2020-2026.csv). Code
sizes measured on the FreeBSD 15.1-RELEASE `src.txz`. Atrium sizes measured on main at
6658a64.
**Related:** LANGUAGE-POLICY.md ("Kernel = C"), threat model (`spec/atrium-navigator-threat-model.md`),
portcullis.md, network.md (§0, vnet + epair + pf), atrium-input.md (Tactus), bluetooth.md
(Vinculum), 2026-11-07-linux-compat-scoping.md (no linuxulator), gpu-isolation.md,
tessera-impl.md §6.6–6.7 (fuzzing plan).

## 1. Short answer

- **FreeBSD is stable and well run, and its kernel is about as memory-unsafe as any large C
  kernel.** From 2020 to 2025 it averaged about 19 advisories a year, 10 of them in the
  kernel. **2026 has had 73 so far, 53 of them in the kernel.** The code did not suddenly get
  worse. AI-assisted bug hunting (Anthropic, OpenAI, AISLE, Calif.io, Praetorian and others)
  started reading old kernel code at scale, and the FreeBSD Foundation answered with a funded
  AI-assisted audit (June 2026). Expect the high rate to continue for a while.
- **Of the 111 kernel advisories, 65 (59%) are memory-safety bugs**: 32 out-of-bounds, 24
  use-after-free or refcount, 9 uninitialised-memory leaks. Safe Rust would have removed
  the bug in 61 of the 111 (55%), and made it much less likely in 17 more. **33 (30%) are logic
  bugs that Rust does not touch**, and those include most of the jail and Capsicum escapes,
  which are the bugs that matter most to Atrium.
- **Atrium already sits on top of a large part of that history.** 42 of the 111 kernel
  advisories are in subsystems Atrium does not use or can compile out (linuxulator, compat32,
  SysV IPC, SCTP, RACK, ipfw/libalias, NFS, ZFS, fuse, netmap, Xen, bhyve). Bluetooth and
  input parsing already moved to user space (Vinculum, Tactus). **What is left is the syscall
  surface every jailed app can reach**: 44 kernel advisories, 26 of them memory bugs, mostly
  use-after-free races in kqueue, unix sockets, tty, shared memory, poll/select and exec.
  Jails do not filter syscalls, so those bugs cross the jail boundary.
- **A full Rust port of the FreeBSD kernel does not make sense for Atrium.** The part of
  `sys/` Atrium actually runs is about 1.6 million lines of C (9.9 million in all of `sys/`).
  Every comparable project (Redox, Asterinas, Theseus) took years with a team and still lacks
  the drivers. We would also become a permanent fork that has to port every FreeBSD security
  fix by hand, which in 2026 would have been 53 kernel fixes.
- **An incremental path does make sense**, in this order: shrink the kernel we ship
  (weeks, no Rust); fuzz and AI-audit **Atrium's own ~70k lines of kernel C** (Tessera, GPU
  driver, zram), which is newer and less reviewed than anything in FreeBSD's core; tighten
  what an app jail can call; then write **new Atrium kernel code in Rust**, starting with
  Tessera's on-disk decoders; and follow FreeBSD's own Rust KPI work instead of inventing
  ours. That changes one line of LANGUAGE-POLICY.md ("Kernel = C"), which is decision D2.

## 2. What Atrium runs from FreeBSD

| layer | FreeBSD code Atrium depends on | Atrium's own code there |
|---|---|---|
| kernel core | `kern/` (289k lines: processes, VFS, sockets, kqueue, jails, Capsicum), `vm/` (49k), arm64 (114k) and soon amd64/x86 (158k) | — |
| networking | `net/`, `netinet/`, `netinet6/` (372k), pf (39k), epair, vnet. Each app has its own vnet stack and an epair; pf on the host is the policy (network.md §0) | — |
| filesystems | devfs, nullfs, tmpfs, fdescfs, procfs, VFS | **Tessera**: `tessera_fs.ko` (32k lines of C) + tessera-core (16.5k, also linked into the kernel) |
| devices | USB (200k), hid/hidraw (14k), virtio (22k), NVMe, GEOM | **atrium-gpu-amd** (12.6k), atrium-kmod virtio-gpu etc. (5.3k), atrium-zram (3.5k), fresco-kmod, carillon-kmod (1.3k) |
| user space | libc (240k), rtld, init/rc, devd, pf tools, dhclient, rtsold | almost every daemon and app is Rust (LANGUAGE-POLICY.md); TLS is rustls only (TLS.md), nothing links OpenSSL |

Measured with `wc -l` over `*.c`/`*.h` in FreeBSD 15.1 `src.txz`. "About 1.6M lines" is `kern`,
`vm`, `net*`, pf, netlink, `security`, the filesystems above, GEOM, CAM, USB, hid, virtio,
NVMe, `arm64`, `amd64`, `x86`, crypto and libkern. FreeBSD-authored userland (`bin`, `sbin`,
`usr.bin`, `usr.sbin`, `lib`, `libexec`, not counting `contrib/`) is another 1.7M. There is
no Rust anywhere in `src`.

We also already carry a small FreeBSD fork (atrium-os/freebsd-src, branch `atrium-os`,
e.g. the zram swap-free notification and swap fast path, fbsd:`da0b30649`, fbsd:`482ce93dd`).
It is a thin patch set over upstream, which is what keeps taking FreeBSD's fixes cheap.

Two things follow. First, the FreeBSD part we depend on is enormous next to anything we
could rewrite. Second, **Atrium has already added about 70k lines of its own kernel C**, all of
it recent, and the Tessera kernel module is now larger than FreeBSD's
whole jail, Capsicum, kqueue and unix-socket code put together.

## 3. Stability

FreeBSD's reputation for stability is earned, and nothing in this review changes it:

- Long-running production users run it at very large scale on exactly the subsystems we use:
  Netflix (network stack, TCP, KTLS, sendfile), NetApp, Juniper, Sony. The kernel's
  locking, VM and network stack carry that load every day.
- The project ships on a fixed schedule (a major release every two years, a minor release
  every six months, from 15.0 in December 2025), with `releng/*` branches that receive
  security fixes as numbered advisories.
- Development kernels carry `INVARIANTS` and `WITNESS` (our VM's GENERIC has both, RUNBOOK
  §kdb), and there are KASAN (amd64 and arm64), KMSAN (amd64) and KCSAN sanitizers, plus
  syzkaller support with a public syzbot instance.

The stability risk for Atrium is our own position, not FreeBSD: **we develop on
16-CURRENT**, which receives fixes first but has no advisories, no errata and no promise of
stability, and our own kernel modules (Tessera especially) are where our panics have come
from (atrium-tessera/docs/forensics-136-dangling-root.md). Decision D6 is about which branch
a shipped Atrium follows.

## 4. The security record, 2020–2026

### 4.1 Count by year

| year | advisories | kernel | of which memory-safety | FreeBSD userland | third-party (contrib) | bhyve |
|---|---|---|---|---|---|---|
| 2020 | 33 | 23 | 18 | 5 | 5 | 0 |
| 2021 | 17 | 9 | 2 | 4 | 3 | 1 |
| 2022 | 15 | 8 | 5 | 2 | 4 | 1 |
| 2023 | 19 | 7 | 3 | 5 | 5 | 1 |
| 2024 | 19 | 7 | 2 | 3 | 4 | 5 |
| 2025 | 12 | 4 | 3 | 2 | 6 | 0 |
| **2026 (to Oct 21)** | **73** | **53** | **32** | 10 | 7 | 1 |
| total | 188 | 111 | 65 | 31 | 34 | 9 |

(Two CPU-errata advisories and one CA-store update are in the totals but in none of the
columns.)

**Why 2026 looks like that.** The credit lines tell the story. At least 14 of the 73 name an
AI tool or AI security team directly ("Nicholas Carlini using Claude, Anthropic",
"Brian Delgado from Intel using Anthropic Opus 4.8", "the OpenAI Codex Security Team",
"Joshua Rogers of AISLE Research Team" six times, "Atuin – Automated Vulnerability Discovery
Engine", codecome.ai), and Calif.io and Praetorian describe their FreeBSD kernel finds (six
more advisories) as AI-driven audits. The bugs themselves are often old: SA-26:28 is a
Capsicum check missing since sigqueue(2) entered capability mode in 2011. June 2026 had
the most advisories in a month in the project's history, and on 2026-06-15 the FreeBSD
Foundation started a six-month, Alpha-Omega-funded AI-assisted vulnerability discovery
project, kernel first, then base userland, then ports; AI finds, people triage and patch.

Two readings matter for us:

1. **The code was always like this.** The 2020–2025 rate measured how hard people looked,
   not how many bugs there were. The 2026 rate is closer to the true density of latent bugs
   in a 30-year-old C kernel.
2. **Attackers have the same tools.** CVE-2026-4747 (SA-26:08, RPCSEC_GSS) came with a
   public write-up of a working remote kernel exploit written by an AI model. The time from
   "bug is findable" to "bug is exploited" is shrinking, so patch speed (D6) now matters as
   much as bug count.

FreeBSD advisories are not comparable one-for-one with Linux CVEs: one advisory can bundle
several CVEs, and since 2024 the Linux kernel CNA assigns a CVE to almost every fix. We use
advisories because they are the unit FreeBSD triages and ships.

### 4.2 Kernel advisories by bug class

| class | count | share | what it looks like here | safe Rust? |
|---|---|---|---|---|
| out of bounds (spatial) | 32 | 29% | integer overflow in a size (vt CONS_HISTORY, sound mmap offset+length, RACK 16-bit counter, ZFS 64→32-bit truncation); unchecked length into a stack buffer (setcred, rpcsec_gss, route sockets, libalias RTSP); off-by-one (KTLS 1.3 padding) | yes: bounds-checked slices and checked or explicit-wrapping arithmetic |
| use-after-free, double free, refcount (temporal) | 24 | 22% | the dominant pattern: **drop a lock to sleep or copyin, take it back, keep using a pointer that is no longer valid** (kqueue fork copy twice, tty TIOCSCTTY, sound SYNCSTART, IPV6_MSFILTER, RACK setsockopt, sysvsem); error path releases a reference twice (jail JAIL_AT_DESC, umtx) | yes, mostly: a guard's lifetime ends when the lock is dropped, so the stale borrow does not compile, and references are `Arc`-like and released once by `Drop`. The race remains and becomes a wrong answer or an error instead of memory corruption |
| uninitialised memory to user | 9 | 8% | stack struct not zeroed before copyout (compat32 kevent, Linux waitid, ktimer CLOCK_TAI, dirent d_off) | yes; also removed today in C by `-ftrivial-auto-var-init=zero` (§5) |
| logic: missing or wrong check | 39 | 35% | jail root escapes via fd passing, nullfs and fdescfs (SA-26:66, 26:04, 26:02); Capsicum checks missing (sigqueue, copy_file_range); a flag silently dropped (unlinkat AT_RESOLVE_BENEATH); a result not checked (wg(4) MAC, thr_kill2); credential race in execve; ASLR order bug for setuid | **no**. A few become harder (`#[must_use]` on a `Result` would have flagged wg(4) and thr_kill2), most do not |
| leak, NULL, unbounded recursion, DoS | 7 | 6% | mbuf leak (TCP challenge ACK), NULL in nvmf, pf SCTP recursion | partly: ownership frees the mbuf, `Option` forces the NULL case; recursion still needs a depth limit |

By where the attacker stands: 78 local, 21 remote over the network, 3 from the LAN, 3 over
the radio (802.11), 3 from a malicious USB device, 3 from a guest VM. **18 kernel
memory-safety bugs were reachable from the network, a radio or a device.**

### 4.3 Kernel advisories by subsystem

| subsystem | all | memory-safety |
|---|---|---|
| network (TCP/IP, IPv6, pf, ipfw, NAT, KTLS, 802.11, SCTP, wg, NFS RPC) | 36 | 23 |
| drivers (sound, USB, console, storage HBAs) | 11 | 9 |
| jails and Capsicum | 11 | 2 |
| other syscalls (ktrace, ptrace, creds, signals, timers, aio, audit) | 12 | 4 |
| VM and shared memory | 8 | 3 |
| IPC and sync (kqueue, unix sockets, SysV, umtx, poll/select) | 7 | 7 |
| filesystems (ZFS, NFS, fuse, msdosfs, VFS) | 7 | 5 |
| exec and ELF | 6 | 4 |
| compat layers (Linux, 32-bit) | 4 | 3 |
| tty, storage targets, crypto, virtualisation | 9 | 5 |

The network stack is the largest single source and the source of almost every remote bug.
The jail and Capsicum rows are small in memory bugs but large in consequence: they are logic
bugs, and they are exactly the boundary Atrium's design rests on.

### 4.4 Userland and third-party code

Of the 31 FreeBSD-userland advisories, 18 are out-of-bounds bugs in network-facing C
(dhclient, rtsold, ping, libfetch, ppp, ggatec, libradius, libnv, libcasper, iconv) and 3 are
shell injection (dhclient lease file, rtsold domain list, the installer's Wi-Fi scan). Of the
34 third-party advisories, most are OpenSSL, OpenSSH, unbound and compression libraries.
**Atrium is mostly out of this column already**: our daemons are Rust and our TLS is
rustls. What remains on an Atrium host is libc, rtld, dhclient and rtsold (host side, LAN
reachable), libnv/libcasper (used by Capsicum's casper helpers, including the Navigator's
`cap_net`), and whatever sshd a developer enables.

## 5. Mitigations FreeBSD already has, and what they buy

| mitigation | state in FreeBSD | what it would have done to the record above |
|---|---|---|
| user ASLR, PIE | on by default for 64-bit since 13.x/14.0 | raises exploit cost; SA-26:32 shows the setuid ordering can still be wrong |
| kernel ASLR | **we could not confirm general KASLR in GENERIC.** Sources disagree. Check on our own 16-CURRENT arm64 and amd64 kernels (kernel text and `kern.` addresses across boots) before relying on it | — |
| SMEP/SMAP (amd64), PAN/PXN (arm64), stack protector, kernel W^X | present | raises cost; SA-21:11 was a SMAP bypass bug in the copyin path itself |
| arm64 pointer authentication, BTI | present on recent releases for userland and kernel; status on our build to be checked | raises cost of control-flow hijack after a memory bug, removes no bug |
| zero-init of automatic variables (`INIT_ALL=zero` in src.conf, clang `-ftrivial-auto-var-init`) | available, **not on by default** | removes the whole uninitialised-stack class (9 kernel advisories) at ~1% cost; we should turn it on (§9 step 0) |
| KASAN, KMSAN, KCSAN | development kernels only | find bugs before release; not a runtime defence |
| syzkaller / syzbot | runs against FreeBSD main | the main fuzzing source before 2026 |
| jails, vnet | default isolation for Atrium apps | contain user space, **not kernel bugs**: a jailed process reaches the full syscall table |
| Capsicum capability mode | Atrium's daemons and renderers enter it (portcullisd, Navigator fetcher, CDM host plan) | removes path-based opens, `execve`, `ptrace`, `setcred`, `jail_*` and namespace calls, so some advisories become unreachable; leaves kqueue, unix sockets, poll/select, shared memory, socket options, ioctls on held fds, which is where most 2026 use-after-frees are |
| MAC framework | available | could deny ioctls, socket options and shm configuration per jail (§9 step 3) |
| CHERI (CheriBSD) | CheriBSD 26.07 is current; a funded project is upstreaming pure-capability kernel and userland with FreeBSD 16 as the target | would stop spatial and most temporal bugs **in unmodified C**, but needs CHERI hardware (Morello boards, CHERI RISC-V). Our targets (Apple Silicon under QEMU/HVF, x86_64 R9700 box) have none. A research lane, not a plan |
| Arm MTE | FreeBSD arm64 support is in progress; Apple Silicon does not expose MTE to guests | probabilistic UAF/OOB detection if hardware ever offers it; not for our targets |

The pattern: FreeBSD has the usual exploit-cost mitigations, but nothing that removes the
memory-bug classes on hardware we will run. That is the gap Rust or CHERI would close.

## 6. How much of this Atrium already avoids

Each kernel advisory is marked in the data file by whether an Atrium app could reach it:

| exposure | kernel advisories | memory-safety | examples |
|---|---|---|---|
| **N**: subsystem Atrium does not use, or can compile out | 42 | 24 | linuxulator (we decided against it, 2026-11-07), compat32, SysV semaphores, SCTP, RACK, ipfw and libalias (we use pf), NFS and RPCSEC_GSS, ZFS (we use Tessera), fuse, netmap, CAM target, Xen, bhyve VMM, cryptodev, mpr/ixl/oce, wg |
| **S**: host-side subsystem we use that apps do not reach directly | 25 | 15 | pf (5, all logic), 802.11 (3), IPv6/ICMPv6 and TCP input (4), USB network and HID parsing (3), sound (only lyrad opens /dev/dsp, lyra-v1), console, KTLS |
| **A**: reachable by an ordinary jailed app | 44 | 26 | kqueue (2), unix sockets, tty (2), posix shm (2), poll/select, exec/ELF (6), ptrace, setcred, routing sockets, IPv6 socket options, timers, umtx, jail descriptors; and 17 logic bugs including both jail-root escapes and both Capsicum misses |

What Atrium's architecture already did:

- **No Linux ABI in the kernel** (linux-compat scoping, accepted): removes the compat-linux
  rows, and the linuxulator's long history before 2020.
- **Bluetooth in user space** (Vinculum, bluetooth.md §2): the radio is parsed by a jailed
  Rust daemon, not `ng_hci`/`ng_l2cap`. No FreeBSD Bluetooth advisory appears in 2020–2026,
  but Linux's BlueBorne and BleedingTooth show what that layer produces once someone looks.
- **Input in user space** (Tactus): HID reports are parsed in jailed Rust. The kernel still
  parses HID *descriptors* in `hidbus` and USB descriptors in `usb(4)` (SA-20:17), so the
  device attack surface is smaller, not gone.
- **No GPU fd in apps, no DRM/linuxkpi** (gpu-isolation.md, ABI v2.1): apps cannot reach our
  GPU driver at all in production; only the broker and frescod can.
- **No OpenSSL, Rust daemons**: most of the userland column does not apply.
- **vnet per app with pf on the host**: SA-26:69 (jail loopback bypass) only hits non-vnet
  jails. The flip side is that every app now drives a full IPv6 and TCP stack instance and
  its socket options, which is where several A-rows sit.

What it did not do: **a jail is not a syscall filter.** FreeBSD has no seccomp equivalent
per jail. Every jailed app can call kqueue, unix sockets, poll, shm, ioctls on what it
holds, and every 2026 use-after-free in those paths was a jail-to-kernel escalation. This is
the remaining place where FreeBSD's C memory bugs cut straight through Atrium's thesis
("every app is its own jail").

**And our own kernel C.** Tessera (48.5k lines in-kernel), the AMD GPU driver (12.6k),
virtio-gpu and friends (5.3k), zram (3.5k) are new, written fast, and have had no outside
audit. The 2026-11-03 GPU review found five isolation breaks in `atrium-gpu-amd` in one
pass (since fixed, gpu-isolation F1). Tessera parses on-disk structures in the kernel, which
is the classic source of filesystem CVEs as soon as someone mounts an untrusted image.
Line for line, this is probably the densest bug source in an Atrium kernel today, and it is
the only kernel C we fully control.

## 7. Rust in operating-system kernels: prior art

| project | what it is | lesson for us |
|---|---|---|
| **Linux, Rust for Linux** | started 2020; the "experimental" label was removed after the December 2025 Maintainers Summit (Ojeda's patch, January 2026). Shipping Rust: Android binder rewrite, the Nova (NVIDIA) and Tyr (Mali) GPU drivers, Asahi's Apple GPU driver (largely out of tree), null block, NVMe and PHY drivers; DRM maintainers expect to require Rust for new drivers within about a year | **New drivers in Rust, existing core in C.** Five years went mostly into safe bindings to C kernel APIs (locks, refcounts, devices, DMA), not into drivers. Nobody is rewriting the scheduler, VM or TCP |
| **Windows** | Rust in the kernel since Windows 11 24H2 (`win32kbase_rs.sys`, the GDI region code), plus DirectWrite font parsing | rewrite **the parsers that take hostile input**, behind the existing interface |
| **Android** | new code in Rust since 2019; memory-safety bugs fell from 76% of Android's vulnerabilities in 2019 to under 20% in 2025, with a reported ~1000x lower density in Rust than in C/C++ | the win comes from **writing new code in Rust**, not from rewriting old code; old C bugs decay as the code ages and is fixed |
| **FreeBSD upstream** | no Rust in `src`. The 2024 "Rust in base" discussion ended with Rust allowed only as an optional, out-of-base userland build (HardenedBSD's May 2025 work). Q4 2025 status report: one developer's out-of-tree kernel KPI bindings and a Rust virtio-sound driver, "not very stable" yet, hoped to be usable for new drivers in 2026; tested on QEMU and some Apple arm64 drivers | there is a starting point on exactly our architecture, but no upstream commitment. Rust kmods are possible today out of tree, at our own maintenance cost |
| **NetBSD, OpenBSD** | no kernel Rust | — |
| **Redox** | a whole OS in Rust, microkernel, around ten years in | proves it can be done; also shows the hardware long tail never ends for a small team |
| **Asterinas** | Rust "framekernel" with a Linux ABI (210+ syscalls, ATC 2025); unsafe code confined to one framework crate (OSTD), the rest is safe Rust; LMbench roughly at Linux parity | the best current design for a new Rust kernel, with a team of dozens over several years, and still aimed at containers and VMs with virtio, not laptops |
| **Theseus** | research single-address-space OS | ideas about live update, not a base |
| **CheriBSD** | FreeBSD with CHERI capabilities; FreeBSD 16 is the upstream target | the other route to memory safety, keeping C, but tied to hardware we do not have |

## 8. Options

### 8.1 A full rewrite of the kernel in Rust

Rewrite the parts of FreeBSD Atrium needs (about 1.6M lines) as a Rust kernel that keeps
FreeBSD's ABI, jails, Capsicum and drivers.

- **Effort.** Asterinas is the closest yardstick: a large team, several years, about 210
  Linux syscalls, a handful of virtio drivers. For our subset (FreeBSD syscalls, jails, vnet,
  pf, Capsicum, USB, NVMe, Wi-Fi, Apple and AMD hardware, Tessera), a realistic range is
  **50 to 150 person-years**, before the long tail of laptop hardware. AI help makes this
  faster but not ten times faster, because the hard part is semantics (VM, locking, TCP),
  not typing.
- **What it gives.** The memory classes go away, so the 61 "yes" rows and most of the 17
  "partial" rows. The 33 logic rows stay, and a new kernel would grow its own logic bugs in
  the jail and Capsicum equivalents, which today benefit from 25 years of FreeBSD review.
- **What it costs beyond effort.** We leave FreeBSD's security process. Every FreeBSD fix
  becomes ours to reproduce. Netflix-grade TCP, the VM and the drivers would all regress for
  years.
- **Verdict: no.** It solves a problem the size of FreeBSD with a team the size of Atrium.

### 8.2 A fork that rewrites FreeBSD's riskiest subsystems in Rust

Keep FreeBSD, but replace, in our tree, the subsystems the data points at: IPv6 input and
extension headers, TCP option and SACK handling, pf's packet parsing, 802.11 frame parsing,
USB descriptor parsing, kqueue, unix sockets, tty.

- **Effort.** 6 to 18 months per subsystem with tests, after a 2–3 month bindings layer.
  The network stack alone is 372k lines plus pf.
- **Problem.** These are the hottest, most actively patched parts of FreeBSD. A private
  rewrite turns each upstream advisory into a manual re-review against our version, and we
  lose Netflix's and syzbot's continuous testing. Our fork stops being a thin patch set.
  **Not recommended**, unless FreeBSD upstream wants the same rewrite and we do it with them
  (D7).

### 8.3 Incremental (recommended)

Keep FreeBSD as it is, upstream, and work in four directions at once, each useful on its
own:

1. **Shrink.** Ship an Atrium kernel configuration without the N subsystems. 42 of 111
   kernel advisories (38%) would not have applied. Cost: weeks. No Rust.
2. **Harden what we own.** Fuzz and audit Atrium's own kernel modules with the same AI
   methods that found FreeBSD's 2026 bugs, under KASAN and syzkaller.
3. **Narrow what apps can call.** A per-app syscall and ioctl profile, so the A rows shrink.
4. **Rust for new Atrium kernel code.** Start with Tessera's decoders, then the GPU driver's
   ioctl validation, and use (and contribute to) FreeBSD's Rust KPI bindings rather than our
   own.

This is the Linux, Windows and Android pattern, adjusted for a platform that does not own its
kernel: new code in Rust where we write it, hostile-input parsers out of the kernel or into
Rust, and the shared core left to upstream, which now has a funded AI audit on it.

## 9. Recommended order of work

| step | what | effort (one developer with AI help) | removes or reduces |
|---|---|---|---|
| **0. Shrink and harden the shipped kernel** | an `ATRIUM` kernel config without: `COMPAT_LINUX*`, `COMPAT_FREEBSD32`, SysV IPC (`allow.sysvipc=0` per jail is already the default; drop the options), SCTP, `tcp_rack`, ipfw/libalias/natd, netgraph Bluetooth, NFS client and server, kgssapi, ZFS (unless D-later), fuse, netmap, CAM target (ctl), cryptodev, Xen, vmm (until the micro-VM lane, linux-compat §1), unused NIC/HBA drivers. Build with `INIT_ALL=zero`. Sysctls: `security.bsd.unprivileged_proc_debug=0`, `kern.elf64.allow_wx=0`, `security.bsd.see_other_uids=0`, `security.bsd.see_jail_proc=0`, no KTLS receive (we use rustls). Confirm KASLR, PAC and BTI state on arm64 and amd64 builds | 1–2 weeks, plus a VM run of the existing e2e and jail checks | the 42 N rows (24 memory bugs); the 9-advisory uninit class; some exec/ptrace rows |
| **1. Audit our own kernel C** | KASAN and `INVARIANTS` kernels for every Atrium kmod test run; syzkaller descriptions for Tessera's syscalls and ioctls and the GPU ioctls (tessera-impl §6.7 already plans this); Tessera image fuzzing; an AI-assisted audit pass over `atrium-tessera/kmod`, `core`, `atrium-gpu-amd`, `atrium-zram`, `atrium-kmod`, written up as a review like 2026-11-03 | 3–5 weeks to set up, then continuous | the densest bug source we control |
| **2. App syscall profile** | a design note, then a per-jail syscall filter in the kernel (§9.1) that, per app jail, denies ioctls and socket options apps never need (tty control ioctls for non-terminal apps, IPv6 multicast filters, shm largepage configuration, routing-socket writes, ktrace, ptrace), plus capability mode for every app class that can live with it (renderers, decoders, Navigator workers already do) | 1–2 months | a share of the 26 A-row memory bugs; the exact share depends on the profile and is the first thing the note should measure against the data file |
| **3. Rust kmod spike** | build a `no_std` Rust kernel module with `bsd.kmod.mk` on arm64 and amd64: panic=abort, allocator over `malloc(9)`, `mtx`/`sx` guards, `copyin`/`copyout` and uio wrappers, a cdev. Start from the out-of-tree FreeBSD KPI bindings if their licence fits LICENSING-POLICY.md, otherwise our own. Exit gate: a Rust cdev passes a KASAN run and a syzkaller hour, and costs no measurable latency over C | 3–5 weeks | — (unblocks 4, 5) |
| **4. Tessera core in Rust** | tessera-core (16.5k lines of C) is freestanding and already linked into both the kernel module and userspace tools, with Rust FFI crates on top. Rewrite it as one `no_std` crate used by both, starting with every on-disk decoder (superblock, journal, pack, B-tree node, manifest), differential-tested against the C core on the existing corpus | 2–4 months | the in-kernel parser of untrusted disk images; the VFS glue stays C at first. **Phase 1 done 2026-11-10**: every on-disk decoder (superblock, journal, pack, B-tree node, manifest, directory B-tree node) is a `no_std` Rust crate (`rs/tessera-decode`) used by `tessera_fs.ko` and the host core build, differential-tested against the C on every corpus and under libFuzzer ([rust-decoders.md](../../atrium-tessera/docs/rust-decoders.md)). Phase 2: the kmod's own inline walkers, the userspace tools, then the write paths |
| **5. GPU driver validation in Rust** | the part of `atrium-gpu-amd` that validates ioctl arguments, command-buffer packets and BO ranges, behind the same C entry points | 2–3 months | the broker-reachable kernel code that decides GPU isolation |
| **6. Move remaining radio and device parsing out of the kernel** | the same move as Tactus and Vinculum, next for Wi-Fi when laptops need it (802.11 frame parsing is radio-reachable: SA-22:07, SA-22:02, SA-23:11) | separate scoping, later | the 802.11 and USB rows |
| **7. Revisit** | after FreeBSD 16.0: upstream Rust KPI status, CHERI in base, and how the 2026 advisory wave settled | a day | — |

### 9.1 Per-jail syscall filter (step 2), first sketch

FreeBSD has no seccomp: a jail limits what a process can *name*, not what it can *call*.
The smallest kernel change that fixes that:

- **A syscall bitmap on the jail.** `struct prison` gets a bitmap with one bit per syscall
  number, set once at creation by jaild (a new jail parameter), inherited by child jails,
  and only ever narrowed. `syscallenter()` (`kern/subr_syscall.c`) tests the bit next to the
  existing Capsicum check, so the cost is one bit test per syscall. A denied call returns
  `ENOSYS` (optionally `SIGSYS`), and a log-only mode reports what an app would have hit, so
  profiles can be generated by running the app instead of by hand.
- **Argument filters only where the bugs are.** A small allowlist of ioctl commands and
  socket options per jail, checked in `kern_ioctl()` and `sosetopt()`. Most 2026
  use-after-frees sat behind an ioctl or a socket option on a descriptor the app legitimately
  holds (tty, sound, IPv6 multicast filters, shm largepage configuration, RACK options),
  which a syscall-number filter alone cannot see.
- **Profiles by app class, not per app**, in the manifest model Portcullis already has
  (portcullis.md): "renderer", "document worker", "networked app", "terminal". Capability
  mode stays on top where an app class allows it.
- **Prior art:** Linux seccomp-bpf (argument filtering via BPF programs; powerful, hard to
  write well), OpenBSD pledge (named promise sets; the model we should copy for usability),
  Capsicum (FreeBSD's own per-syscall flag in the syscall table).
- **Where it lives:** a patch in our FreeBSD fork, in C because it sits in
  `syscallenter()`, a few hundred lines. Worth offering upstream as a jail feature. Effort:
  2–4 weeks for the mechanism and VM tests, then a few weeks of profiles.
- **Performance is a hard requirement** (Shankar 2026-11-07: jails are prized for running at
  near-native speed, and this must not change that). So: no interpreter (no BPF) on the
  syscall path; a jail with no filter takes the existing path unchanged (one predictable
  branch); a filtered syscall costs one bit test on data already in cache, and argument
  checks run only inside `ioctl`/`setsockopt`, never on other calls. Gate before merge:
  LMbench `lat_syscall` and a `getpid`/`read` loop within noise (target under 1%), and the
  existing Navigator, Fresco and Tessera benchmarks unchanged, on arm64 and amd64, filter
  off and on.
- **What it will not do:** calls every app needs (kqueue, poll/select, unix sockets, mmap)
  stay reachable, and so do their bugs. The design note should count, from the data file,
  how many A rows each candidate profile would have closed.

Steps 0, 1 and 2 need no Rust and no policy change and should come first; they also give the
numbers that tell us whether 4 and 5 are worth it. Steps 3–5 need D2.

## 10. Decisions for Shankar

| # | decision | recommendation |
|---|---|---|
| **D1** | Port the FreeBSD kernel to Rust (a full rewrite, §8.1, or a fork that rewrites core subsystems, §8.2)? | **Decided 2026-11-07: no.** Stay on upstream FreeBSD, with our fork kept a thin patch set. Take the incremental path (§8.3) |
| **D2** | Change LANGUAGE-POLICY.md from "Kernel = C" to "**new Atrium-authored kernel code is Rust by default** once the step-3 spike passes; C stays for VFS/newbus glue and for code we import"? | **Yes. Decided by Shankar 2026-11-10** after the step-3 spike ([results](../../kmod-rust-spike/README.md)); LANGUAGE-POLICY.md updated. It applies the reasoning we already use for userspace (untrusted-input parsing dominates) to our own kmods |
| **D3** | First Rust kernel target? | **Tessera's on-disk decoders** (step 4): freestanding, parser-heavy, already has Rust crates and a test harness. GPU validation (step 5) second |
| **D4** | Ship a stripped `ATRIUM` kernel config and the hardening sysctls in step 0? | **Yes, now.** Cheapest win in this review |
| **D5** | Build a per-app syscall/ioctl profile (step 2)? | **Yes** (Shankar 2026-11-07: worth implementing in the kernel). Mechanism sketched in §9.1; design note first, measured against the data file |
| **D6** | Which FreeBSD branch does a shipped Atrium follow? Development stays on 16-CURRENT; a release needs a branch with advisories | **Ship on a supported `stable`/`releng` branch with a fast update path** (pkgbase), move to 16 when 16.0 releases. Patch latency matters more than ever in 2026 |
| **D7** | Work with FreeBSD upstream on Rust KPI bindings instead of keeping private ones? | **Yes**, if their licence fits; report what we learn from the spike |
| **D8** | Run an AI-assisted audit of Atrium's own kernel modules now (step 1)? | **Yes**, before more kernel code lands |
| **D9** | Kernel modules cannot be jailed: a bug in any module is a bug in the whole kernel. Make it a rule that new code which parses hostile input (radio, devices, network formats, disk images from removable media) goes into a jailed user-space daemon first, and into the kernel only when performance requires it (as Tactus and Vinculum already do)? | **Yes. Agreed by Shankar 2026-11-07.** Tessera and the GPU fast paths stay in the kernel; for them the answer is Rust (D2) plus fuzzing (step 1) |

## 11. Caveats

- The classification is ours, done by reading each advisory's problem description; the CSV
  has one row per advisory so it can be checked or re-cut. "Safe Rust prevents" means the
  bug as described could not have been written in safe Rust, or (partial) that a standard
  Rust idiom (`#[must_use]`, RAII references, `Option`) would very likely have caught it.
  It does not count bugs a Rust rewrite would introduce.
- Third-party bundles (OpenSSL, unbound) count as one advisory each even when they fix
  several CVEs.
- Atrium exposure (A/S/N) is judged against today's design (vnet jails, pf, Tessera, no
  linuxulator, rustls). If we adopt ZFS, bhyve for micro-VMs or KTLS, rows move from N to S.
- KASLR, PAC and BTI status is stated with less confidence than the rest; step 0 checks it
  on our own builds.
- Web sources for 2026 events (AI credit trends, the Foundation project, Linux's Rust
  status, Android's figures, CheriBSD plans) are news and project posts, not peer-reviewed;
  the advisory counts and credits come straight from freebsd.org.

## Sources

- FreeBSD security advisories index and texts, https://www.freebsd.org/security/advisories/ (read 2026-11-07)
- FreeBSD 15.1-RELEASE source, https://download.freebsd.org/releases/arm64/15.1-RELEASE/src.txz
- FreeBSD Status Report Q4 2025 (Kernel Rust Support), https://www.freebsd.org/status/report-2025-10-2025-12/
- LWN, "FreeBSD considers Rust in the base system", https://lwn.net/Articles/985210/
- HardenedBSD, Optional Rust-in-FreeBSD support, May 2025, https://hardenedbsd.org/article/shawn-webb/2025-05-20/optional-rust-freebsd-support-may-2025-status-report
- FOSS Force, FreeBSD launches AI-assisted project to find and fix vulnerabilities (June 2026), https://fossforce.com/2026/06/freebsd-launches-ai-assisted-project-to-find-and-fix-vulnerabilities/
- Calif.io, "MAD Bugs: Claude wrote a full FreeBSD remote kernel RCE" (CVE-2026-4747), https://blog.calif.io/p/mad-bugs-claude-wrote-a-full-freebsd ; "An AI audit of FreeBSD", https://calif.io/research/freebsd-audit
- Praetorian, FreeBSD kernel zero-days found by an AI agent, https://www.praetorian.com/blog/ai-vulnerability-research-freebsd-kernel/
- AISLE, FreeBSD zero-days, https://aisle.com/blog/aisle-matches-anthropic-mythos-on-freebsd-zero-days
- TaoSecurity, FreeBSD released the most security advisories in project history in June 2026, https://taosecurity.blogspot.com/2026/07/freebsd-released-most-security.html
- Miguel Ojeda, "rust: conclude the Rust experiment", https://lkml.iu.edu/2601.2/00914.html ; devclass, Rust permanent in the Linux kernel, https://devclass.com/2025/12/15/rust-boosted-by-permanent-adoption-for-linux-kernel-code/
- Google, "Rust in Android: move fast and fix things" (Nov 2025), https://blog.google/security/rust-in-android-move-fast-fix-things/
- Asterinas, USENIX ATC 2025, https://www.usenix.org/conference/atc25/presentation/peng-yuke
- CheriBSD 26.07 release notes, https://www.cheribsd.org/release-notes/26.07/index.html ; CHERI Alliance, project to merge CHERI into FreeBSD, https://cheri-alliance.org/new-innovateuk-funded-project-to-merge-cheri-support-into-freebsd/
- kasan(9), https://man.freebsd.org/cgi/man.cgi?query=kasan&sektion=9
