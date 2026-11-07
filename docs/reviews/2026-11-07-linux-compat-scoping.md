# Scoping: our own secure Linux compatibility layer

**Status:** scoping study, 2026-11-07. Nothing built. Decisions in §9 are open.
**Question (Shankar):** FreeBSD's Linux emulation (the linuxulator) has a long CVE history,
which is why the Widevine CDM was headed for a bhyve micro-VM. Should Atrium instead build
its own secure Linux layer, use it as a "Linux jail" for the CDM, and later for Linux apps?
**Related:** threat model §2.9 (CDM: bhyve preferred, linuxulator + Capsicum fallback),
legacy-web §8.1 (CDM island, user-installed, no network), limen.md (DRM paragraph),
atrium-player.md item 3 (Cisco's OpenH264 binary "via the CDM route"), sandbox.md
("What about Linux apps?"), ROADMAP.md (Linux app compat line), portcullis.md ("Not a VM").

## 1. Short answer

Yes to building something of our own, but **not a Linux syscall layer**, and **not one
thing for both jobs**.

- **The CDM is a shared library, not a program.** It needs a host process that loads it and
  answers about two hundred glibc calls. It does not need a Linux kernel ABI. A small
  **foreign-library host** (our own ELF loader plus a glibc-ABI shim, in Rust, running as a
  native FreeBSD process in capability mode inside a jail) runs it with **zero Linux code in
  the kernel**. That is smaller and safer than the linuxulator, a rewritten linuxulator or a
  micro-VM, and it works on the Mac dev VM where bhyve does not. About 2–3 months of work.
- **Linux apps are a different size of problem.** A layer that runs real Linux programs has
  to emulate most of Linux (≈300 syscalls, /proc, /sys, futex/epoll/inotify semantics,
  ptrace, namespaces), and the syscall layer is the *smaller* half next to X11/Wayland,
  D-Bus, PipeWire and GPU. gVisor, the best-funded attempt, took a Google team most of a
  decade. Everyone who ships Linux apps on another OS today (WSL2, ChromeOS Crostini,
  Apple's Containerization) runs a real Linux kernel in a lightweight VM. **If Atrium ever
  runs Linux apps, it should be the same: a micro-VM lane**, not a syscall layer.
- Sharing one layer would make the CDM carry the apps' attack surface for no gain.

## 2. What the Widevine CDM actually needs

Evidence, without disassembling anything:

- **Interface.** The CDM implements Chromium's public CDM interface
  (`content_decryption_module.h`, BSD-licensed): a few exported entry points
  (`InitializeCdmModule`, `CreateCdmInstance`, `GetCdmVersion`, `DeinitializeCdmModule`) and
  a host-callback object (`cdm::Host_10/11`: allocate buffer, set timer, wall time, session
  messages, promises, `FileIO` for per-origin storage, output-protection query). Everything
  it touches outside that goes through its imports.
- **Firefox's media-plugin (GMP) sandbox is a field-tested ceiling.** Firefox runs the
  Widevine CDM on Linux under a seccomp policy (`security/sandbox/linux/SandboxFilter.cpp`,
  `GMPSandboxPolicy` plus `SandboxPolicyCommon`). It allows only: memory (mmap, munmap,
  mprotect, mremap, madvise subset, brk, memfd/ftruncate for shared memory), threads (clone
  with thread flags only, futex, set_robust_list, rseq, sched_yield), time (clock_gettime and
  friends, nanosleep, times), getrandom, signals for its own threads, read/write/poll/epoll on
  descriptors it already holds, send/recv on the existing IPC socket, and **read-only opens of
  a short list of pre-opened files** (absolute path must match). No socket creation, no
  network, no writable opens. `uname` gets a faked answer, `readlink` returns EINVAL, and
  there is a comment that "the Widevine CDM may probe advice arguments, including invalid
  values" for madvise.
- **Files it reads** (from Firefox and Ubuntu bug reports): `/proc/cpuinfo` (bug: a short
  read crashed it), `/proc/<pid>/maps`, and its own library file. Reading its own image and
  maps is consistent with an integrity check; a host must answer these consistently.
- **Libraries.** It links against glibc (libc, libpthread, libdl, libm, librt on older
  glibc). Kodi users report `libnspr4.so` as a dependency on some builds; to be confirmed
  with `readelf -d` on the current blob.
- **Known loader quirks** (from the Raspberry Pi and Asahi installers): recent arm64 builds
  use `DT_RELR` relocations (glibc ≥ 2.36 needs a `GLIBC_ABI_DT_RELR` hack), need a glibc
  TLS-offset patch or they segfault, and reference libgcc's LSE atomic helpers
  (`__aarch64_ldadd4_acq_rel` …) that the installers stub. Asahi additionally pads the ELF
  for 16 KiB pages. **FreeBSD arm64 uses 4 KiB pages**, so we need no binary patching.
  Every one of these is a *loader* problem, which our own loader handles by design rather
  than by patching glibc or the blob.
- **Architectures.** Google publishes the CDM for x86_64 Linux through Chrome's component
  updater. arm64 Linux builds today come from ChromeOS images (what Asahi and Pi extract).
  Check whether Chrome for arm64 Linux, when it ships, serves an arm64 component.

**Conclusion:** the CDM's real needs are memory, threads, time, randomness, a handful of
synthetic read-only files, and a pipe to its host. That is a library-hosting problem.

## 3. Options

| | Option | Where the code runs | New kernel surface | Effort (1 engineer) | Reuse for Linux apps |
|---|---|---|---|---|---|
| A | Stock linuxulator + Capsicum (today's fallback) | kernel, ~25 k lines of `sys/compat/linux` C, 350+ syscalls reachable | all of it | ~1 month (port + jail glue); plus linux_base userland | yes, poor isolation |
| A′ | Linuxulator with a per-process Linux syscall allowlist (~60 calls, Firefox's list) | kernel | the ~60 handlers incl. futex, mmap, clone, epoll emulation | 1–2 months kernel work + upstreaming | partial |
| B | Our own in-kernel Linux layer (rewrite) | kernel, new C | as large as A eventually | years | yes |
| C | User-space Linux kernel à la gVisor | user-space supervisor; kernel hook to trap foreign syscalls | small hook | 2–4 years to useful desktop coverage | yes, that is its purpose |
| **D** | **Foreign-library host: own ELF loader + glibc-ABI shim** | user space, native FreeBSD process in capability mode | **none** (optional SIGSYS trap hook, §5.3) | **2–3 months** | no (libraries only) |
| E | bhyve micro-VM with a guest running the CDM | guest kernel + bhyve device models | vmm.ko, bhyve (Capsicum'd) | 1–2 months; needs hardware virtualisation | yes, the apps lane |

Notes per option:

- **A.** Precedent: FreeBSD's `foreign-cdm` port does exactly this for native Chromium
  (a `fcdm-worker` under the linuxulator, `fcdm-jail` wrapper). It works, but users hit
  "unsupported browser" on streaming sites and worked around it by faking a Windows user
  agent. Kernel surface is the whole linuxulator. Most recent advisory found:
  FreeBSD-SA-26:30.linux (June 2026, CVE-2026-49413, setuid + LD_PRELOAD); the futex code has
  its own history (EN-21:22, CVE-2018-6927). I could not reach freebsd.org from the cloud
  session to count the full list.
- **A′.** A real improvement and upstreamable, but the allowed set is exactly where the
  linuxulator bugs have been (futex, signals, mmap, epoll emulation). Still C in the kernel,
  still needs a Linux glibc userland installed.
- **B.** Rewriting the same ABI in the same language in the same place repeats the same
  class of bugs. Rust in the FreeBSD kernel is not available to us. Reject.
- **C.** The secure way to run *programs*. Costs: gVisor is hundreds of thousands of lines of
  Go plus a dedicated team, and still ships fixes for sandbox escapes; a per-syscall trap
  costs microseconds; FreeBSD has no seccomp-trap or syscall-user-dispatch, so we would add
  one. Overkill for the CDM; for apps, a VM is cheaper and better (§6).
- **D.** §5. The CDM never makes a Linux syscall: every libc call lands in our Rust shim,
  which implements it with native FreeBSD calls allowed in capability mode, or refuses it.
- **E.** Strong boundary, but: the guest needs a kernel. A Linux guest is GPL (fine if
  user-installed like the CDM, not in our image per LICENSING-POLICY); a FreeBSD guest just
  moves the linuxulator inside the VM; the cleanest guest is **D inside a tiny guest**.
  bhyve on arm64 needs bare-metal EL2; the Atrium dev VM on the Mac runs under QEMU/HVF
  without nested virtualisation, so E is not available there today. Cost per instance:
  tens of MB of RAM and a boot on first play.

## 4. Attack surface, compared

What a hostile license response or sample (or a compromised CDM) can reach:

| Option | Reaches |
|---|---|
| A | every linuxulator handler; FreeBSD kernel |
| A′ | ~60 linuxulator handlers; FreeBSD kernel |
| C | the trap hook; our supervisor (user space, jailed) |
| **D** | **the native syscalls capability mode allows** — the same surface as every other Atrium decoder jail (dav1d, image decoders); our shim (user space, same process) |
| E | bhyve's virtio device models (Capsicum'd process), vmm.ko |
| D in E | E, with D's surface inside the guest |

D's surface is not new: we already accept it for every hostile-input decoder. The shim
lives in the CDM's own process, so a CDM compromise owns the shim too; that is fine because
the shim holds nothing (no fds beyond the host pipe and its shared-memory buffers, no
network, no filesystem). The trust boundary is the jail plus capability mode, exactly as
for the media worker.

## 5. Option D in more detail

### 5.1 Pieces

1. **Loader** (Rust, in the host): parse ELF, map PT_LOAD segments, apply RELA/RELR
   relocations, run init arrays, support glibc symbol versioning, set up static TLS for the
   blob (TLSDESC / initial-exec, aarch64 variant I and x86_64 variant II). Threads are
   created by our pthread shim, so we reserve the blob's TLS block in each thread.
   *Measured precedents:* Android's libhybris and Wine's loader do the same job for other
   ABIs.
2. **glibc-ABI shim:** each imported symbol at its glibc version (`malloc@GLIBC_2.17`,
   `pthread_mutex_lock`, `clock_gettime`, `__errno_location`, `__cxa_*`, `dl_iterate_phdr`,
   the aarch64 LSE helpers, …), implemented on FreeBSD with **glibc struct layouts** where
   they differ (`pthread_mutex_t` size, `struct stat`, `sigaction`, clock ids, errno values).
   Expected 150–300 symbols; the exact list comes from `readelf --dyn-syms -V` (§8 step 0).
   Unknown symbols resolve to a trap that logs and fails the call, never to a FreeBSD symbol
   of the same name.
3. **Virtual filesystem:** `open` succeeds only for a fixed table of synthetic read-only
   files (`/proc/cpuinfo`, `/proc/self/maps` generated from our own mappings, the library
   image itself), as Firefox does with its pre-opened set. Persistent CDM storage goes only
   through the CDM interface's `FileIO`, which the host backs with a per-origin store
   (Tessera volume, scoped per origin, wipeable from settings).
4. **Host side:** implements `cdm::Host_10/11` in Rust and speaks the island channel
   (postcard, bounded frames, fuzzed) to the media worker: `LicenceRequest`/`LicenceResponse`
   relayed by the engine (limen.md), encrypted samples in, decrypted samples or frames out
   through shared memory.
5. **Jail profile:** no network, no filesystem, capability mode after load, rctl memory and
   CPU caps, one instance per media island.

### 5.2 Effort

| Piece | Estimate |
|---|---|
| Loader incl. TLS and RELR, tested with Chromium's open-source ClearKey CDM and Firefox's clearkey | 3–4 weeks |
| glibc shim for the Widevine import list | 3–5 weeks |
| Host interface + island channel + per-origin storage | 2–3 weeks |
| Jail profile, fuzzing the channel, CI that re-checks imports for each new CDM version | 1–2 weeks |
| **Total** | **~2–3 months** |

Ongoing: Widevine updates every few weeks; a new version occasionally adds an import. CI
runs the new blob's import list against the shim and fails loudly.

### 5.3 The one real risk: raw syscalls

If the obfuscated CDM issues `svc #0` / `syscall` directly instead of calling libc, those
calls reach FreeBSD's native syscall table with Linux numbers. In capability mode the
result is an error or a crash, not an escape, but the CDM would not work. Firefox's policy
traps by syscall number, not by caller, so it does not tell us whether such calls exist.

- **Find out first** (§8 step 0) by tracing the CDM on Linux and checking which syscalls
  come from code outside libc.
- **If they exist:** add a small kernel hook, a per-process flag that turns every syscall
  from a marked address range into a SIGSYS the shim handles (FreeBSD's equivalent of
  Linux's syscall user dispatch). That is a few hundred lines, far smaller than A′, and it is
  also the hook option C would need.

### 5.4 Second customer: Cisco's OpenH264

atrium-player.md item 3 leaves H.264 open between "Cisco's binary under Linux ABI
emulation" and no H.264, because Cisco's patent cover applies only to its own binaries and
Cisco builds none for FreeBSD. OpenH264's Linux `.so` is a far simpler library (libc,
libm, pthread). The same host runs it in a decoder jail. This gives D a concrete use that
does not depend on any Widevine agreement (§7), and it is the natural first target.

## 6. Linux apps, separately

If Atrium ever runs Linux apps, recommend a **micro-VM lane**, not a syscall layer:

- A user-installed Linux guest kernel and userland (GPL stays outside our image).
- Graphics through virtio-gpu **native context**, already designed in GPU ABI §6.3: the guest
  runs vendor userspace and only our ABI crosses, with per-guest quota and recovery.
- Windows via a Wayland bridge into Fresco (ChromeOS's sommelier model), files via
  virtio-fs onto Tessera volumes, audio into Lyra, input from Tactus.
- One VM per app or per trust group, like Apple's Containerization.

This keeps portcullis.md's "Atrium apps are not VMs" true: Linux apps are a separate lane
with weaker integration, which sandbox.md already says. The ROADMAP line "Linuxulator +
Fresco-X11/Wayland bridge" should change to this if accepted. On the Mac dev VM it needs
nested virtualisation, so it would be developed on the x86_64 box.

A gVisor-style layer (C) is worth revisiting only if micro-VM memory or start-up cost
proves unacceptable for the apps people actually want.

## 7. Licensing and terms

- **Redistribution:** the CDM may not be shipped; the user installs it (settled, §8.1).
- **Using it at all:** Google licenses Widevine to integrators by agreement (free, but an
  agreement). Electron needed castLabs' agreement plus **VMP signing**; Brave needed an
  agreement. Without one, Navigator is an unlicensed host: Google can revoke, and services
  allow-list clients (the `foreign-cdm` users were refused until they faked a Windows user
  agent). We do not impersonate browsers (legacy-web), so **DRM playback realistically
  requires approaching Google for a Widevine agreement**, whatever we build. Ask before
  building anything Widevine-specific.
- **Reverse engineering:** Chrome's and Widevine's terms forbid it, and we follow the same
  stance as for amdgpu firmware. Everything in §2 and §5 comes from the public CDM
  interface, the library's dynamic symbol table (what any dynamic linker reads), behaviour
  under tracing, and other projects' published notes. **No disassembly, no patching of the
  blob** (Asahi's 16 KiB fixup is unnecessary on 4 KiB FreeBSD).
- **L3 only.** Software CDM; services cap resolution (often 480p–720p). L1 needs a hardware
  TEE path and is out of scope.
- **Our code:** loader and shim are ours (BSD/MIT); the ClearKey test CDM is BSD.

## 8. If accepted: order of work

0. **Measure (2–3 days, on Linux: the R9700 box or a cloud container).** With a user-obtained
   CDM: `readelf -d --dyn-syms -V` for the import list; run it under a minimal host with
   seccomp in log mode and record syscalls by caller address (libc vs CDM text); note every
   file it opens. Same for OpenH264. Output: the exact shim list and the raw-syscall answer.
1. Loader + shim with ClearKey and OpenH264 (no Widevine needed).
2. Decoder-jail profile; H.264 behind the player's build feature.
3. Widevine host and island channel, only after a Widevine agreement (or a decision to
   proceed for personal use without one).
4. Optional: D inside a bhyve micro-VM on hardware that supports it.

## 9. Decisions for Shankar

1. **Split the two jobs.** CDM (and OpenH264) get a foreign-library host (D); Linux apps, if
   ever, get a micro-VM lane (§6). No general Linux syscall layer.
   *Recommend yes.*
2. **CDM default containment.** Make D in a capability-mode jail the default everywhere,
   with bhyve as optional extra hardening, replacing threat model §2.9's "bhyve preferred,
   linuxulator fallback". The linuxulator leaves the CDM path entirely.
   *Recommend yes:* D's surface equals our other decoder jails, it runs on the Mac, and E
   adds a guest kernel and VM plumbing for a marginal gain.
3. **Widevine agreement first.** Approach Google before any Widevine-specific work; never
   fake a user agent. *Recommend yes.*
4. **Start with OpenH264.** Build loader + shim when the player's H.264 question is taken
   up, with OpenH264 as the first customer and ClearKey as the test CDM.
   *Recommend yes;* until then this stays parked like the rest of DRM.
5. **Raw-syscall hook.** Add the SIGSYS trap hook only if step 0 shows the CDM makes raw
   syscalls. *Recommend yes (conditional).*
6. **Docs.** If 1–2 are accepted: amend threat model §2.9, limen.md's DRM paragraph,
   legacy-web §8.1, sandbox.md and the ROADMAP Linux-apps line.

## Sources

- Firefox GMP seccomp policy: `security/sandbox/linux/SandboxFilter.cpp` (mozilla/gecko-dev);
  Mozilla sandbox architecture wiki; bugs 1304220 (`times`), 1770905 (`/proc/<pid>/maps`);
  Launchpad 1945100 (`/proc/cpuinfo`).
- FreeBSD `foreign-cdm`: reviews.freebsd.org D42026; FreeBSD forums thread 92723.
- Linuxulator size: `sys/compat/linux` + `sys/{amd64,arm64}/linux` in freebsd-src main,
  2026-11-07 (≈56 k lines incl. generated/headers, ≈25 k lines in `sys/compat/linux/*.c`;
  amd64 `syscalls.master` has 366 entries).
- Advisories: FreeBSD-SA-26:30.linux (CVE-2026-49413); FreeBSD-EN-21:22.linux_futex.
- Widevine on arm64 Linux: xesco/pivine; xbmc/inputstream.adaptive#1128; "The Quest for
  Netflix on Asahi Linux"; savage69kr widevine fixup gist.
- Widevine licensing: Electron "Testing Widevine CDM" docs; Wikipedia "Widevine".
