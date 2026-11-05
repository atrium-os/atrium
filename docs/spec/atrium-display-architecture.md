# Atrium display driver architecture — stance & first milestone

**Status:** notes, 2026-06-10. Non-binding; settled in discussion, feeds the
display ABI + the gpusim display model. Companion to the GPU driver stance
[`atrium-gpu-driver-architecture.md`](atrium-gpu-driver-architecture.md), the
ABI [`atrium-gpu-abi-v2.md`](atrium-gpu-abi-v2.md), and the AMD module design
[`atrium-gpu-amd-design.md`](atrium-gpu-amd-design.md). The functional/timing
model is the gpusim repo (separate).

> **Amended 2026-11-03 for GPU ABI v2.1** (`atrium-gpu-abi-v2.md` §7, §6.4,
> §10.2–10.4). Where this document and §7.1 below disagree with the older text,
> §7.1 wins. In short: scanout takes a `bo_fd`/`share_fd`, never a VRAM offset;
> the display is leased per plane; the last frame stays up when the lease holder
> dies; resets and resume re-apply the last committed state and post a reset
> event; output CRC readback is a capability.

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
  *(2.1: the hardware reads by VRAM address, but the ABI never carries one.
  Userspace passes the BO fd; the display module resolves the address itself,
  §7.1.)*

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


### 7.1 v2.1 amendments (normative)

- **Scanout by object.** Each plane in an atomic commit names a `bo_fd` or a
  `share_fd`. The display module resolves it to the BO, checks size, pitch,
  format, modifier and tiling against the plane (GPU ABI §5.12), pins the
  backing while it is latched, and releases it only after it has been flipped
  away (GPU ABI §5.11). `ATRIUM_GPU_IOC_BO_EXPORT_SCANOUT` and every ioctl field
  that carries `{vram_offset, size}` are retired.
- **Per-plane leases.** Opening `/dev/atrium-display<N>` is exclusive to the
  broker (devfs, `EBUSY` on a second open). The broker holds every plane by
  default and may lease a single plane to a separate owner (a fd scoped to that
  plane, with its own fences). This is the hook for a GPU-independent "safe"
  plane (GPU ABI §6.4 rules 5 and 6). The `WM/shell` policy layer of §12.5 is
  unaffected: it drives the broker, not the device.
- **Hold the last frame.** When a lease holder's fd closes, the display module
  keeps scanning out what was last latched on its planes until the next holder
  commits. No modeset happens during the gap (GPU ABI §10.2).
- **Fences.** In-fence and out-fence are `(syncobj_fd, value)` pairs. A commit
  whose in-fence is errored is dropped: the previous buffer stays on screen and
  the out-fence carries the same error (GPU ABI §10.3).
- **Reset and resume.** After a GPU rung-2 reset or a resume, the module
  re-applies its last committed mode and planes. Planes whose BOs were lost show
  the boot framebuffer until the next commit. The display fd posts
  `ATRIUM_DISPLAY_NOTE_RESET` (with the GPU's new lost generation) through
  kqueue. Rung 0 and rung 1 GPU recovery never touch the display block.
- **Output CRC.** Where the hardware computes a CRC of what the pipe actually
  sent, it is exposed as a display capability and read per frame through the
  display fd, so a monitor process can check that what is on screen matches
  what was committed.
- **Header.** The display ABI gets its own `include/atrium/display_abi_v2.h`
  following the same rules as the GPU header (`struct_size` first, `int32_t`
  fds, no implicit padding). Until it lands, `atrium-gpu-amd/atrium_display_abi.h`
  is the intermediate surface.

### 7.2 v2.1 modes, EDID and hotplug (implemented 2026-11-05)

- **Generic above, physical below.** EDID and CTA-861 parsing
  (`atrium-display/edid.c`) and the sink probe (`sink.c`) are shared by
  every display driver. A driver supplies four hooks in
  `struct atrium_display_hw_ops` (`display_hw.h`): `sense` (is a monitor
  there), `read_edid_block` (DDC/I2C, or DP AUX on DisplayPort),
  `program_timing` (its CRTC/OTG registers) and `hpd_rearm` (its
  hotplug interrupt). The AMD module implements them for DCN; another
  vendor's module implements the same four.
- **Mode list.** `ATRIUM_DISPLAY_IOC_CONNECTOR` returns the raw EDID (up to
  512 bytes: base + one extension), every mode it describes (detailed
  timings, then CTA Video Data Block VICs, preferred first, duplicates and
  interlaced dropped; at most 16) and a connector generation.
- **Any mode.** A commit's `mode_index` may name any listed mode; the module
  programs that mode's timing. An index past the list is `EINVAL`. A nonzero
  `mode_generation` that is not the current one is `ESTALE`, so a commit
  built for a monitor that has since been swapped is refused instead of
  lighting the wrong timing on the new one.
- **Hotplug.** The HPD interrupt bumps the generation, drops the cached sink
  and posts `ATRIUM_DISPLAY_NOTE_HOTPLUG` through kqueue (`data` = new
  generation). A registration that names notes in `fflags` hears only those.
- **Policy above the kernel.** frescod lists the modes to the WM
  (`OP_WM_DISPLAY_MODES`), switches on `OP_WM_SET_DISPLAY_MODE` or a
  hotplug (same monitor back: keep its mode; a different monitor: its
  preferred mode or `FRESCOD_MODE`), and broadcasts `EV_DISPLAY_CHANGED`,
  on which forum-wm re-lays out. `forum-ctl modes` / `forum-ctl mode WxH[@Hz]`
  drive it by hand.

### 7.3 v2.2 variable refresh (implemented 2026-11-05)

- **Range.** The shared parser reads a monitor's VRR range from E-EDID 1.4:
  the continuous-frequency feature bit plus a display range limits
  descriptor at least 10 Hz wide (`atrium_edid_vrr_range`). CONNECTOR
  reports it in `out_vrr_min_hz` / `out_vrr_max_hz` with flag `VRR`, and
  `VRR_ACTIVE` while the lit mode runs with it. HDMI's VRR data block and
  DisplayID's adaptive-sync block are not read yet.
- **Turning it on.** `COMMIT_VRR` with `MODESET` runs that mode with
  variable refresh; a `MODESET` without it turns VRR off. `EINVAL` without
  `MODESET`, on a monitor without a range, or for a mode whose refresh is
  outside the range. Commits after that are unchanged: a vsync'd flip ends
  the frame it waits on as soon as the shortest frame (the mode's own
  period) allows, and with nothing to show the monitor refreshes at its
  floor.
- **Driver hook.** `set_vrr(hw, timing, min_hz)` is optional in
  `atrium_display_hw_ops`. AMD programs OTG `V_TOTAL_MIN` (the mode's total)
  and `V_TOTAL_MAX` (stretched to `min_hz`, `atrium_timing_vtotal_at`) and
  selects both in `V_TOTAL_CONTROL`.
- **Policy.** frescod turns VRR on for any mode the range covers
  (`FRESCOD_VRR=0` opts out). No pacing change was needed: it already
  renders on damage and commits when the frame is ready.

### 7.4 v2.3 HDR (implemented 2026-11-05)

- **What the monitor offers.** The shared parser reads the CTA-861.3 HDR
  Static Metadata Data Block (EOTFs, the luminance it wants content
  mastered for) and the Colorimetry Data Block (BT.2020 RGB)
  (`atrium_edid_hdr_caps`). The HDR ioctl (`'D', 67`) reports them as bit
  masks of `EOTF_*` / `COLORIMETRY_*` (SDR and the default always) and
  decoded luminances; CONNECTOR flags `HDR`, and `HDR_ACTIVE` while an HDR
  EOTF is signalled.
- **Signalling.** HDR with `SET` gives the EOTF (SDR, traditional HDR, PQ,
  HLG), the colorimetry and CTA-861.3 static metadata type 1. It takes
  effect with the next successful commit, flip or MODESET, so the monitor
  switches with the first frame rendered for it. `EINVAL` for an EOTF or
  colorimetry the monitor doesn't offer. A MODESET onto a monitor that no
  longer takes the signalled state falls back to SDR.
- **10-bit scanout.** `FORMAT_XRGB2101010` (B 9:0, G 19:10, R 29:20).
  The format is the commit's; HDR doesn't require it, but 8-bit PQ bands.
- **Shared infoframes, driver hook.** `atrium-display/` builds the DRM
  (0x87) and AVI (0x82) infoframes; the optional
  `send_infoframes(hw, frames, n)` hook sends them every frame (n 0 stops).
  AMD writes them to VPG0's generic packets and has DIG0 send them, and
  re-sends them after a device reset. Colour pipeline (degamma, CSC,
  regamma) is left in bypass: the surface's code values reach the monitor.
- **Policy.** frescod stays SDR by default. `FRESCOD_HDR=1` on an HDR10
  monitor switches the stream to HDR10 and re-encodes each frame on the way
  to scanout: sRGB to linear, BT.709 to BT.2020, SDR white at 203 cd/m²
  (BT.2408), PQ into 10 bits. The desktop looks as it did; HDR content can
  later go above white. The gpu renderer draws straight into scanout, so it
  stays SDR. A hotplug re-decides.

### 7.5 v3.0 several monitors (implemented 2026-11-05)

Design: `docs/reviews/2026-11-05-multi-monitor.md`. One display node drives
every output (`ATRIUM_DISPLAY_MAX_OUTPUTS` 8; the AMD driver has 4 DCN pipes);
the 2.x ioctls act on output 0.

- **Seeing them.** OUTPUTS (`'D', 68`) gives every output at a glance: its
  monitor's identity, connector, lit mode and pixel clock, plus the topology
  generation and the display's bandwidth budget and use. CONNECTOR_AT /
  STATUS_AT (69, 70) are CONNECTOR / STATUS for any output; HDR takes an
  `output` (the old `_pad`).
- **Identity.** `atrium_edid_identity`: FNV-1a over the EDID's maker,
  product and serial bytes and its serial-number descriptor; never 0 for a
  valid EDID. The same monitor reads the same on any output and any boot;
  the output's path is mixed in only for monitors with no serial, so two
  identical serial-less monitors are told apart.
- **Atomic commit across outputs.** COMMIT_MULTI (71) takes one commit per
  output. All MODESET / `COMMIT_OFF` (a topology change) or all flips, never
  mixed (`EINVAL`); an output named twice or out of range is refused with
  `FAULT_NO_OUTPUT`. A topology change checks everything before touching a
  register: fences, modes and the bandwidth budget, which it charges for the
  outputs it leaves alone and then each lit output it asks for in order; the
  first past the budget fails with `FAULT_BANDWIDTH`. `out_failed` names
  that part and `out_reason` says why. `TEST_ONLY` stops there. Applying it,
  every named output goes dark, then each is lit; if the hardware still
  refuses one, every output is put back from a snapshot. Flips are per
  output; `EBUSY` if any named output still has one pending.
- **Events.** NOTE_HOTPLUG fires for any output; its data is the topology
  generation, and an output's generation is the topology generation of its
  last hotplug (with one monitor the data equals CONNECTOR's
  `out_generation`, as in 2.x). Vblank notes take output bits
  (`NOTE_OUTPUT(n)`); none means output 0.
- **Policy: frescod.** One desktop, the bounding box of the outputs'
  viewports: left to right by default, remembered per set of monitors by
  identity. Each output shows its part, paced on its own (a slower monitor
  catches up with the newest frame). Hotplugs settle for 500 ms first, and
  a monitor gone for less than 5 s keeps its place and its lit output. When
  the bandwidth runs short the refused output steps down to its next mode,
  the same choice every time, and stays dark only if none fits, with the
  reason logged. With several monitors the gpu renderer gives way to
  Tier-2. `OP_WM_OUTPUTS` tells the WM where each monitor sits;
  `OP_WM_ARRANGE_OUTPUTS` moves them (checked, then remembered for that set
  of monitors).
- **Policy: Forum.** Each monitor is laid out on its own: chrome, bar and
  dock on the primary (the top-left monitor), each window on its home
  monitor, zoom / split / snap per monitor. A window's home is where it
  opened (the monitor of the window you were working in) or where it was
  sent (Super+Shift+Right, `forum-ctl move-monitor`). If its monitor
  leaves, the window shows on the primary; when the monitor comes back, on
  any output, the window goes back. Forum keeps each set's arrangement
  and each monitor's scale per user (`/var/db/atrium/<user>/forum-displays.state`)
  and puts them back when frescod, the WM or the machine starts again.
- **Scale.** Each monitor has a scale (`WmOutput.scale_milli`): its pixel
  density over 110 ppi in quarter steps from 1.0 to 3.0 (EDID physical
  size; 1.0 without one), so a 24" 1080p or a 27" 1440p shows at 1.0 and a
  27" 4K at 1.5. `OP_WM_SET_OUTPUT_SCALE` (`forum-ctl scale N F|auto`)
  overrides it per monitor. A window is told the scale of the monitor it
  mostly covers with `EV_WINDOW_DPI_CHANGED` whenever that changes (moved
  to another monitor, or the monitor's scale set); it renders at that
  scale. Window geometry stays in desktop pixels.

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
