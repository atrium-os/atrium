# Hospes: the foreign-library host

**Status:** proposed architecture, 2026-11-11. H0 measured 2026-11-11
(`docs/reviews/2026-11-11-hospes-h0.md`): the CDM makes raw syscalls, so K1 is required;
findings folded in below. §14 decisions accepted 2026-11-11.
**Follows:** `docs/reviews/2026-11-07-linux-compat-scoping.md` (accepted 2026-11-07: closed
Linux libraries run in our own loader and glibc shim in a capability-mode jail; no Linux ABI
in the kernel; Linux apps, if ever, get micro-VMs).
**Customers:** Cisco's OpenH264 binary (atrium-player.md §9.3), then the Widevine CDM
(legacy-web §8.1, limen.md "DRM", threat model §2.9).
**Name:** *Hospes*, Latin for both host and guest: the one who takes in a stranger under
house rules. Proposed for NAMING.md (§14 decision 1).

## 1. What Hospes is, and what it is not

Hospes runs **one closed Linux shared library** (`.so`, built against glibc) inside **one
native FreeBSD process** that it fully controls, and exposes that library's API to the rest
of Atrium over a typed channel.

- **It is** an ELF loader, a glibc-ABI shim, a small virtual filesystem and a host for one
  library interface (the Chromium CDM interface, or OpenH264's decoder interface). All in
  Rust, all in user space.
- **It is not** a Linux syscall layer. The library never makes a Linux syscall: every call
  it makes lands in our shim, which either implements it with native FreeBSD calls allowed in
  capability mode or refuses it.
- **It is not** a way to run Linux programs. No `execve` of Linux binaries, no `/proc`
  beyond a few synthetic files, no fork. Linux apps are the micro-VM lane's job.
- **It never modifies the library.** No patching, no disassembly (§12). The bytes the user
  installed are the bytes we map.

## 2. Principles

1. **Capability mode before parsing.** A Hospes process is exec'd with its descriptors
   already open, calls `cap_enter` as its first act, and only then reads the ELF. Even a
   hostile ELF meets an empty world.
2. **No FreeBSD libc in the process.** The runtime is `#![no_std]` Rust making raw FreeBSD
   system calls (§4). The foreign library and our runtime are the only code present, so no
   two thread-local-storage conventions ever fight over the thread pointer.
3. **Strict imports.** Every symbol the library imports must be in the shim's table at the
   version it asks for. An unknown import refuses the load (§6.4). Nothing falls through to
   "a FreeBSD function of the same name".
4. **Never trust the library's output.** The processes on the other side of the channel
   check every size, index and count, as the player already does for its decode worker.
5. **Honest to the library.** Where the CDM asks about the platform (output protection,
   platform challenge), we answer truthfully, even when the truth costs resolution. We fake
   only what ABI compatibility needs (a fixed `uname` saying Linux, as Firefox does).
6. **The host is separate from the loader.** The CDM host logic (`cdm::Host`, sessions,
   storage) does not depend on the loader or shim. If Google ever ships a native FreeBSD or
   Atrium CDM, it slots into the same host with the shim removed (decided 2026-11-07).

## 3. Process model

```
 player jail (one per embed, from Limen)                      CDM jail (one-shot, per island)
 ┌───────────────────────────────────────────────────┐       ┌─────────────────────────────┐
 │ atrium-player (supervisor)                         │ ctrl  │ hospes-cdm                  │
 │   Limen channel to the engine; licence messages ◄─┼──────►│   cap mode, no_std runtime  │
 │                                                    │       │   libwidevinecdm.so mapped  │
 │ atrium-player-decode (worker, cap mode)            │ data  │   per-origin store (dir fd) │
 │   demux incl. CENC info; dav1d, libvpx, Opus  ◄────┼──────►│                             │
 │        │ samples                                   │ +shm  └─────────────────────────────┘
 │        ▼                                           │
 │ hospes-codec (cap mode, no_std runtime)            │
 │   Cisco libopenh264.so mapped; writes frames       │
 │   into the worker's shared frame slots             │
 └───────────────────────────────────────────────────┘
```

- **hospes-codec** runs **inside the player's jail** as a sibling of the decode worker. It
  parses the same hostile bitstreams dav1d does, so it gets the same trust: capability mode,
  an address-space limit, a per-call watchdog held by the worker, killed and restarted from
  the next keyframe on a crash. The supervisor spawns it with: a socket to the worker, the
  blob as a read-only fd, and the shared frame slots.
- **hospes-cdm** runs in **its own one-shot jail** (`portcullis-oneshot`, via portcullisd
  `ExecInstance`, as the Fresco renderers and Navigator workers do), as legacy-web §8.1
  settled: it is the one component we can neither read nor build, and it holds per-origin
  state that nothing else should see. One instance per media island; it dies with the island.
  The jail adds rctl limits and kill-on-teardown on top of capability mode.
  *Built 2026-11-12* (`docs/reviews/2026-11-12-hospes-cdm-jail.md`): the app asks portcullisd
  with `OpenCdm { key_system, origin }`, which needs `protected-media` in its installed
  manifest; the host is `org.atrium.hospes-cdm`, run as `_hospes-cdm` (65027) by a system
  worker rule, and receives its channels, library and store on stdin (`SCM_RIGHTS`).
- **Two channels to the CDM.** The *control* channel goes to the supervisor: sessions,
  licence messages, key status. The supervisor relays licence messages to and from the engine
  over Limen and never looks inside them. The *data* channel goes to the decode worker:
  encrypted samples in, clear samples (or frames) out, through shared slots.
- **No network anywhere in Hospes.** Licence and provisioning requests leave as messages;
  the engine sends them through its Nuntius fetcher under the page's rules.

## 4. The runtime (`hospes-rt`)

A static, `#![no_std]`, `#![no_main]` Rust binary per host, with `alloc`.

- **Entry.** Our own `_start`. It reads its descriptor numbers from the auxiliary vector and
  the fixed fd layout, sets resource limits it was told to set, calls `cap_enter`, then
  continues.
- **System calls.** A thin raw-syscall module per architecture (arm64 `svc #0`, x86_64
  `syscall`) for the short list capability mode allows and we need: `mmap`/`munmap`/
  `mprotect`/`madvise`, `thr_new`/`thr_exit`/`thr_self`/`thr_kill`, `_umtx_op`,
  `clock_gettime`/`nanosleep`, `getrandom`, `read`/`write`/`recvmsg`/`sendmsg`/`close`,
  `kevent`/`kqueue`, `openat` (only beneath the CDM's store fd), `fstat`, `ftruncate`,
  `cap_rights_limit`, `exit`. FreeBSD's kernel ABI is stable in practice, and Atrium ships its
  own kernel, so this is a contract we control.
- **Allocator.** Our own, over `mmap`: size classes for small blocks, direct mappings for
  large ones, guard pages optional (on in tests). It serves both our code and the library's
  `malloc` family, so one heap limit covers the whole process.
- **Threads.** Created with `thr_new`. Each gets a stack with a guard page and a
  **glibc-shaped thread control block** (§5.3) at the thread pointer. Our own per-thread
  state sits in a reserved area of that block, at offsets no glibc-compiled code reads.
- **Event loop.** One host thread runs a `kevent` loop: channel messages, timers the
  library set, and its own calls into the library. Chromium calls the CDM from one thread
  and so do we; the library's own worker threads are its business.
- **Crashes.** A fault in the library is not caught. The process dies, the peer sees the
  channel close and treats it as `Error { DECODE }` or `Error { CDM }`.

Why no libc: FreeBSD's libc and glibc-compiled code both own the thread pointer, with
different layouts (on x86_64, glibc code reads its stack-protector canary at `%fs:0x28`,
which in FreeBSD's layout is something else). Swapping the pointer on every call across the
boundary, as libhybris and Wine have had to, is a long tail of subtle bugs. Without libc
there is nothing to swap.

## 5. The loader (`hospes-elf`, `hospes-rt::load`)

### 5.1 Parsing

`hospes-elf` is a safe-Rust (`#![forbid(unsafe_code)]`), `no_std` ELF64 parser with
bounds-checked reads and a fuzz target. It accepts only:

- `ET_DYN`, matching machine (`EM_AARCH64` or `EM_X86_64`), little-endian, ELF64;
- `PT_LOAD` segments whose `p_align` is a multiple of the page size (Widevine and OpenH264
  on arm64 use 64 KiB), with offsets congruent to addresses and no overlap; `PT_DYNAMIC`, `PT_TLS`,
  `PT_GNU_RELRO`, `PT_GNU_STACK` (must be non-executable), `PT_GNU_EH_FRAME`, `PT_NOTE`;
- no `DT_TEXTREL`, no `PT_INTERP` (we do not run programs).

### 5.2 Mapping and relocation

- Reserve the whole image span, then map each `PT_LOAD` from the blob fd at its offset
  (`MAP_FIXED` inside our reservation), zero-fill `.bss` (Widevine's is about 59 MiB).
- **Symbol lookup order:** the library's own exports first, then the shim, as glibc's global
  scope would give (OpenH264 references its own symbols through the GOT). Weak undefined
  symbols with no definition (`OPENSSL_memory_*`, `__gmon_start__`, `_ITM_*`, `__gcov_*`)
  stay zero.
- **Eager binding only.** Every relocation is applied at load, `DT_BIND_NOW` behaviour,
  then `PT_GNU_RELRO` is made read-only. No lazy PLT, so the GOT is never writable after load.
- **Relocations supported:** `RELATIVE`, `GLOB_DAT`, `JUMP_SLOT`, `ABS64`/`64`,
  `DTPMOD64`, `DTPOFF64`/`DTPREL`, `TPOFF64`/`TPREL`, `TLSDESC`, `IRELATIVE`, and `DT_RELR`
  packed relocations. Anything else refuses the load.
- **Symbol versioning.** `DT_VERNEED`/`DT_VERSYM` are honoured: an import of
  `memcpy@GLIBC_2.14` binds to the shim's `memcpy` entry for that version, and only to it.
- **`IRELATIVE` / ifunc** resolvers run with Linux-shaped `AT_HWCAP`/`AT_HWCAP2` values
  computed from the real CPU, so the library picks the same code paths it would on Linux.
- **Init and fini.** `DT_INIT`, `DT_INIT_ARRAY` run on the host thread after binding;
  `DT_FINI_ARRAY` and `__cxa_atexit` handlers at orderly shutdown only.
- **W^X.** Segments are mapped with their declared permissions, never write and execute at
  once. If the library later asks `mprotect` for write+execute, the shim refuses; a
  write-then-execute transition is allowed but logged. (§13 R3 covers obfuscated code that
  needs more.)

### 5.3 Thread-local storage

- One static TLS block for the library, sized from `PT_TLS`, allocated in every thread's
  control block at a fixed offset computed at load. The library's initial-exec and TLSDESC
  accesses resolve to that offset; general-dynamic `__tls_get_addr` calls go to the shim,
  which knows the one module.
- **arm64 (TLS variant I):** the thread pointer (`TPIDR_EL0`) points at a 16-byte header
  (`dtv`, reserved), followed by the library's block.
- **x86_64 (TLS variant II):** `%fs` points at a glibc `tcbhead_t`: self-pointer at 0,
  `dtv` at 8, stack-protector canary at `0x28`, pointer guard at `0x30`; the library's block
  sits below it. The canary is a fresh random value per process.
- Our own per-thread state (errno, shim bookkeeping) lives in the header's reserved tail.

### 5.4 What the library sees of itself

`dl_iterate_phdr`, `dladdr` and `/proc/self/maps` (§7) all report the same image at a fixed
virtual path (`/opt/hospes/lib/<name>`), with real addresses and permissions. The library's
own file, if it reads it, is served from the blob fd. Consistency matters because integrity
checks compare these.

## 6. The glibc-ABI shim (`hospes-glibc`)

### 6.1 Shape

A table per architecture of `(name, version) → implementation`, generated from one Rust
source of truth. Implementations are `extern "C"` Rust functions in `hospes-glibc`, using
**glibc's struct layouts and constants**, not FreeBSD's. The layouts live in `glibc-abi`
modules per architecture and are checked against real glibc headers by a Linux CI job (§11.2).

### 6.2 What it implements

From the CDM's and OpenH264's needs (scoping §2; exact list from H0, §10):

| Area | Covers | Notes |
|---|---|---|
| Memory | `malloc`, `calloc`, `realloc`, `free`, `posix_memalign`, `aligned_alloc`, `malloc_usable_size` | our allocator, one heap limit |
| Strings, memory ops | `mem*`, `str*`, `wcs*` subset | optimised routines for `memcpy`/`memset` on decrypt buffers |
| Threads | `pthread_create/join/detach/self/equal`, mutex, rwlock, cond (incl. `pthread_cond_clockwait`), once, keys, attrs | glibc sizes (`pthread_mutex_t` is 40 or 48 bytes), on `_umtx_op` |
| Time | `clock_gettime`, `gettimeofday`, `time`, `nanosleep`, `clock_nanosleep`, `times` | Linux clock ids translated |
| Randomness | `getrandom`, `getentropy`, `arc4random*` if imported | kernel `getrandom` |
| Errors | `__errno_location`, `strerror` | per-thread errno in the control block; Linux errno numbers |
| Files | `open`/`openat`/`fopen`, `read`, `fread`, `fgets`, `close`, `fstat`, `stat`, `lseek`, `readlink` | the virtual filesystem only (§7) |
| Process | `getpid`, `gettid`, `getuid` family, `sysconf`, `getauxval`, `uname`, `prctl` (name only), `sched_yield`, `sched_getaffinity` | fixed, stable answers |
| Signals | `sigaction`, `sigprocmask`, `sigemptyset` family, `raise` | recorded, not delivered, except `abort` |
| Dynamic linking | `dlopen` (only names we provide), `dlsym`, `dlerror`, `dladdr`, `dl_iterate_phdr`, `__tls_get_addr` | |
| C++ runtime glue | `__cxa_atexit`, `__cxa_finalize`, `__cxa_thread_atexit_impl`, `__stack_chk_fail`, `__libc_single_threaded` | |
| Unwinding | `_Unwind_*` (libgcc_s): both libraries import it | LLVM libunwind built freestanding (Apache-2.0 with LLVM exception), reading the library's `PT_GNU_EH_FRAME` |
| C++ runtime (OpenH264) | `operator new/delete`, `__cxa_guard_*`, `__cxa_pure_virtual`, `__cxa_throw_bad_array_new_length`, `__gxx_personality_v0`, the `__class_type_info` family of vtables as data objects | libc++abi pieces, built freestanding (same licence) |
| Old and locale ABI (CDM) | `__xstat64`, `__fxstat64`, `newlocale`/`uselocale`/`freelocale`, `strtod_l` family, `strftime_l`, `catopen`/`catgets`/`catclose`, `__ctype_*_loc`, `mbrtowc` family | C locale only; message catalogues always fail |
| Semaphores, fork hooks | `sem_*`, `__register_atfork`, `setpriority`, `sched_setscheduler` | `sem_*` on `_umtx_op`; the rest accepted and ignored |
| Math | `libm` subset | Rust `libm` (MIT/Apache) |
| Atomics helpers | `__aarch64_ldadd4_acq_rel` and the other LSE outline helpers | trivial, implemented directly |
| Logging | `fprintf(stderr)`, `vsnprintf`, `syslog` | formatted into our log channel, rate-limited |
| Indirect syscalls | `syscall(long nr, ...)` | a Linux-number dispatcher; H0 saw `gettid` and `getrandom` from the CDM; everything else `ENOSYS` |

### 6.3 Behaviour rules

- **Refuse, don't pretend.** A call outside what we allow returns the Linux error a sandbox
  would give (`EPERM`, `ENOSYS`, `ENOENT`), as Firefox's GMP policy does. One exception,
  learned from Firefox: the CDM probes `madvise` with odd arguments and must get an error,
  never a crash.
- **No real filesystem, no sockets, no processes.** `socket`, `connect`, `fork`, `execve`,
  `system`, `popen` are not in the table, so a library importing them refuses to load.
- **Locale is "C".** `setlocale` succeeds only for `C`/`POSIX`.

### 6.4 Unknown imports

At load, the loader lists every import not in the table. In production that **refuses the
load** and reports the names. A development flag binds unknowns to a stub that logs the name
and aborts on first call, which is how H0 and new-version bring-up find gaps. CI checks each
new CDM or OpenH264 version's import list against the table (§11.4).

## 7. The virtual filesystem

A fixed table. Nothing else exists.

| Path | Content |
|---|---|
| `/proc/cpuinfo` | generated in Linux's format from the real CPU (features, model, cores) |
| `/proc/self/maps`, `/proc/<pid>/maps` | generated from our mapping table, Linux format, real addresses and permissions |
| `/proc/self/auxv` | the Linux-shaped auxiliary vector we computed |
| `/proc/self/status` | minimal, consistent fields (Name, Pid, TracerPid 0) |
| `/proc/net/unix` | the header line only (the CDM reads it five times in H0) |
| `/dev/urandom` | reads served from `getrandom` |
| `/sys/devices/system/cpu/online`, `possible` | from the real CPU count |
| `/opt/hospes/lib/<name>` | the library's own bytes, from the blob fd |
| `/etc/localtime` | UTC |

H0 saw the CDM open `/proc/self/maps` nine times and `/proc/net/unix` five times with raw
`openat`, `/dev/urandom` through `open64`, and `stat` two Chrome cgroup paths (which return
`ENOENT`, as on a machine without them). Any other path returns `ENOENT`. The CDM's persistent data never goes through this table; it uses the
CDM interface's `FileIO` (§8.4).

## 8. The CDM host (`hospes-cdm`)

### 8.1 Interface

The Chromium CDM interface (`content_decryption_module.h`, BSD-licensed, vendored at the
version the installed CDM declares). Hospes supports the interface versions the current
Widevine build exports (10 and 11 today; H0 confirms) and refuses others.

- The library exports `InitializeCdmModule_4`, `CreateCdmInstance`, `GetCdmVersion`,
  `DeinitializeCdmModule`. We call them through the loader's symbol lookup.
- `cdm::ContentDecryptionModule_N` and the objects it returns (`Buffer`, `DecryptedBlock`,
  `VideoFrame`, `AudioFrames`, `FileIO`) are C++ objects. We call their virtual methods
  through the Itanium C++ ABI vtable (function pointers taking `this` first), from a small
  `unsafe` module checked against the real header by the test CDM (§11.3).
- Our `cdm::Host_N` is a vtable we build in Rust; its methods are `extern "C"` functions
  taking `this`.

### 8.2 Host callbacks

| Callback | Hospes |
|---|---|
| `Allocate(size)` | a slot in the shared sample/frame area, so decrypted output needs no copy |
| `SetTimer`, `GetCurrentWallTime` | event-loop timer; real wall time |
| `OnInitialized`, promise callbacks | control channel results |
| `OnSessionMessage` (licence request, renewal, release, individualisation) | control channel `SessionMessage`, relayed by the supervisor to the engine |
| `OnSessionKeysChange`, `OnExpirationChange`, `OnSessionClosed` | control channel events |
| `SendPlatformChallenge` | answered *not supported* (we have no platform attestation) |
| `EnableOutputProtection`, `QueryOutputProtectionStatus` | the truth: link types present, no HDCP protection (until Fresco's display path has one). Services may cap resolution; that is the stated cost |
| `CreateFileIO` | §8.4 |
| `RequestStorageId` | a random per-origin id, kept in the origin's store, wiped with the site's data (§14 decision 6) |
| `ReportMetrics` | dropped |

### 8.3 Decrypt paths

The CDM advertises what it supports; Hospes uses the first that works:

1. **Decrypt only.** Worker sends an encrypted sample with its subsample map, key id, IV and
   scheme (`cenc` or `cbcs` with pattern). The CDM returns clear compressed bytes in a slot;
   the worker decodes them with its own dav1d/libvpx/OpenH264 path. This keeps decoding in
   decoders we already harden, and is what we prefer.
2. **Decrypt and decode.** If the CDM only offers this for a codec, it decodes and returns
   I420 frames in slots; the worker converts them into the Fresco video layer as it already
   does for dav1d output.

**H0b (2026-11-11) settles the order: video uses decrypt-and-decode.** Widevine refuses part
of every decrypt-only video stream (VP9 keyframes; 28% of H.264 non-IDR samples), so a
decrypt-only stream cannot play. Its decrypt-and-decode path returned every frame for H.264
and VP9, bit-identical to ffmpeg where comparable. Hospes must hand it H.264 in **Annex-B**
with SPS/PPS before each IDR (as Chrome's MP4 parser does), adjusting the first subsample's
clear bytes. Decrypt-only remains the path for audio. Decision 7 is revised accordingly.

Either way the worker checks every returned size against the slot and the declared frame
size before copying anything. At Widevine L3 nothing here is a protected path; the frames are
ordinary video (legacy-web §8.1).

### 8.4 Per-origin storage

- At jail creation the supervisor's broker opens (or creates) the origin's CDM store, a
  directory on a Tessera volume keyed by the top-level origin and the key system, and passes
  it as a directory fd limited with `cap_rights_limit` to create, read, write, unlink.
  *Built:* the broker is portcullisd. The store is
  `/var/db/atrium/cdm-store/<app>/<key system>/<origin, percent-encoded>/` (0700, the host's
  user), the library `/var/db/atrium/cdm/<key system>/current/`; both are rights-limited by
  portcullisd and again by hospes-cdm before it loads anything. The app names only the origin
  and never reaches another app's stores.
- `FileIO` maps record names to files under that fd with `openat` beneath it; names are
  checked (`[A-Za-z0-9._-]{1,64}`), total size is capped (1 MiB default), record count capped.
- The store is wiped with the site's data, and listed in settings as "protected content
  licences for <origin>".

### 8.5 Provisioning

A fresh Widevine install must be individualised. The CDM produces the request as a session
message (individualisation type); the supervisor hands it to the engine like a licence
request, and the engine sends it through Nuntius to the provisioning URL the CDM names, with
no cookies. The response comes back the same way. Hospes never reaches the network. The
exact flow (which URL, whether the page or the host sends it) is confirmed in H0 and H4.

## 9. The codec host (`hospes-codec`)

For Cisco's OpenH264 decoder. Its API is `WelsCreateDecoder` / `WelsDestroyDecoder` plus the
`ISVCDecoder` C++ interface (vtable), `DecodeFrameNoDelay`, and option setters.

- Worker sends access units; Hospes returns decoded I420 planes in shared slots with width,
  height, strides and PTS. The worker converts into the layer as for dav1d.
- Limits set before decode: maximum picture size (7680×4320, as the layer), **one decoder
  thread** (H0: its threaded mode changes output), and the worker's per-call watchdog.
- Coverage: Baseline and B-frame-free High only (H0, R7). Streams it cannot decode correctly
  are refused by profile/B-frame check, not played wrong.
- Lives in the player jail, spawned by the supervisor, killed with it.

## 10. Measuring first: H0

Before the shim is written, measure on Linux what each library really uses. A Linux-only
harness, `hospes-probe`, run on x86_64 (cloud container or the R9700 box) and on arm64 (a
Linux VM on the Mac):

1. `readelf -d --dyn-syms -V` (or our own parser): `NEEDED`, every import with its version,
   TLS model, relocation types, interface versions exported.
2. Load the library under real glibc with an `LD_AUDIT` module recording **which imports are
   actually called**, and how often (the shim can start with what is used).
3. Run under seccomp in log mode plus a ptrace tracer recording, for each system call, the
   **caller address**. Calls from inside glibc are fine. Calls from inside the library's own
   text are **raw syscalls**, which the shim cannot see (§13 R1).
4. Record every file opened, every `mprotect` with execute, every signal handler installed.
5. Workload: for OpenH264, the H.264 conformance bitstreams; for Widevine, Shaka Player's
   public test content through our test CDM host on Linux.

Output: a report in `docs/reviews/`, the generated import table, the file list. About three
to four days. It needs the user's consent to download the two libraries (§14 decision 4).

## 11. Testing

1. **Loader fixtures.** Small `.so` files we build on Linux with glibc (x86_64 and aarch64
   cross), each exercising one feature: every relocation type, each TLS model, versioned
   symbols, ifunc, RELR, init arrays, C++ exceptions thrown and caught inside the library,
   threads with thread-locals. Committed as built binaries plus their source and build script.
2. **ABI layout check.** A Linux CI job compiles a C program against real glibc headers that
   prints `sizeof`/`offsetof` for every struct and constant in `glibc-abi`, and compares with
   the Rust definitions. Any drift fails the build.
3. **Test CDM.** A small C++ CDM built against the real `content_decryption_module.h`,
   implementing Clear Key (AES-CTR and CBCS). It is test-only C++ (LANGUAGE-POLICY's
   pragmatic test clause) because the point is that a real C++ compiler, not us, decides the
   vtable layout. It runs in both a Linux reference host and Hospes; outputs must match.
4. **Version watch.** A CI job takes a library version (from the user's install path on the
   test machine, never committed), runs the import check and the fixture workload, and fails
   on any unknown import or behaviour change.
5. **OpenH264 conformance.** Cisco's binary under Hospes decodes the conformance bitstreams;
   output MD5s must equal the same binary's output on Linux.
6. **Fuzzing.** `hospes-elf` (hostile ELF), both channel decoders, the virtual filesystem's
   path handling.
7. **Isolation probe.** As for the Fresco renderers: a probe library loaded by Hospes tries
   to open files, sockets, `/dev`, other processes, and to make raw syscalls; every attempt
   must fail.

## 12. Licensing and install

- **Never shipped.** Atrium ships Hospes, not the libraries.
- **OpenH264:** with the user's consent the system fetches Cisco's binary for the CPU from
  Cisco's own server (`ciscobinary.openh264.org`) through Nuntius, checks it against the hash
  for that version listed in Atrium's release, stores it in Tessera, and never modifies it.
  This is the arrangement under which Cisco's patent licence covers the user (Firefox does the
  same). The user can turn it off.
- **Widevine:** user-initiated. Two routes (§14 decision 5): the user supplies a
  `libwidevinecdm.so`, or Atrium downloads Google's official Chrome for Linux package from
  Google's public download site, verifies Google's package signature, and extracts only the
  CDM and its manifest. Atrium never presents itself to Google's component updater as Chrome.
- **No reverse engineering.** Everything Hospes needs comes from the public CDM header, the
  dynamic symbol table, traced behaviour on Linux and other projects' published notes.
- **Build first, licence after** (decided 2026-11-07): demonstrate with the test CDM, Clear
  Key and public Widevine test content; no faked user agent, no attempt at a commercial
  service's allow-list; then approach Google, asking for both a licence and a native build.

## 13. Risks

- **R1. Raw syscalls in the CDM — confirmed by H0.** The CDM's own code issues `openat`,
  `read`, `close` and `rt_sigaction` (queries for `SIGILL`, `SIGTRAP`, `SIGABRT`, `SIGSYS`).
  Unhandled, they hit FreeBSD's table with Linux numbers (Linux `read` is 0, FreeBSD's
  indirect `syscall`); in capability mode that is an error or a crash, not an escape, but the
  CDM fails. Remedy, **now required for Widevine (step K1, built 2026-11-12 with a module system call
  in place of the `procctl`: `docs/reviews/2026-11-12-hospes-k1.md`)**: a small kernel hook, a `procctl` that makes any
  syscall instruction executed from registered address ranges deliver `SIGSYS` with the
  registers; the shim's handler emulates it from its Linux-number table. A few hundred lines,
  written in Rust per the 2026-11-10 kernel decision, upstreamable as FreeBSD's equivalent of
  Linux's syscall user dispatch. The `SIGSYS` handler answers `rt_sigaction` queries truthfully
  (our trap handler is visible, as Firefox's seccomp handler is, and Widevine runs there).
- **R2. Anti-tamper checks.** The CDM may check `/proc/self/maps`, its own file, debugger
  presence, timing. Hospes answers consistently and has no debugger attached. If a check
  still fails it shows in H0 as a difference between Linux and Hospes, and we do not work
  around it by patching the library.
- **R3. Self-modifying code.** If the obfuscated CDM writes code at run time, it needs
  write-then-execute. We allow the transition with logging, never simultaneous write+execute.
- **R4. Unwinder.** Confirmed: both libraries import libgcc_s's unwinder, and OpenH264
  libstdc++'s personality routine. Hospes carries freestanding LLVM libunwind and libc++abi
  pieces.
- **R5. Revocation and service refusal.** Outside our control until a licence; stated in the
  scoping doc.
- **R6. arm64 availability — resolved.** Google's Chrome for arm64 Linux ships the arm64 CDM
  (same version, 4.10.3112.0). Both architectures can be developed; arm64 dynamic behaviour
  is still to be measured on a Linux VM on the Mac.
- **R7. OpenH264's coverage.** H0: Cisco's decoder is bit-exact on Baseline and on High
  without B-frames, wrong on Main/High with B-frames (most web H.264), and its threaded mode
  changes output. Hospes runs it single-threaded; its product value is WebRTC and Baseline
  content (player §9.3 to weigh this).

## 14. Decisions

**All accepted by Shankar 2026-11-11, as recommended.**

1. **The name Hospes** for the component, with crates `hospes-elf`, `hospes-rt`,
   `hospes-glibc`, `hospes-proto`, `hospes-cdm`, `hospes-codec`, `hospes-probe` in a new
   `hospes/` workspace. *Recommend yes.*
2. **A `no_std`, libc-free runtime** (§4), instead of a normal Rust process that swaps the
   thread pointer at every call into the library. *Recommend yes:* it removes the hardest bug
   class and shrinks the process to our code plus the library.
3. **Placement:** hospes-codec as a sibling process in the player's jail; hospes-cdm in its
   own one-shot jail per island, with control and data channels (§3). *Recommend yes.*
4. ~~**H0 now:**~~ **Done 2026-11-11** (Shankar approved; cloud container). Was: download Cisco's OpenH264 binary and Google's Chrome for Linux package onto a
   Linux machine for measurement (needs `ciscobinary.openh264.org` and `dl.google.com` allowed
   in the cloud environment, or the R9700 box). *Recommend yes,* cloud container for x86_64,
   a Linux VM on the Mac for arm64.
5. **Widevine install route:** offer both a user-supplied file and a download of Google's
   official Linux Chrome package with signature check and CDM-only extraction. *Recommend
   both.*
6. **Storage id:** a random per-origin id, wiped with site data, rather than none (which
   some services reject) or one per device (a tracking identifier). *Recommend per-origin.*
7. ~~**Decrypt-only preferred**~~ **Revised by H0b:** video must use decrypt-and-decode (the
   CDM refuses part of every decrypt-only video stream); decrypt-only for audio (§8.3).

## 15. Build order

| Step | What | Done when | Estimate |
|---|---|---|---|
| H0 ✔ | Measure both libraries on Linux (§10) | done 2026-11-11: `docs/reviews/2026-11-11-hospes-h0.md`, data in `hospes/probe/h0-data/` | — |
| H1 ✔ | `hospes-elf`, `hospes-rt`: parse, map, relocate, TLS, threads, allocator; loader fixtures pass on FreeBSD arm64 and amd64 | §11.1 and §11.2 green; isolation probe green | done 2026-11-11: `docs/reviews/2026-11-11-hospes-h1.md` (fixtures `-nostdlib` for now; glibc-built ones and the §11.2 layout check move to H2 with the shim's `glibc-abi` structs) |
| H2 ✔ | `hospes-glibc` for OpenH264's import set; `hospes-codec`; player integration behind the H.264 build feature | conformance MD5s match Linux (§11.5); player plays an H.264 MP4 in the VM | done 2026-11-11: `docs/reviews/2026-11-11-hospes-h2.md` (12/12 conformance streams bit-identical on Linux and FreeBSD amd64/arm64; no unwinder needed for OpenH264, it moves to H4 with the glibc-built fixtures) |
| H3 ✔ | `hospes-proto`, `hospes-cdm` with the test CDM; CENC/CBCS info from the demuxers; control and data channels; per-origin store | Clear Key content plays through Hospes; outputs equal the Linux reference host | done 2026-11-12: `docs/reviews/2026-11-12-hospes-h3.md` (transcript identical to the reference host on Linux and FreeBSD amd64/arm64; three Clear Key clips play with frames equal to their clear sources; the player uses decrypt-only until H4, and the CDM's jail, own user and rights-limited store move to H4) |
| H4 ✔ | Widevine: shim extended to its imports, provisioning flow, install route, public test content | Shaka's Widevine test stream plays in the amd64 VM | done 2026-11-12: `docs/reviews/2026-11-12-hospes-h4.md` (angel-one H.264 + Opus plays to the end in the FreeBSD amd64 VM, and later the arm64 VM, licence through Nuntius, CDM installed from Google's signed package; no provisioning request on Linux desktop; first run needed `futex` in the shim, the key system, clear-lead MP4 and key-id session handling) |
| K1 ✔ | The `SIGSYS` trap hook (`procctl`, Rust, §13 R1) and the shim's Linux-syscall handler for `openat`/`read`/`close`/`rt_sigaction` | the H0 CDM run's raw syscalls all handled under Hospes; isolation probe still green | done 2026-11-12: `docs/reviews/2026-11-12-hospes-k1.md` (core hook in the fork, policy in `hospes_trap.ko` (C glue + Rust table); a module system call instead of `procctl`; H0's raw call set trapped and answered on FreeBSD arm64/amd64 via a test library, the real CDM's run is H4's first check) |
| H5 ✔ | Hardening: fuzz targets, version-watch CI, optional bhyve wrapper | fuzzers run clean 1 h each; CI job live | done 2026-11-12: `docs/reviews/2026-11-12-hospes-h5.md` (five targets clean 1 h each after replacing the `ar` crate; `hospes-version-watch.yml` live, green on x86_64 and aarch64; bhyve wrapper not built) |

About three months in all plus K1, with OpenH264 usable after H2 (about six weeks in). EME in the
Navigator engine (the page-facing `requestMediaKeySystemAccess` and friends) is the Navigator
thread's side and is not in this spec; it only needs the control messages of §8.2.
