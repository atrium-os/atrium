# Atrium display driver architecture — stance & first milestone

**Status:** notes, 2026-06-10. Non-binding; settled in discussion, feeds the
display ABI + the gpusim display model. Companion to the GPU driver stance
[`atrium-gpu-driver-architecture.md`](atrium-gpu-driver-architecture.md), the
ABI [`atrium-gpu-abi-v2.md`](atrium-gpu-abi-v2.md), and the AMD module design
[`atrium-gpu-amd-design.md`](atrium-gpu-amd-design.md). The functional/timing
model is the gpusim repo (separate).

This captures the architecture of the **display** half of the GPU stack — the
block that drives a monitor — and the first milestone (`D-display-1`). The
short version: the display is a *decoupled engine* that scans out a *shared BO*,
and the model that earns it is a **scanline-accurate raster timing simulator in
deterministic virtual time** — which is also gpusim's on-ramp to the
timing/energy prize.

## 1. Principle: the display is a decoupled engine over a shared buffer

On real silicon the display block (AMD DCN, etc.) is on-die but
*architecturally independent* of the graphics/compute engine — its own
registers, its own path to the memory controller, its own outputs. That maps
onto the settled energy stance ([`feedback_energy_policy_coordinated_not_coupled`])
— "display shares energy *intent* read-only but keeps independent mechanisms."

So the display driver is **not** another consumer of the GPU command stream.
The data path is:

```
compositor → (GPU) render into a BO → hand the BO to the display
           → (display) scan it out every refresh
```

Two earlier decisions make the seam almost free:

- **fd-as-handle BOs (M9a) are the dma-buf-equivalent.** The compositor
  allocates a BO from the GPU driver, renders into it, and passes the *same BO
  fd* to the display for scanout — SCM_RIGHTS-passable, no separate
  import/export machinery. The fd *is* the shared-buffer primitive.
- **VRAM residency (M18) is what a scanout framebuffer wants.** The display
  DMA-reads the FB every refresh, so it is VRAM-resident, and the display reads
  it by **VRAM offset** (it does not walk GPUVM — it has its own simple
  scanout-address path; it takes the offsets from the BO's `pages[]`).

The scanout FB is therefore a VRAM-resident BO shared by fd. The whole memory
story is already built.

## 2. Fidelity: scanline-accurate, because tearing is a scanline phenomenon

A display you can only *read back* tells you the final pixels were right. The
bugs that matter in a display driver are **timing** bugs — tearing, a missed
vblank, judder, a flip racing the render fence. To catch those the model must be
a **timing model**. Frame-granular would paper over tearing specifically, so the
decision is **scanline-accurate**.

This is a *raster* model, not a framebuffer model: the CRTC carries the full
mode timing — `hactive/hfront/hsync/hback` (→ `htotal`), the vertical
equivalents (→ `vtotal`), and the **pixel clock**. Refresh rate is a *derived*
quantity, `pixel_clock / (htotal × vtotal)`, not a number you set — which is
exactly what EDID's detailed timing descriptors carry, so DDC/EDID modeling and
the raster model feed each other.

## 3. The hard constraint: deterministic virtual time

gpusim is deterministic by charter — `Instant::now`/`Date::now`/`Math.random`
are banned; the async scheduler (`sched.rs`) is a fixed-point sweep, not a
wall-clock thing. A display clock that reaches for real time breaks that. So the
display clock is **virtual time** — a tick counter.

That is the feature, not the limitation: deterministic virtual time makes a tear
or a stutter **replayable**. "Frame 7 tore at scanline 412 because the flip
landed 0.3 ms after the render fence" is a reproducible assertion, not a flaky
once-in-a-while. The strict-referee/failure-fidelity philosophy applied to time.

**Substrate.** The display is a discrete-event simulator — `sched.rs`'s temporal
sibling. A virtual timeline holds scheduled events (vblank, flip-commit, HPD);
`advance_to(t)` pops them in timestamp order. Resolution is **picoseconds**
(a 1080p pixel is ~6.7 ns, so nanoseconds are too coarse to place a *column*;
`u64` ps still covers months).

**No pixel-stepping.** The beam position is an *analytic* function of virtual
time: `beam(t) = decompose((t − frame_start) × pixel_clock)` into
`(scanline, column)`, accounting for the blanking regions. Between events nothing
is computed; `beam(t)` is evaluated only at event boundaries. A 148.5 MHz 1080p
raster costs the same as 640×480 — integer math at a handful of boundaries.

## 4. "What was displayed" is a per-scanline integration

There is no single "current framebuffer" anymore. The displayed frame is **what
the beam painted over one refresh**, produced as a *per-scanline FB selection*:
for each scanline, which BO was bound when the beam crossed it.

- **vsync off:** a flip at beam scanline 412 → lines `[0,412)` from the old FB,
  `[412,height)` from the new — the **exact tear line**. Multiple flips in a
  frame → multiple tear lines.
- **vsync on:** the flip is *latched* and applied when the beam wraps to the top
  at vblank → the selection is uniform, no tear.

That integrated frame is the readback/verification surface. The deterministic
tests it unlocks are sharp and replayable: exact tear-line, vsync-produces-no-
tear, frame-repeat/judder count on a missed flip, dropped-flip on over-submit.

**Flip-over-submit policy:** queue depth 1, **drop-and-count**. A superseded
flip is a *recorded* stutter the referee asserts on, not a silent loss.

## 5. DDC / EDID / HPD as a modeled handshake

Same reasoning as the command stream: don't hand the driver an EDID struct —
model the **DDC (I²C) register handshake** so the driver's real EDID read-and-
parse path runs byte by byte, the way we model PM4/MMIO. The failure modes then
come for free, in the failure-fidelity spirit: a monitor that NAKs mid-read, a
truncated/corrupt EDID, **HPD** (hot-plug detect) connect/disconnect as a
scheduled timeline event. "Driver parsed a corrupt EDID and programmed an
impossible mode" is exactly what the referee should catch.

## 6. Two tiers of time (the sync-vs-async split, again)

The deterministic *engine tests* (`cargo test`) and the *live in-VM driver*
can't both own time. The split mirrors `submit_dma_ring` (sync, live) vs
`doorbell_async` (deterministic, referee):

- **Engine/referee tier → pure virtual time.** The test advances the clock
  explicitly ("run 3 refresh periods, here are the events"). Where tear/stutter/
  fence-race invariants are asserted deterministically. The real verification.
- **Live transport tier → host-timer vblank.** The QEMU device ticks vblank off
  a host timer so the actual kmod runs against real-ish refresh. Non-
  deterministic, but it is only "does the driver function," not "is the timing
  provably correct."

The timing-truth lives in the deterministic engine; the in-VM path is the
functional drive.

## 7. The ABI

- **Atomic commit.** One ioctl describes the desired display state (which FB on
  which plane on which CRTC at which mode), applied at the next vblank. Strictly
  better than legacy per-object setcrtc/pageflip; no half-applied states. Takes
  the *evidence* from KMS without cargo-culting the legacy object soup.
- **kqueue events**, consistent with the syncobj path (M9b): the display fd is
  `EVFILT_READ`-able; **vblank** and **flip-done** fire there, so a compositor
  folds "frame on screen" into the same `kevent()` as input, timers, and GPU
  completion.
- **In/out fences on the flip**, wired to the syncobj timeline (M9b): the flip
  waits on the render-complete fence (don't scan out a half-drawn frame) and
  signals a flip-done fence (the *previous* FB is now free to reuse). The tear-
  free double-buffer loop in primitives we already have. *Today the in-fence
  signals instantly* because the GPU is a functional/instant model — that is the
  seam GPU render-timing fills next (§9).
- **Scanout FB = a VRAM BO imported by fd** from the GPU driver (§1).
- **Opaque mode programming.** Like the GPU's opaque command stream: the actual
  pixel-clock / PHY / mode-timing register programming stays kernel/firmware-
  internal. Userspace names a mode parsed from EDID; the kernel programs it.

Essential objects (load-bearing for real hardware, kept from KMS): **connector**
(a physical port — EDID, hotplug), **CRTC** (the scanout pipe — timings, drives
an output), **plane** (primary / overlay / cursor layers). The **encoder** is
collapsed into the CRTC for now (add it when modeling DP link training /
bandwidth limits).

## 8. Energy lands here first

Scanline-accuracy gives an exact per-frame VRAM-read cost
(`width × height × bpp` fetched every refresh at the memory-controller energy
rate). That makes the two big display power levers concrete and *measurable*,
and the display carries gpusim's **first cost counters**:

- **Panel self-refresh (PSR):** an unchanged per-scanline selection → read a
  local copy, skip the VRAM fetch. "Unchanged" is well-defined here because the
  per-scanline FB selection is known.
- **Variable refresh (VRR/Freesync):** stretch `vtotal` to the flip → fewer
  fetches/sec.

This is the concrete hook the energy router ([`feedback_energy_policy…`]) has
waited for — the display is where the timing/energy model is *born*, because
here the timing is externally observable (frames on a screen at a cadence).

## 9. The staged consequence: GPU render-timing is next

Scanline-accuracy surfaces a finding. The flip commits at
`max(submit_time, fence_signal_time)`; the in-fence is the GPU render-done
syncobj. But the GPU is a **functional, instant** model today — render completes
at submit, the fence signals immediately. So we fully model **tearing and
stutter driven by the compositor's flip cadence vs. vblank** now (independently
valuable; catches real compositor bugs). The *other* frame-pacing failure —
**"the GPU render overran the frame deadline, so the flip slipped a refresh"** —
cannot happen until GPU render itself takes virtual time.

So the display is not just *a* timing model; it is the one that makes the GPU's
lack of timing visible. The arc: **display timing first** (externally
observable, self-contained), then **GPU render-timing** as the deliberate
follow-on that reuses the same virtual-time substrate to close the frame-pacing
loop — and that is also the dispatch/draw cost model the energy router wants.
**Decision: display first; GPU render-timing is the explicit next milestone.**

## 10. Referee invariants (failure-fidelity)

- flip to a non-resident FB faults;
- flip to a BO smaller than the mode faults;
- corrupt / out-of-range EDID parsed into an impossible mode faults;
- a vblank must follow a committed flip (no silent drop);
- tear iff vsync off (vsync-on must produce a uniform per-scanline selection);
- a dropped (superseded) flip is *recorded*, never silent;
- HPD-disconnect mid-scanout is a defined transition (black/last-frame), not UB.

## 11. First milestone — `D-display-1`

Single connector, single CRTC, primary plane. Encoder collapsed; overlays,
hardware cursor, multi-monitor, tiling/compression, and VRR deferred.

**gpusim (the bulk of the new work):**
1. the picosecond virtual-time discrete-event timeline (`advance_to`, scheduled
   events) — the `sched.rs` temporal sibling;
2. a display engine block: CRTC raster timing + analytic `beam(t)`; connector +
   EDID detailed timings over a modeled DDC/I²C register interface + HPD; the
   flip path (depth-1 queue, vsync on/off, in-fence, vblank latch); the
   per-scanline FB-selection integration → the painted frame;
3. the display register block the kmod programs (mode timing, FB base, flip
   trigger, DDC, vblank control);
4. the §10 referee invariants;
5. engine tests: exact tear-line, vsync-no-tear, judder count, dropped-flip.

**QEMU device:** a display register block forwarding to the model (like the
doorbell/regs apertures); host-timer vblank for the live path (can come later).

**kmod (`atrium-gpu-display`, or a file in `atrium-gpu-amd` extracted later —
§4.2 "split when the seam is real"):**
`QUERY_CONNECTORS → read EDID over DDC → SET_MODE → import a VRAM FB by fd →
atomic flip (with in-fence) → vblank/flip-done on the display fd → read back the
painted frame`.

**userspace test:** the path above end to end, asserting an exact tear line with
vsync off and none with vsync on.

## 12. Open questions (deferred, not blocking D-display-1)

- **kmod structure** — the §4.1 three-kmod split (pci / gpu / display) is now
  *justified*; pragmatically start the display in-tree and extract the kmod once
  the seam is real (the §4.2 file-split discipline).
- **Encoder model** — collapsed now; needed when modeling DP link training /
  link-bandwidth limits (a mode that exceeds link bandwidth should fault).
- **Multi-plane** — overlay + hardware cursor are real hardware compositing
  (an energy lever: skip GPU composition); additive on the per-scanline model.
- **VRR** — additive once the timeline exists (vblank scheduled at the flip,
  clamped to `[min,max]`).
- **Displayed-frame readback ABI** — a debug surface for the in-VM path (the
  engine tests read the integrated frame directly).

## 13. Summary position

The display is a decoupled engine scanning out a VRAM BO shared by fd. The model
that earns it is a **scanline-accurate raster simulator in deterministic
picosecond virtual time** — analytic `beam(t)`, per-scanline integration, exact
tears — fed by a modeled DDC/EDID handshake, asserting tear/stutter/fence
invariants, two-tiered (deterministic engine vs host-timer live path), carrying
gpusim's first energy counters (refresh / PSR / VRR). It is the system's first
timing surface, and it structurally pulls **GPU render-timing** in behind it as
the next milestone.
