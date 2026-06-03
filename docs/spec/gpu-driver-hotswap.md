# GPU driver hot-swap — live render-driver upgrade without reboot

## What this is

Atrium can **unload, upgrade, and re-target the GPU render/compute driver
while the desktop keeps running and the display stays lit** — no reboot, no
display-server restart, no application losing state. Updating the *display*
engine (DCN) is the one case with a brief, state-preserving freeze. The
common case — render/compute driver updates — is fully seamless.

This is a serviceability/availability capability, but it is **not a feature
that was built**: it falls out of the composition of three decisions made
for unrelated reasons (below). It is recorded here because the property is
distinctive and the protocols are worth pinning down before implementation.

### Why this is structurally impossible on a GPU stack bound to one driver

Linux has a DRM/KMS split (display vs. render), but still cannot do this,
for two reasons Atrium resolves:

1. **The render path is bound to the driver at the client/compositor
   level.** Apps pick their Vulkan/GL driver at context creation; the
   compositor holds DRM master. `rmmod amdgpu` returns `EBUSY` while a
   display server runs; `VK_ERROR_DEVICE_LOST` forces every client to tear
   down and rebuild. So a render-driver update means a session teardown.
2. **There is no certified pixel-equivalent CPU substitute** to keep
   rendering on during the swap. `llvmpipe` is a startup choice, not a live
   per-surface migration.

Splitting display from render is necessary but not sufficient; you also
need (2) a certified CPU renderer and (3) client-isolating indirection.

## The three preconditions (each motivated elsewhere)

| Precondition | Built for | Reused here for |
|---|---|---|
| **Tier-equivalence** (Tier-2 SW == Tier-3 GPU pixels, certified) — `energy-policy.md` | the energy router's correctness | a complete functional substitute to render on while the GPU driver is gone |
| **Routing indirection** (clients speak the wire to the daemon; never hold a GPU context/BO directly) — `RoutingBackend` | per-surface energy migration | swapping the driver *under* running clients without them noticing |
| **Three-kmod split** (`pci.ko` / `gpu.ko` render / `display.ko` DCN, independently loadable) — `atrium-gpu-amd-design.md` §4.1 | crash isolation, headless servers, independent evolution | keeping the panel lit (DCN) and the device state alive (PCI) while the render kmod is swapped |

None aimed at live driver hot-swap; together they provide it.

## The layering, and why it's the right way round

The three-kmod split makes **change frequency inversely correlate with swap
cost** — which is exactly correct, because hardware-function stability is
what drives change rate:

| kmod | size | change rate | swap cost |
|---|---|---|---|
| `atrium-gpu-amd-pci.ko` (shared root: BAR/MSI-X/`softc`) | ~500 L | ~never | hardest (both depend on it) — but never needed |
| `atrium-gpu-amd-display.ko` (DCN: modeset + scanout) | ~3–5 K | rare (hardware-stable) | brief held-frame freeze |
| `atrium-gpu-amd.ko` (render/compute: shaders, submission) | ~6–8 K | constant (compiler/perf/security/features) | **seamless** |

The thing you update most often is the thing you can swap most invisibly;
the immovable foundation is the one that never moves. The frequently-cited
real-world reasons to update a GPU driver — shader-compiler fixes, perf
tuning, new compute/API features, command-submission security fixes — are
*all* render/compute concerns, i.e. the seamless case.

## Protocol A — render/compute driver hot-swap (seamless)

`pci.ko` and `display.ko` stay loaded throughout; only `gpu.ko` is swapped.

1. **Drain.** The router force-routes every surface to Tier-2 (a policy
   override: pin home, ignore the cost model). Rendering continues on the
   CPU; the compositor and apps never stop.
2. **Quiesce.** Wait for in-flight GPU work to retire; release the Tier-3
   backend's handles to the render driver.
3. **Swap.** `kldunload atrium-gpu-amd.ko`; install + `kldload` the new one.
   Nothing the user sees was rendered by it — the display (DCN) keeps
   scanning out CPU-composited frames; the device's PCI/BAR/IRQ state
   persists via the still-loaded `pci.ko` + shared `softc`.
4. **Re-certify.** Bring up a fresh Tier-3 backend over the new render
   driver and re-run the differential certifier (`docs/spec/energy-policy.md`
   §"Per-pipeline certification") against it — the new driver is only
   trusted for migration once it is proven tier-equivalent. A pipeline that
   fails stays pinned to Tier-2 (never a wrong pixel from a bad new driver).
5. **Resume.** Surfaces become eligible again and migrate back to Tier-3 as
   the cost model warrants; resources re-materialize on the new backend from
   the residency retain-log (the same replay path single-homing already
   uses).

**Display disruption: zero.** DCN never unloads.

### Scanout source during the window

While the render driver is absent, the CPU compositor writes frames into a
**system-memory framebuffer that DCN scans out from directly** — display
engines generally support system-memory scanout, so no copy engine from the
render kmod is required. (If a deployment scans out only from VRAM, a
minimal DMA path must survive the swap; system-memory scanout is the clean
default.)

## Protocol B — DCN / display driver update (brief, state-preserving)

DCN owns scanout, so updating it is the one case with output discontinuity.
It is a *blip*, not an outage, because the display controller scans out
**autonomously** once its CRTC is programmed — it needs the driver loaded to
*change* configuration, not to *keep* showing a frame.

1. **Quiesce output** — stop updating the scanout buffer (freeze on the last
   frame). The compositor keeps running on Tier-2 and keeps producing frames
   into the framebuffer; only the panel output is held.
2. **Unload DCN *without disabling the CRTC*** — the hardware keeps
   displaying the frozen frame. `pci.ko` stays loaded (device state intact).
3. **Load the new DCN**, which **adopts the running CRTC state (no
   modeset)** — the still-loaded `pci.ko` + shared `softc` make re-attach to
   a live device realistic.
4. **Resume output** — the latest CPU-composited frame is already in the
   buffer, so resume is instant.

**Best case:** a sub-second held-frame freeze — no black flash, no HDMI/DP
re-link. **Worst case** (the new driver needs a clean teardown / different
hardware state): a full modeset — a brief blank + link re-train, ~1–2 s.

**Either way, no application or compositor state is lost** — they ran on
Tier-2 throughout. A DCN update is, at worst, **equivalent to a monitor
resolution change** — a momentary blank everyone already tolerates — except
nothing dies and no work is lost. It is also rare and schedulable (take it
during a natural pause, with a graceful "updating display" hold).

## State-preservation guarantee

Across both protocols: **no client reconnects, no GPU context is recreated
by an app, no rendered work is lost.** Clients hold wire-level resources
(images/buffers/pipelines as `u32` handles), not GPU driver objects; the
router owns the mapping to backends and the residency log owns the means to
rebuild it on a new backend. The *only* thing that can blip is the
pixels-to-panel path during a DCN update — and that holds the last frame.

## Other capabilities the same primitive unlocks

The drain → swap → re-cert → resume primitive also gives, for free:

- **GPU TDR / hang recovery** — render driver wedges → drain to Tier-2 →
  reset the GPU / reload `gpu.ko` → re-cert → resume, instead of the session
  dying.
- **eGPU / GPU hot-plug** — unplug → Tier-2; replug → bring up Tier-3,
  re-cert, resume.
- **Live render-driver development** — iterate on `atrium-gpu-amd.ko` while
  the desktop keeps running on the CPU.
- **A/B driver validation** — bring the new render driver up as a *second*
  Tier-3 and differential-test it live against the incumbent before cutting
  over.

## What implementation requires (none of it new mechanism)

1. **A drain policy override** in the router: force every surface home
   (Tier-2), ignore the cost model, and report when fully quiesced (all
   surfaces pinned, no Tier-3 work in flight). The inverse "resume" lifts
   the override.
2. **A re-certification trigger** on new-backend bring-up: run the
   differential certifier per pipeline and seed the `CertificationRegistry`
   before any surface is allowed back to Tier-3 (replaces the bring-up
   `--trust-tiers` shortcut).
3. **System-memory scanout** kept available to DCN independent of the render
   driver (the default scanout path during a render swap).
4. **Backend hot-attach/detach** on the routing layer: release the Tier-3
   backend cleanly (drain-gated) and attach a fresh one without disturbing
   Tier-2 or client state.

The render-side machinery (tier-equivalence, routing, residency replay,
certification) already exists and is verified; the display-side
(`pci.ko`/`display.ko` independence, CRTC adopt-without-modeset) is the
`atrium-gpu-amd` D5+ bring-up's concern.

## Status

Design recorded; **not implemented.** The composing pieces (energy router,
tier-equivalence cert, single-homed residency) are built and tested
(`docs/spec/energy-policy.md`); the three-kmod split is designed
(`docs/spec/atrium-gpu-amd-design.md`); the live render-driver swap is
demonstrable on the bring-up MoltenVk path (host composites, so Protocol A's
scanout question is moot there) and is a native-D5+ capability on real AMD
hardware. The reframing this records: the router **decouples the OS's
graphics from any single render driver's lifetime**, with the display kept
alive by an independent engine throughout.
