# Carillon — doorbell-driven guest→host aqueduct-gpu transport

> *A carillon is a tuned set of bells rung together — the doorbell at the
> heart of this transport, and the host coalescing many completions into
> one ring per wake. The name is a QEMU/Mesa-ecosystem codename in the
> lineage of `venus` (the path it replaces); it deliberately does **not**
> follow Atrium's Latin / classical-architecture convention, because the
> device + host endpoint live in QEMU, not in the Atrium runtime.*

> **Status.** **COMPLETE — full real frame VM → host → Metal, verified.**
> A FreeBSD-VM userspace program (`carillon-guest`) drives the whole
> aqueduct-gpu wire through `/dev/carillon0` (MSI-X doorbell + shared
> memory) to the host daemon's `Session` → `MoltenVkBackend` → Metal on
> the Apple M4 Max; the rendered green triangle returns through the rings
> (`ROUND-TRIP OK`, `centre pixel = [51,217,77,255]`). Host `Session` +
> guest `GpuClient` are reused **verbatim** over the Carillon byte-stream
> bridge; the transport core is the shared `carillon-transport` crate.
> Earlier milestones: T0 host loopback, T1 guest kmod, the MSI-X doorbell
> root-cause, the host-side bridge→Metal test (`carillon_wire`). See "VM
> bring-up (verified)" below for the run recipe + the MSI-X requirements.
> The forward-looking replacement for the venus
> paravirt path (`atrium-venus.md`, superseded) and for the *polling*
> ivshmem sketch in `aqueduct-gpu.md` §6.1–6.2. This document specifies
> **Carillon**, the transport that carries the aqueduct-gpu wire across
> the VM boundary; it does not change the wire itself.
>
> **Companion docs.** Read `aqueduct-gpu.md` first (the FrameOp wire,
> backend tiers, memory transport §4.4) and `atrium-gpu-abi-v2.md`
> (kmod cdev/ioctl/mmap conventions). This doc plugs *underneath* the
> aqueduct-gpu wire: it is how the guest's FrameOp stream reaches the
> `aqueduct-gpu-host` daemon (and thus `MoltenVkBackend` → Metal) when
> the producer is a FreeBSD guest under macOS-HVF instead of a host
> process on a Unix socket.
>
> **Naming.** **Carillon** (locked 2026-06-02). QEMU-side codename, not
> bound to Atrium's Latin-architecture convention — the device and host
> endpoint are QEMU plumbing, like `venus`. Use `carillon` for the QEMU
> device variant, the host transport frontend, and the guest kmod.
>
> **One-line summary.** A fully interrupt-driven shared-memory carrier
> for the aqueduct-gpu FrameOp stream between a FreeBSD guest and a
> macOS host daemon. Both sides **sleep on a doorbell** (guest: MSI-X
> ISR; host: a blocking eventfd read) — no ring-spin, no vsync timer,
> no control-register polling. It replaces venus (which polled and
> panicked in libthr) *and* improves on our own ivshmem reference
> (which still polls control registers every frame).

---

## 1. Why — and why this shape

### 1.1. What we are replacing

Two prior transports get the guest's render commands to a host GPU; both
are wrong for Atrium long-term:

- **venus** (`atrium-venus.md`, superseded). A generic Vulkan-shaped
  paravirt protocol over virtio-gpu + virglrenderer + MoltenVK. Rejected
  in `aqueduct-gpu.md` §1 for a long tail of HVF/virgl interaction bugs,
  and specifically: it **busy-serializes** every `vkCmd*` through
  MoltenVK's `MTLCommandBuffer` release pipeline, and its ring shmem
  aliases libthr's `struct pthread` under HVF stage-2 staleness, causing
  the `mutex_assert_is_owned` panic we never fully closed
  (`feedback_venus_libthr_panic_followup`). venus also has no home past
  D5 — the whole virtio-gpu/virgl/venus chain is ripped out for native
  drivers anyway.

- **The polling ivshmem reference** (`~/src/fresco` +
  `~/src/karythra-os`). This is *our own* prior art and the structural
  basis for this design — but it polls. The host "server polls
  [`CTRL_SLOTS_ALIVE_MASK`] each frame"; the guest's `shmem_sync` /
  `shmem_upload_blob` spin on `CTRL_CMD_READ` / `CTRL_COMP_WRITE`
  (`ivshmem_gpu.rs:246`, `:341`); the guest main loop falls back to
  `wait_for_wake_or_timeout(16)` — a 16 ms vsync-shaped timer even when a
  doorbell exists. Polling burns a core and defeats CPU parking. That is
  exactly the energy waste the user wants gone.

### 1.2. The thesis

> **Sleep until there is work; wake exactly once when there is; do zero
> work in between.**

Both endpoints block. The guest producer writes a frame to the ring,
rings the host's doorbell (one MMIO write), and the *submitting thread*
returns immediately — it does not wait for completion unless the caller
asked for a fence. The host daemon's transport thread is parked in a
blocking read on an eventfd; QEMU makes that fd readable when the guest
rings; the thread wakes, drains the ring, runs `submit_frame`, writes
completions, and rings the guest's doorbell. The guest's MSI-X ISR fires,
wakes the one task waiting on that fence, and the ISR returns. No spin
loop, no periodic timer, on either side, in steady state.

This is the property venus never had (it serialized synchronously) and
the reference gave up (it polled). It is also the property that lets
**Laminar park cores** — see §7.

### 1.3. Non-goals

- **Not a new wire.** The bytes on the ring are the existing aqueduct-gpu
  FrameOp stream (`FrameBuilder`/`FrameDecoder`, `aqueduct-gpu.md` §5).
  This doc defines framing/notification/flow-control around that stream,
  nothing semantic.
- **Not the native-HW path.** On real FreeBSD GPUs (D5+) there is no
  guest/host boundary; the client talks to the atrium-gpu kmod directly
  (`aqueduct-gpu.md` §2). This transport exists only for the
  macOS-HVF + QEMU bring-up/dev/CI environment.
- **Not a display transport.** Scanout on the FreeBSD VM already works
  through the native atrium-gpu/atrium-display kmod path
  (`project_d0_step1`); frescod scans out there. This carries *render*
  work to a host GPU backend, not pixels to a screen.

---

## 2. Architectural placement

```
              GUEST (FreeBSD VM under macOS-HVF)            HOST (macOS)
  ┌─────────────────────────┐
  │ atrium-vk-icd  /  frescod renderer (Tier-3 route)       │
  │   builds aqueduct-gpu FrameOp stream (FrameBuilder)     │
  └───────────┬─────────────┘
              │ write(2) / ioctl on the transport cdev
  ┌───────────▼─────────────┐
  │ transport kmod (this doc)│  newbus PCI driver for ivshmem-doorbell
  │  • maps BAR2 shmem       │  (vendor 0x1AF4 dev 0x1110), MSI-X vector
  │  • owns submission ring  │
  │  • rings host doorbell   │   BAR0 Doorbell write ──┐
  │  • MSI-X ISR → wake task │   ◄── MSI-X (guest)     │ QEMU ivshmem-doorbell
  └───────────┬─────────────┘                          │  device + ivshmem-server
              │ shared memory (BAR2 ⇄ host shm mmap)   │
══════════════╪═════════════════════════════════════════╪═══ VM boundary ═══
              │                                          ▼
  ┌───────────▼──────────────────────────────────────────────────┐
  │ aqueduct-gpu-host daemon  (transport = "ivshmem" instead of    │
  │   a Unix socket; everything above the transport is unchanged)  │
  │   • doorbell thread: blocks on eventfd, drains submission ring │
  │   • FrameDecoder → Session → Backend::submit_frame             │
  │   • writes completion ring, rings guest doorbell (eventfd)     │
  │            └─ MoltenVkBackend → MoltenVK → Metal (M-series)    │
  └────────────────────────────────────────────────────────────────┘
```

The daemon's existing `accept()`-on-`/tmp/aqueduct-gpu.sock` loop
(`aqueduct-gpu.md` §6.2) becomes one of two transport frontends. The new
frontend opens the ivshmem channel instead. **The `Session`, resource
tables, shader/pipeline caches, and `Backend` trait are identical** —
the transport only changes how a `Frame` envelope arrives and how a
completion leaves. This is the whole point of having defined the wire
transport-agnostically.

---

## 2.5. Where Carillon lives — and why almost none of it is in QEMU

A natural question (asked of every paravirt GPU path): *is Carillon a
QEMU thing?* No. Contrast with venus, which people misremember as "part
of QEMU" but was actually spread across three homes — none of which was
QEMU's interesting part:

| venus piece | Actual home |
|---|---|
| Guest Vulkan ICD (`libvulkan_virtio.so`) | **Mesa** (`src/virtio/vulkan/`) |
| Wire definition | **venus-protocol** (separate repo) |
| Host renderer (executes the stream) | **virglrenderer** (`src/venus/`, freedesktop) |
| Transport device | **QEMU** — only the generic `virtio-gpu` device, linking virglrenderer as a library (+ the BLOB/fence patches Atrium carries in `qemu-build`) |

That split — venus logic in Mesa + virglrenderer, every fix "one more
workaround in someone else's code" — is precisely the pain
`aqueduct-gpu.md` §1 cites for dropping venus.

**Carillon is built on `ivshmem-doorbell` specifically so it needs no
*new* QEMU code.** `ivshmem-doorbell` is a long-standing QEMU device
(`hw/misc/ivshmem.c`); the reference (`fresco/ivshmem_server.rs`) drives
it + an external server with zero device patches. So every Carillon
component lives in the **Atrium repo**, and QEMU contact is one launch
flag:

| Carillon piece | Home | Lang |
|---|---|---|
| Guest endpoint (PCI/MSI-X kmod, ring + doorbell) — §9 | **Atrium tree** (atrium-gpu kmod family / new kmod) | C |
| Host endpoint (ivshmem-server port + transport frontend) — §10 | **Atrium tree** (`aqueduct-gpu-host`) | Rust |
| The QEMU device | **`ivshmem-doorbell`, used as-is** — no Carillon-specific patch | — |
| QEMU touchpoint | **`scripts/run-vm.sh`** flags (`-device ivshmem-doorbell,vectors=N,chardev=…`) | — |

**Caveat — Atrium runs a QEMU fork regardless.** We do *not* run vanilla
QEMU: `qemu-build` already carries macOS-HVF fixes, the BLOB/virtio-gpu
fence-routing work, and other host improvements (some of which we may
upstream later; until then we ship the fork). The accurate claim is
therefore not "no fork" but **"no *new, Carillon-specific* QEMU code"** —
Carillon consumes the `ivshmem-doorbell` device the fork already provides
unchanged, and adds nothing to the QEMU patch set in v1. Keeping the
transport *logic* in our own daemon (not in QEMU/virglrenderer) is the
deliberate lesson from venus: our iteration loop, our license, one fewer
device to maintain, whole-frame validation we own.

**The one v2 exception.** If ivshmem's 2-peer model / per-connection
isolation / multi-consumer contention ever bites (§13, and
`aqueduct-gpu.md` §6.1's "dedicated `atrium-gpu-shmem` virtio device"
escape hatch), Carillon migrates to a **bespoke QEMU device**. *That*
device would be a *new* addition to the `qemu-build` patch set, and could
in principle be proposed upstream — but, like Atrium's existing HVF/BLOB
patches, it is Atrium-specific enough that it would most likely stay a
carried patch. It is explicitly **not v1**: v1 reuses the existing
`ivshmem-doorbell` device precisely to avoid *growing* the QEMU fork.

---

## 3. The doorbell mechanism (no spin, no poll)

This is the heart of the design and the part most directly grounded in
the reference. QEMU's `ivshmem-doorbell` device plus a tiny host-side
`ivshmem-server` give us two cross-boundary wakeups built on eventfds.

### 3.1. What QEMU's ivshmem-doorbell gives us

- **BAR2** = the shared memory region (mmap'd identically on both sides).
- **BAR0** = device registers: `IntMask@0x00`, `IntStatus@0x04`
  (read-to-clear), `IVPosition@0x08` (this peer's id), `Doorbell@0x0C`.
  Writing `(peer_id << 16) | vector` to the Doorbell register makes QEMU
  signal the *other* peer's eventfd for that vector → MSI-X interrupt in
  that peer. (Guest side confirmed working in karythra
  `ivshmem_gpu.rs:546–585`: enable MSI-X, unmask `IntMask`, register the
  IRQ, read `IntStatus` to clear in the ISR.)
- The host attaches to QEMU as the *other* ivshmem peer over a Unix
  socket, receiving the shmem fd and per-vector eventfds via
  `SCM_RIGHTS` (fresco `ivshmem_server.rs:142–163`, `send_i64_with_fd`).

### 3.2. Guest → host doorbell ("submit")

1. Guest writes a frame into the submission ring (§4), `dsb` barrier,
   advances the ring's write index (`shmem_write32`-style, with cache
   clean — BAR2 is mapped Normal-cacheable, see §6.3).
2. Guest writes the Doorbell register (BAR0 + 0x0C). One MMIO write.
3. **Guest submitting thread returns now.** If the caller wants a fence,
   it parks the *calling task* on that fence id (§5.3) — it does **not**
   spin on the ring.

QEMU turns the Doorbell write into a write on the host peer's eventfd.
On the host, that eventfd is exactly fresco's `_server_read_fd` /
`doorbell_read_fd()` (`ivshmem_server.rs:180–183`): *"blocking-read'able
from a thread that wakes the event loop, so the loop can sleep instead
of polling at vsync."* We make that the **only** wake source.

### 3.3. Host → guest doorbell ("complete")

1. Host writes completion records into the completion ring (§5).
2. Host calls `notify_peer()` (`ivshmem_server.rs:166–172`) — an 8-byte
   write to `qemu_write_fd` → QEMU raises the guest's MSI-X vector.
3. Guest MSI-X ISR fires, reads `IntStatus` to clear, and **wakes the
   tasks waiting on completed fence ids** (it does not itself parse the
   ring beyond what's needed to know "drain now"; the woken task drains).

### 3.4. The no-spin invariant (the improvement)

We delete every poll in the reference:

| Reference (polls) | This design (sleeps) |
|---|---|
| host "polls `SLOTS_ALIVE_MASK` each frame" | host blocks in `read()`/`kevent()` on the doorbell eventfd |
| guest `shmem_sync` spins on `CTRL_CMD_READ` (`:250`) | guest needs no sync-spin; submit is fire-and-forget, fences are doorbell-woken |
| guest `shmem_upload_blob` spins on `CTRL_COMP_WRITE` (`:341`) | guest task parks on a fence; ISR wakes it |
| guest main loop `wait_for_wake_or_timeout(16)` (`:963`) | guest task sleeps with **no timeout** in steady state |

A bounded timeout survives only as a **liveness backstop** (a slow
watchdog, seconds not milliseconds, see §5.4) — never as the steady-state
wake path. On the host, the doorbell thread uses `kqueue`
(`EVFILT_READ` on the eventfd) per the native-primitives charter — not a
busy `recv` loop, not `libevent`.

---

## 4. Shared-memory layout

BAR2 is carved into a control page, two rings, and a region table. This
follows the reference's slot/ring shape (`ivshmem.rs`, `ivshmem_gpu.rs`)
but drops the per-frame *content* (CAS staging) — large data rides the
separate host-visible memory transport (§6), exactly as
`aqueduct-gpu.md` §4.4 / §6.1 already specify ("Memory regions
themselves are NOT in ivshmem").

```
BAR2 (shared, identical VA-relative offsets both sides):

  [0x00000 .. 0x01000)  control page (4 KiB) — handshake + ring indices
  [0x01000 .. 0x10000)  submission ring  (guest → host)   60 KiB
  [0x10000 .. 0x1F000)  completion ring  (host → guest)   60 KiB
  [0x1F000 .. 0x20000)  reserved
  [0x20000 .. end    )  region table     (host → guest)   BO descriptors
```

Control page (`u32` fields, LE, cache-line padded — generalises
`ivshmem.rs` ctrl regs):

```
  0x000  magic            'AGVT'  (transport id + version)
  0x004  abi_version
  0x008  host_status      0=down 1=ready
  0x00C  guest_status     0=down 1=booted (guest zeroes on attach, as in
                          ivshmem_gpu.rs:605 — host detects re-boot)
  0x010  host_page_size   16384 on Apple silicon, 4096 elsewhere (§6.2)
  0x020  sub_write        guest writes, host reads     (submission head)
  0x024  sub_read         host writes, guest reads     (submission tail)
  0x028  comp_write       host writes, guest reads     (completion head)
  0x02C  comp_read        guest writes, host reads     (completion tail)
  0x040  caps             backend caps (mirrors HandshakeResponse::caps)
  ...    reserved
```

Each ring is a classic SPSC ring of fixed-size **descriptors** (not the
payload). A submission descriptor references a *frame blob* — the
serialized FrameOp stream — by `(offset, len)` into a frame-staging
arena, OR inlines small frames. The producer never blocks on a full
ring beyond a bounded back-pressure wait that *also* sleeps on the peer's
doorbell (a full submission ring means the host is behind; the guest
parks on the completion doorbell, not a spin — fixing
`submit_command_raw`'s `spin_loop()` at `ivshmem_gpu.rs:379`).

Ring index discipline is the reference's: monotonically increasing `u32`
write/read counters, `index = counter % ENTRIES`, `dsb` between payload
write and counter advance, cache-clean on write / cache-invalidate on
read (§6.3). Single-producer/single-consumer per direction, so no ring
lock is needed across the boundary (the guest's intra-guest
`RING_LOCK`, `ivshmem_gpu.rs:49`, stays — it serialises guest threads,
not the boundary).

---

## 5. Submission / completion / fence model

### 5.1. Submission descriptor

```
struct SubDesc {            // 64 B, in the submission ring
  u32 kind;                 // 1 = frame, 2 = inline-control
  u32 fence_id;             // 0 = fire-and-forget; else completion key
  u32 frame_off;            // offset into frame-staging arena (BAR2 or region)
  u32 frame_len;            // bytes of FrameOp stream
  u32 flags;                // e.g. NEEDS_READBACK
  ...                       // reserved / small inline payload
}
```

The FrameOp stream itself is unchanged: `BeginRenderPass`,
`BindPipeline`, `Draw`, `EndRenderPass`, `CopyImgToBuf`, `FillBuffer`,
… decoded by the daemon's existing `FrameDecoder` and replayed by
`Backend::submit_frame` (today `MoltenVkBackend::record_and_submit`).

### 5.2. Completion descriptor

```
struct CompDesc {           // 64 B, in the completion ring
  u32 kind;                 // 1 = frame-done, 2 = error
  u32 fence_id;             // matches SubDesc.fence_id
  u32 result;               // 0 = ok; else error code
  u32 readback_off;         // if NEEDS_READBACK: where the bytes landed (§6)
  u32 readback_len;
  ...
}
```

### 5.3. Fence semantics — the aqueduct fence, end to end

aqueduct-gpu already has a `fence_id` concept (the wire's completion
contract). We bind it straight onto the transport:

- Guest assigns `fence_id`, submits, and **parks the calling task** on a
  per-fence wait object (an intra-guest condvar/waitqueue keyed by
  `fence_id`) — *not* a ring spin.
- Host runs `submit_frame`, gets the backend's fence signal (on
  `MoltenVkBackend`, the `vkWaitForFences` after `record_and_submit`),
  writes the `CompDesc`, advances `comp_write`, and rings the guest
  doorbell **once per drain batch** (coalesced — if the host completed N
  frames in one wake, it rings once, not N times).
- Guest ISR wakes; the woken task drains the completion ring, matches
  `fence_id`s, and releases each waiter. `fence_id == 0` frames never
  park anyone — pure fire-and-forget (the common compositor case).

This is the model the reference *wanted* (`shmem_upload_blob` returns the
CAS hash on completion) but implemented as a spin; here it is a sleep.

### 5.4. Liveness backstop (not a poll)

A single low-frequency watchdog (e.g. one `kqueue` `EVFILT_TIMER` at
1–2 Hz on the host; one coarse timer task on the guest) exists **only**
to detect a dropped doorbell / crashed peer and to surface an error to
waiters, never to drive normal completion. Steady-state frames must wake
purely via doorbell. CI asserts "doorbell wakeups ≈ frames; timer
wakeups ≈ 0" so a regression that reintroduces polling is caught.

---

## 6. Memory / BO transport

Large data (textures, vertex/SSBO buffers, readback targets) does **not**
flow through the rings. It uses the host-visible region mechanism that
`aqueduct-gpu.md` §4.4 / §6 already specify and that the import path
(`examples/import_region`) already VM-verified:

1. Host allocates a region (`shm_open` + `ftruncate` + touch + `mlock`),
   registers it with QEMU, exposes it to the guest via the atrium-gpu
   kmod cdev mmap path. (`aqueduct-gpu.md` §4.4 steps 1–6.)
2. The transport's **region table** (BAR2 `0x20000+`) carries
   `(region_id, gpa/bar-offset, len, page_size)` descriptors so the
   guest can resolve a region referenced by a FrameOp without an extra
   round trip.
3. On `MoltenVkBackend`, a region backs an `MvkBuffer` (host-visible +
   coherent, already mapped) so readback (`CopyImgToBuf` →
   `buffer_read_bytes`) lands in memory the guest can read directly.

### 6.2. Host-page-size alignment (Apple 16 KiB vs guest 4 KiB)

This is a known sharp edge from the D0 work (`ATRIUM_HOST_PAGE_SIZE`).
macOS on Apple silicon uses **16 KiB** pages; the FreeBSD guest assumes
**4 KiB**. Any region that both sides map, and any ring boundary the
host `mmap`s, **must be 16 KiB-aligned and 16 KiB-multiple-sized** or the
host mapping straddles/short-maps. The control page publishes
`host_page_size` (control `0x010`) at handshake; the guest kmod rounds
all region and ring sizes up to that. Ring base offsets in §4 are chosen
16 KiB-aligned for this reason.

### 6.3. Cache coherency under HVF

BAR2 is mapped **Normal cacheable** on the guest (per the D0 finding that
HVF stage-2 + cacheable + explicit `DC CIVAC`/`DC IVAC` propagates to
host physical RAM; the reference relies on the same —
`ivshmem_gpu.rs:124–141`, `cache_clean_range` on write, `invalidate` on
read). The transport kmod owns these barriers; producers/consumers above
it see a coherent ring. The host side, being plain `mmap` of the shm fd,
needs no explicit flushes (x86/arm host caches are coherent with its own
mapping); the guest's `DC` ops are what cross HVF.

---

## 7. Energy synthesis — why interrupt-driven matters here

This transport is the macOS-host instance of the **Tier-3** render path,
and its interrupt-driven shape is load-bearing for Atrium's energy story
(`energy-policy.md`, `feedback_energy_policy_coordinated_not_coupled`):

- **It lets Laminar park cores.** A polling transport pins a guest core
  in a spin loop and a host thread in a busy `recv`; both defeat CPU
  parking and DVFS down-clocking. A sleeping transport leaves the guest
  vCPU genuinely idle between frames, so Laminar's RLC loop can park it —
  the whole reason `wait_for_wake_or_timeout` was a regression risk
  (`feedback_laminar_ctrl_default_on`: parked CPUs stay parked only when
  nothing keeps waking them).
- **It complements the tier router, without coupling to it.** The router
  (energy-policy phase 1) decides Tier-2 (CPU, no GPU wake) vs Tier-3
  (this transport → GPU). The transport doesn't read Laminar's state and
  Laminar doesn't read the transport's — they share only the read-only
  mode/headroom signal. The transport's contribution to the policy is
  structural: *being cheap when idle* is what makes "route to GPU" not a
  standing energy cost between frames.
- **No renderer term leaks into Laminar.** Consistent with the locked
  decision: Laminar never learns this transport exists; it only ever
  parks an idle vCPU it already knows how to park.

The energy win over both predecessors is concrete: venus spun MoltenVK's
command pipeline every frame; the reference polled control regs every
frame; this design does I/O only when a frame actually crosses the
boundary, and at most one wakeup per direction per batch.

---

## 8. Trust boundary

The guest is **untrusted** with respect to the host GPU. The host daemon
already owns this boundary for the socket transport; the ivshmem
transport does not weaken it:

- The host **validates every FrameOp** it decodes (bounds, resource-id
  ownership within the connection's id namespace, no raw host pointers
  on the wire — ids only, per `aqueduct-gpu.md` §4.2). A malformed or
  hostile ring entry yields a `CompDesc` error, never a host crash.
- Ring indices are validated host-side: a guest-supplied `sub_write`
  that jumps or wraps maliciously is clamped/rejected; the host trusts
  only its own `sub_read`.
- Region references resolve through the host's region table; the guest
  cannot name a region it wasn't granted.
- The shmem fd and eventfds are passed host→guest at QEMU setup; the
  guest cannot forge additional peers (single-peer device, fresco's
  2-peer server).

Whole-frame validation in one pass is cheaper than venus's per-`vkCmd`
checking and is where the "host validates FrameOps" guarantee lives.

---

## 9. Guest kmod (FreeBSD, native primitives only)

A new FreeBSD kernel module — the transport's guest endpoint — written
in **C** (kernel = C per `LANGUAGE-POLICY`), using **only native
primitives** (newbus, MSI-X via the standard PCI methods, cdev,
`bus_dma`/`pmap` for the cacheable BAR2 mapping). **No linuxkpi, no
drm-kmod, no shims** (`feedback_no_linuxkpi`). It is *not* karythra's
microkernel driver — that's the reference for ring/doorbell semantics,
not portable code.

Responsibilities:

- `newbus` PCI attach matching ivshmem (vendor `0x1AF4`, device
  `0x1110`); map BAR0 (registers) + BAR2 (shmem).
- MSI-X allocation (`pci_alloc_msix`) for the doorbell vector; an
  `intr` handler that reads `IntStatus` to clear and wakes the waiting
  task(s) — the FreeBSD analogue of karythra's `irq::register` + ISR
  (`ivshmem_gpu.rs:576–582`).
- Cdev exposing: (a) a submit path (write/ioctl that enqueues a `SubDesc`
  and rings BAR0 Doorbell), (b) a fence-wait path (`kqueue`
  `EVFILT_USER`-style or a blocking ioctl that `tsleep`s on the fence
  waitqueue — woken by the ISR), (c) `mmap(2)` of granted regions for
  zero-copy BO access (reusing the atrium-gpu kmod mmap convention; this
  transport kmod may *be* part of, or sit beside, the atrium-gpu kmod).
- BAR2 cacheable mapping + the `DC CIVAC`/`DC IVAC` discipline of §6.3.

The userspace producer (`atrium-vk-icd` / frescod renderer) is **Rust**
and talks to this cdev. A thin safe Rust binding (analogue of
`atrium-gpu-rs`) wraps it.

---

## 10. Host attach (reuse fresco/ivshmem_server.rs)

The host side reuses the working code in
`~/src/fresco/src/platform/ivshmem_server.rs` essentially verbatim,
adapted into the `aqueduct-gpu-host` daemon as a second transport
frontend:

- `IvshmemServer::new(sock, shmem_path, size)` — bind the ivshmem-server
  Unix socket QEMU connects to, create the shm fd + the two eventfd
  pipes, `send_init` the SCM_RIGHTS handshake (version, peer id, peer
  connect fd, shmem fd, interrupt fd) in QEMU's expected order
  (`:142–163`).
- `doorbell_read_fd()` (`:183`) → register on the daemon's `kqueue`
  (`EVFILT_READ`). The daemon's doorbell thread blocks here.
- `notify_peer()` (`:166`) → ring the guest after writing completions.
- QEMU launches with `-device ivshmem-doorbell,vectors=N,chardev=…` over
  the ivshmem-server socket (mirrors `scripts/run-vm.sh`'s existing
  ivshmem wiring; the `--venus` flag's virtio-gpu-gl path is orthogonal
  and can be dropped for this transport).

The daemon gains a `--transport ivshmem|socket` selector (default
`socket` for host-only dev/CI; `ivshmem` for VM runs). Above the
transport, `Session` + `Backend` are untouched, so the existing
93/93 lib + 81/81 smoke tests keep covering the backend; new tests cover
the ring/doorbell framing.

---

## 11. Dev / CI / VM vs real HW

| Environment | Transport | Backend |
|---|---|---|
| Host dev / CI (macOS) | Unix socket (existing) | Tier-2 SW or `MoltenVkBackend` |
| FreeBSD VM under macOS-HVF | **this transport (ivshmem-doorbell)** | host `MoltenVkBackend` → Metal |
| Real FreeBSD HW (D5+) | none — kmod-direct | native atrium-gpu Vulkan driver |

This transport is **exactly** the VM-row carrier: it is how a process
inside the FreeBSD guest reaches the host's MoltenVK/Metal, which is the
capability the user asked for ("a modern venus replacement so the VM can
reach MoltenVK"). It is dev/bring-up infrastructure, not a shipping
component — on real hardware the guest/host split disappears.

---

## 11.5. VM bring-up (verified)

The guest↔host doorbell round-trip is **verified end-to-end in the
FreeBSD VM**, interrupt-driven over **MSI-X via the GIC ITS** (no spin,
no poll): `carillon_smoke` mmaps `/dev/carillon0`, reads the control page
(magic `0x54564741`, host_page_size `16384`), stages a frame, rings the
host (BAR0 doorbell), the host's `serve_ivshmem` wakes on kqueue, runs
the backend, and rings back; the guest wakes on the MSI-X ISR and reads
the completion — **ROUND-TRIP OK in ~0.02 s** (vs ~1 s on the
INTx-timeout fallback). BAR2 is mapped `VM_MEMATTR_WRITE_BACK` (cacheable)
and is coherent across HVF with the host's `mmap`.

**Platform requirements (the part that took real digging):**

1. **`gic-version=3`** in QEMU (`scripts/run-vm.sh --carillon`). PCI MSI-X
   on arm64 needs the **GIC ITS**, which only exists with GICv3. (GICv2 +
   gicv2m has no ACPI IORT → no PCI MSI routing.)
2. **`hw.pci.honor_msi_blacklist=0`** in the guest `/boot/loader.conf`.
   **This was THE blocker.** FreeBSD's `pci_msix_blacklisted()` blacklists
   MSI on "non-PCIe chipsets"; on arm64 the `pcie_chipset` global is
   unset, so QEMU's host bridge (which lacks `PCI_QUIRK_ENABLE_MSI_VM`)
   is blacklisted → MSI-X disabled for **every** PCI device (virtio
   included, which silently falls back to INTx). Turning the blacklist
   off lets virtio *and* Carillon use MSI-X via the ITS.
3. The guest kmod must **allocate the MSI-X table BAR** (BAR1 here):
   `pci_alloc_msix` requires that BAR mapped + `RF_ACTIVE` in the driver's
   resource list before it hands out vectors (the bus does not map it).

**What it is NOT:** not an HVF limitation (HVF injects guest-allocated
MSIs fine via QEMU userspace), not the ITS/IORT (QEMU's IORT correctly
maps RIDs `0..0xffff` → the ITS group; the emulated ITS works under HVF),
and not a `Carillon`-specific QEMU patch. **No new QEMU code is required**
(confirming §2.5): MSI-X delivery under HVF rides the *pre-existing*
atrium ivshmem **poll-timer** (`msix_notify`), which is the substitute
for KVM irqfd that macOS HVF lacks. The legacy-INTx path in the kmod is a
robustness fallback only; on this platform MSI-X is the live path.

---

## 12. Phased implementation plan

Each phase is independently VM-verifiable; cross-compile on the macOS
host (`cargo build --release --target aarch64-unknown-freebsd`), run in
the VM — **never `cargo --release` inside the VM**
(`feedback_no_vm_cargo_release`).

- **T0 — Host attach + loopback.** Port `ivshmem_server.rs` into
  `aqueduct-gpu-host` as a transport frontend; stand up the control page
  + both rings in a shm file; prove doorbell round-trip with a *host-only*
  fake guest (a test process mmap'ing the same shm, ringing eventfds).
  No kmod yet. Asserts the no-spin invariant on the host side.
- **T1 — Guest kmod attach + shmem map.** FreeBSD newbus driver attaches
  to the ivshmem device, maps BAR0/BAR2, reads the control page,
  completes handshake (`guest_status=booted`, reads `host_status=ready`).
  Verify in VM: dmesg shows attach, `host_page_size` read correctly.
- **T2 — Guest doorbell + ISR.** MSI-X alloc + ISR clearing `IntStatus`;
  guest rings host Doorbell, host wakes from `kqueue` and rings back,
  guest ISR fires. Verify: doorbell count increments on both sides, zero
  timer wakeups.
- **T3 — One frame end to end.** Submit a trivial FrameOp frame
  (clear+readback) from a guest userspace test through the kmod →
  transport → daemon → `MoltenVkBackend` → Metal; completion + readback
  bytes return through the completion ring to the guest. This is the
  "first pixel from the VM through Metal" milestone — the venus
  replacement proven.
- **T4 — Fence parking + fire-and-forget.** Wire `fence_id` to a guest
  waitqueue (`tsleep`/wakeup); prove a fenced submit parks the caller and
  the ISR wakes it; prove `fence_id==0` submits never park. CI asserts
  doorbell-wakeups ≈ frames, timer-wakeups ≈ 0.
- **T5 — Region/BO transport.** Hook the region table to the existing
  host-visible region mechanism (`examples/import_region`); a guest
  FrameOp that samples a texture region renders correctly on Metal.
- **T6 — Real client.** Route `atrium-vk-icd` (or the frescod Tier-3
  route) over the transport; render a graphics-draw app from the VM on
  Metal. Unblocks the Tier-2↔Tier-3 crossover bench (energy-policy
  phase 1) *from inside the VM*, not just host-only.

---

## 12a. Graphics coverage: what runs through to Metal today (and the gap to a game)

Audited 2026-06-16. The transport and the *wire grammar* are the strong
part; the gap to "an arbitrary game's Vulkan runs on Metal" is two bounded
implementation pieces, not a design unknown. Be precise about which.

**Already solid (do not re-investigate):**
- **Transport — DONE & verified.** The green-triangle round-trip
  (`aqueduct-gpu-host/tests/carillon_wire.rs::real_frame_through_carillon_bridge_to_metal`)
  is real: VM → Carillon ring/doorbell → host `MoltenVkBackend` → Metal →
  pixels back through the ring (`[51,217,77,255]`).
- **Wire vocabulary — COMPLETE, and NOT venus-style.** `aqueduct-gpu/src/opcodes.rs`
  `FrameOp` is a *closed, high-level* graphics grammar that already names
  the full surface: `BeginRenderPass`, `BindPipeline`, `BindVertexBuf`,
  `BindIndexBuf`, `Draw`/`DrawIndexed`/`DrawIndirect`, all the
  `SetCullMode`/depth/stencil dynamic state, `Dispatch`, `CopyBufToImg`,
  `Blit`. The "venus-class serialization layer" worry does **not** apply —
  the protocol is designed and smaller than per-`vkCmd` serialization.

**Gap 1 — host backend translation is PARTIAL.** `MoltenVkBackend::record_and_submit`
(`aqueduct-gpu-host/src/moltenvk.rs`) wires render pass + graphics pipeline
+ `vkCmdDraw` + the full **compute** path (dispatch/descriptors/push-constants/
barriers) — all tested. But the ops a real game needs are *parsed and then
silently dropped* by the catch-all `_ => { /* not yet modelled on tier-3 */ }`
at **moltenvk.rs:1235**: vertex/index buffer binding, `DrawIndexed`, **texture
sampling** (sampled-image descriptors + `SHADER_READ_OPTIMAL` layout),
`CopyBufToImg`, and most `SetXxx` dynamic state. A game doing
`vkCmdBindVertexBuffers`/`vkCmdDrawIndexed` renders **black with no error** —
geometry never reaches the rasterizer. The verified triangle dodged this by
being a *procedural* fullscreen tri (VS synthesizes positions from
`gl_VertexIndex`; no vertex buffer). Closing it is match-arm codegen (~2wk),
not architecture. **moltenvk.rs:1235 is the literal line where "renders a
triangle" becomes "renders a game."** (Note: the Tier-2 SW/CPU backend,
`tier2_backend.rs`, already implements all of these — it's only the Metal
backend that's partial.)

**Gap 2 — the guest-side ICD is a STUB.** `atrium-vk-icd/src/lib.rs` is
skeleton-only: loader negotiation succeeds, then every `vkGetInstanceProcAddr`
returns NULL. The verified triangle **bypassed it** — the test hand-builds the
`FrameOp` stream via a `GpuClient` emitter. So an off-the-shelf game going
through the normal Vulkan loader → `atrium-vk-icd` → `FrameOp` has no working
translator yet. Larger than Gap 1, but bounded: it targets the high-level
closed grammar above, not full Vulkan reflection. (This is T6's `atrium-vk-icd`
route, made concrete.)

**UI vs games — the split this enables:**
- **Frescod-on-Metal (the desktop) needs NEITHER gap fully closed.** frescod is
  its own renderer: it can emit `FrameOp`s directly (the `GpuClient`/Tier-2
  style), **bypassing the stub ICD**, using only the *already-verified* subset
  (render pass + pipeline + `Draw`). So host-side compositing on Metal is
  reachable on proven code — no ICD, no vertex-buffer work. This is the route
  to delete the per-frame guest↔host readback for the desktop.
- **An arbitrary game on Metal needs BOTH gaps** — it links the real loader
  (Gap 2) and uses vertex/index buffers + textures (Gap 1).

---

## 13. Open questions

- **One device or fold into atrium-gpu kmod?** The transport cdev and
  the atrium-gpu BO/mmap cdev overlap (both map host-visible regions).
  Decide at T1 whether this is a separate kmod or a transport mode of the
  atrium-gpu kmod. Leaning: separate kmod, shared region-mmap helper.
- **Multi-connection.** The reference is 2-peer / per-slot (4 slots).
  Phase 1 is single guest producer multiplexed by the daemon's
  per-connection id namespace inside one ivshmem channel; a second
  ivshmem device per jail is a v2 question (mirrors `aqueduct-gpu.md`
  §6.1's "dedicated atrium-gpu-shmem virtio device" escape hatch).
- **Vector count.** One MSI-X vector suffices (completion doorbell). A
  second vector for out-of-band errors/region-table updates is optional;
  `vectors=2` matches the reference's QEMU line.
```
