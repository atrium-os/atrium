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
  seam GPU render-timing fills next (§10).
- **Scanout FB = a VRAM BO imported by fd** from the GPU driver (§1).
- **Opaque mode programming.** Like the GPU's opaque command stream: the actual
  pixel-clock / PHY / mode-timing register programming stays kernel/firmware-
  internal. Userspace names a mode parsed from EDID; the kernel programs it.

Essential objects (load-bearing for real hardware, kept from KMS): **connector**
(a physical port — EDID, hotplug), **CRTC** (the scanout pipe — timings, drives
an output), **plane** (primary / overlay / cursor layers). The **encoder** is
collapsed into the CRTC for now (add it when modeling DP link training /
bandwidth limits — §8).

## 8. The output topology — connectors, encoders, PHYs (HDMI / DP / USB-C)

"How does the driver detect and redirect output to HDMI vs DisplayPort vs
USB-C" is the output-side topology, and it is where the **queryable-capability /
opaque-mechanism** split (the same one that keeps the GPU command stream out of
the kernel) does the most work — because the interface zoo is enormously vendor-
and protocol-specific and must *not* leak into the neutral ABI.

**The real pipeline has two stages we collapsed.** Between the CRTC (§4, scanout/
raster) and the connector (§5, EDID/DDC/HPD) sit:

- an **encoder** — formats the CRTC's raw pixel stream into a wire protocol
  (TMDS for HDMI/DVI; the packetized, link-trained DP stream for DisplayPort);
- a **PHY** — the SerDes driving the actual lanes; a *shared, limited* resource
  (e.g. 4 PHYs for 6 connectors).

And the wiring between CRTCs, encoders, PHYs, and connectors is **not fixed — it
is a crossbar/mux**. "Redirect output to a different interface" is fundamentally
*reassigning that crossbar*: pointing a CRTC at a different connector through a
different encoder/PHY, subject to constraints (limited PHYs, which encoder speaks
which protocol, link bandwidth). Routing is an **assignment problem**, not a
lookup.

**The three interfaces differ in *detection* and *bring-up*, never in scanout:**

- **HDMI** — TMDS. Detect via a dedicated **HPD pin** + EDID over **I²C DDC**
  (exactly the `ddc_read` byte path the model already runs). Bring-up: set the
  TMDS clock. Our `Connector` today is HDMI-shaped.
- **DisplayPort** — a *trained packet link*. Detect via HPD + EDID/DPCD over the
  **AUX channel** (not I²C). Bring-up: **link training** — negotiate lane count
  (1/2/4) and rate over AUX, train CR + EQ, verify; a mode exceeding the trained
  bandwidth *fails* (or needs DSC). Plus **MST**: one port fanning out to
  *multiple* sinks via a hub/daisy-chain.
- **USB-C** — a *connector*, not a protocol. It carries **DP Alt Mode** (DP over
  the USB-C pins), negotiated by the **USB-C / Power-Delivery subsystem** over
  the CC pins, which decides *whether* DP is present at all, the **orientation**
  (reversible cable), and the **lane split** (4-all-DP vs 2-DP + 2-USB). The
  GPU's DP output is then *muxed* onto the negotiated pins. So USB-C display
  detection is a **cross-subsystem event** — PD says "a DP sink appeared on port
  X, orientation Y, N lanes," and only then does the display driver link-train.

**The neutral / opaque split (the answer to "redirect cleanly"):**

- *Neutral (kernel ABI, queryable — what the compositor touches):* a connector
  is a resource with a **type** (`HDMI`/`DP`/`USB-C`/`eDP`), an HPD state, and
  EDID. The type is a queryable *attribute* (settings UI, output preference) — it
  does **not** program the PHY. **HPD events** ride kqueue (the same `kevent()`
  as vblank). The compositor issues an **atomic commit** ("CRTC0 → connector
  (USB-C #2) at 1920×1080") — naming the *what*, never the encoder/PHY/link. A
  connector's **mode list is already bandwidth-filtered**, so "does it fit the
  link" is surfaced as "this mode exists or doesn't."
- *Opaque (firmware / vendor display logic — never in the neutral ABI):* the
  **encoder/PHY crossbar assignment**, the **protocol** (TMDS vs DP-link, link
  training, AUX vs DDC, HDCP, CEC, DSC, MST stream allocation), and the **USB-C
  mux / orientation / alt-mode** coordination.

So **"redirect" = an atomic commit re-routing a CRTC to a different connector**,
and the kernel+firmware silently reconfigure the crossbar and bring up the right
protocol. An infeasible config (no free PHY, mode exceeds the link, USB-C not in
DP alt-mode) **fails atomic-check**, and the compositor picks another. No
`if (hdmi) tmds() else dp_train()` ever crosses the neutral boundary — the
display analog of "no command IR in the kernel."

**USB-C needs cross-subsystem plumbing — done the BSD-native way.** A
DP-over-USB-C connector is **virtual until PD activates alt-mode**: (1) cable in
→ the **Type-C/PD driver** negotiates over CC, enters DP Alt Mode, picks
orientation + lane split, configures the **lane mux**; (2) PD publishes a
**kernel hotplug event** ("connector X is now a DP sink + mux config") — the
settled *no-udev, kernel-publishes-events* shape ([[feedback_bsd_devevents_shape]])
— which the display driver subscribes to; (3) the connector materializes (its
HPD asserts), the driver reads EDID over AUX and link-trains on the muxed lanes.
The **mux is owned by the Type-C side, the DP link by the display side** — a
clean capability/ownership split, not a monolith.

**MST makes the connector set dynamic — and reintroduces the scheduler.** A DP/
USB-C port under MST spawns **child connectors** (the chained sinks), each with
its own EDID + CRTC, sharing one physical link's bandwidth by time-division. So
the topology is *dynamic* (a hub plugs in → N connectors appear), and the link
bandwidth is a **shared resource allocated among streams** — another instance of
the federation shape ([[atrium-gpu-scheduler]]): a small bandwidth scheduler on
the DP link, same "shared resource, per-claimant budget" form as the GPU/CPU/
memory controllers.

**What this earns the model (failure-fidelity):** once the encoder/PHY/link layer
exists, the new referee faults are *mode-exceeds-link-bandwidth*, *no-free-PHY
for this assignment*, and *flip to a USB-C connector whose alt-mode isn't active*
— "the compositor requested a route the hardware can't honor," caught
deterministically.

**Scope.** `D-display-1` deliberately keeps **one HDMI-shaped connector** (DDC +
HPD). Interface variety — connector `type` + transport (`Ddc` vs `Aux`), the
encoder/PHY crossbar + a link/bandwidth model, USB-C alt-mode, and MST child
connectors — is the natural *next* display milestone, sitting on this same
deterministic substrate, where the headline new invariants are the three faults
above.

## 9. Energy lands here first

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

## 10. The staged consequence: GPU render-timing is next

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

## 11. Referee invariants (failure-fidelity)

- flip to a non-resident FB faults;
- flip to a BO smaller than the mode faults;
- corrupt / out-of-range EDID parsed into an impossible mode faults;
- a vblank must follow a committed flip (no silent drop);
- tear iff vsync off (vsync-on must produce a uniform per-scanline selection);
- a dropped (superseded) flip is *recorded*, never silent;
- HPD-disconnect mid-scanout is a defined transition (black/last-frame), not UB.

## 12. First milestone — `D-display-1`

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
4. the §11 referee invariants;
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

## 12.5 The policy layer above the engine — the WM/shell is a privileged Insula app (settled 2026-06-15)

The display has the same engine/policy split as audio, and the same answer for
who the policy layer *is*. Mapping (the audio side is settled in
`atrium-lyra-architecture.md` §7/§7.1/§9):

| | Audio | Display |
|---|---|---|
| **Engine** — seat-shared, owns the hardware, **not** an Insula app | lyrad | **Fresco** (composition + window placement / z-order *mechanism*) |
| **Per-session policy** — an **Insula app** with an elevated capability | Choragus | **Forum** (the WM/shell: dock, switcher, focus, session UI) |
| **Normal app** — default-denied the elevated cap | `audio` (own streams) | `graphics = "fresco"` (own windows only) |

The decision: **the Insula app framework applies to everything, the WM/shell
included.** Forum is a normal Insula app — signed manifest (Sigstore), libatrium,
the jail + dedicated-uid + Portcullis trust chain, the per-session lifecycle. It
is *not* special infrastructure. Its privilege is one declared, granted, verified
**capability**, not an escape from the model:

- **`window_management`** — the display analog of `audio_monitor`: default-deny,
  grants **cross-app window enumeration/control + input routing**, held only by
  the trusted session shell. A normal app is structurally isolated — it sees only
  its own windows (`graphics`); Forum is the one component granted the whole-system
  view, exactly as the recorder is the one component granted `audio_monitor`. The
  cap is auditable end to end: declared in Forum's manifest, granted by user/policy,
  verified via `getpeereid` → the launch registry (`portcullis-peer`).
- The **engine** (Fresco) stays out of the app model — it is the seat-shared
  display engine (lyrad's sibling), owning scanout. Fresco executes the placement
  *mechanism*; Forum decides the cross-app *policy* (which surface is focused, the
  overview, the dock) and invokes it — the same mechanism/policy line Choragus/lyrad
  draw for sound.

Consistency notes: this matches `pergola.md` (Pergola the toolkit owns neither
multi-app composition nor placement — "the scene server's WM role"); the
`window_management` capability is **not yet in the `portcullis-toml` schema** (the
concrete follow-on, mirroring how `audio_monitor` was added). Precedent: Wayland
keeps the WM as privileged compositor core; Android makes the shell a
platform-signed privileged *app* — Atrium takes the second and makes the privilege
one auditable capability rather than "the shell is special". See
[[project_atrium_multiuser_seats]], `portcullis.md`, and **`forum.md`** (Forum's
full design — the Atrium-native, apps-have-no-ambient-screen-authority WM model +
the decomposed least-privilege structure).

## 13. Open questions (deferred, not blocking D-display-1)

- **kmod structure** — the §4.1 three-kmod split (pci / gpu / display) is now
  *justified*; pragmatically start the display in-tree and extract the kmod once
  the seam is real (the §4.2 file-split discipline).
- **Output topology (encoder / PHY / interface variety)** — the design is
  settled in §8 (connector `type` + transport, the encoder/PHY crossbar, DP link
  training + bandwidth, USB-C alt-mode cross-subsystem, MST child connectors);
  *collapsed in the model* for `D-display-1` (one HDMI-shaped connector), built
  as the next display milestone. New referee faults then: mode-exceeds-link-BW,
  no-free-PHY, flip-to-inactive-USB-C-altmode.
- **Multi-plane** — overlay + hardware cursor are real hardware compositing
  (an energy lever: skip GPU composition); additive on the per-scanline model.
- **VRR** — additive once the timeline exists (vblank scheduled at the flip,
  clamped to `[min,max]`).
- **Displayed-frame readback ABI** — a debug surface for the in-VM path (the
  engine tests read the integrated frame directly).

## 14. Summary position

The display is a decoupled engine scanning out a VRAM BO shared by fd. The model
that earns it is a **scanline-accurate raster simulator in deterministic
picosecond virtual time** — analytic `beam(t)`, per-scanline integration, exact
tears — fed by a modeled DDC/EDID handshake, asserting tear/stutter/fence
invariants, two-tiered (deterministic engine vs host-timer live path), carrying
gpusim's first energy counters (refresh / PSR / VRR). It is the system's first
timing surface, and it structurally pulls **GPU render-timing** in behind it as
the next milestone.
