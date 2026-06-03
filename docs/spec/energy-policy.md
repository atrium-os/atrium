# Atrium energy policy — coordinated, not coupled

## Goal

Atrium has more than one energy-aware control point:

- **Laminar** — the kernel scheduler: an RLC closed-loop controller
  that places/parks threads and (phase H) drives CPU DVFS.
- **The Tier-2/Tier-3 renderer router** (planned) — decides whether a
  Vulkan workload runs on the **CPU software renderer (Tier-2)** or the
  **hardware GPU (Tier-3)**. For small ops that finish before the GPU
  leaves its low-power state, staying on the already-warm CPU avoids the
  GPU's wake cost (power-gate exit + DVFS ramp + submit overhead) — a
  net energy win, since damage-driven rendering means *most* frames are
  small.
- Secondary points: display backlight/refresh, the compositor's
  idle/foreground knowledge, the GPU's own power state.

The question this doc settles: **should these share one energy policy,
or have independent knobs?**

The decision: **energy *intent* permeates every layer; energy
*mechanism* stays local to each layer.** Layers are *coordinated* by a
shared, slow, read-only policy + a few shared facts — they are **not
coupled** into one controller, and no layer's control loop reacts to
another's outputs. "Coordinated, not coupled."

## Why not couple the control loops

1. **Two reactive loops oscillate.** Laminar is a closed-loop
   controller; the tier router is also a feedback loop (route → load /
   heat shift → re-route). If each reacts to the other's *output* you
   get nested-loop instability: the router sends work to a cold CPU →
   load climbs → Laminar unparks cores / ramps DVFS → the CPU now looks
   fast+warm → the router sends *more* → … ; or, inversely, Laminar
   parks for battery → the router sees a thin CPU → wakes the GPU → the
   exact wake it was trying to avoid. Hunting between tiers is worse
   than any single steady state.

2. **Timescales don't match.** DVFS / placement act at µs–ms (per
   tick). Tier routing acts per-op / per-frame (~8–16 ms @ 120 fps).
   GPU residency hysteresis is tens of ms. Coupling control across
   mismatched timescales is where ringing lives. The stable shape is a
   *slow* shared mode with *fast* local loops.

3. **The Laminar charter forbids it.** Laminar's design discipline is a
   single cost function + RLC closed loop, kept clean enough to be the
   **upstream FreeBSD** scheduler; GUI/renderer behaviour belongs in
   downstream variants, not in the scheduler's cost function. Baking
   renderer policy into Laminar would (a) fragment the RLC story with a
   GUI-specific term, and (b) make Laminar non-upstreamable and
   Atrium-coupled. **The scheduler must not know the renderer exists.**

## The steelman for coupling — and how to capture it cleanly

There *is* a real global view: on an SoC, CPU and GPU share one power
rail and one thermal budget, so "do it on CPU vs wake the GPU" is
genuinely a joint optimization against a shared envelope (this is what
centralized SoC power managers do). That truth does **not** imply
coupled control loops. Capture it with a thin **energy-policy
authority** that *allocates the shared budget as a slow, one-directional
constraint* (sets the mode + hands each layer a thermal/power headroom);
each layer then optimizes **locally** within it. Budget-setting is slow
and unidirectional, so it preserves stability and modularity while
honouring the shared-resource reality.

## The interface

### Shared — read-only, slow (the "permeation")

A small system energy surface that every layer *reads* (transport per
signal — see **Transport** below):

- **Policy mode** — `perf | balanced | battery`, derived from AC state,
  thermal state, and user/session intent (idle, foreground app). Every
  layer pulls the same direction off the *same* mode — battery ⇒
  Laminar biases parking, the router prefers Tier-2-when-cheap, the
  display dims — *coherently*, because they read one mode, not because
  they are wired to each other.
- **Thermal / power headroom** — the budget the authority allocates;
  the constraint each local optimizer respects.
- **GPU power / residency state** — *the* cross-layer fact the router
  needs and that nothing exposes today: is the GPU hot, and where is its
  power-down hysteresis. Published by the GPU stack (kmod /
  aqueduct-gpu), read by the router.

Optionally, a **one-way hint** renderer → energy authority ("GPU work
incoming / idle"), so the authority can inform CPU/GPU budget — never
renderer → scheduler directly.

### Private — each layer's own mechanism + knobs (untouched)

- **Laminar** keeps its RLC internals + DVFS/parking knobs. It never
  sees a frame, a window, or a tier. It may *read* the policy mode as a
  bias input (a single scalar), nothing more.
- **The router** keeps its per-op cost model + the Tier-2/Tier-3
  decision, gated on the shared GPU-state + mode. Its control variable
  is the **frame deadline** (8.33 ms @ 120 fps), local to the renderer.
- **Display / compositor** keep their own backlight/refresh/idle knobs.

### Precondition: tier-equivalence

Routing is only safe because a routed op is **pixel-identical on either
tier** — the BGRA/sRGB convention work + the differential-test
discipline (Tier-2 ≈ Tier-3 ≈ interpreter). Policy can move work freely
between tiers *because* correctness does not depend on where it lands.
Tier-equivalence is therefore a hard invariant for any energy routing,
not a nice-to-have.

## Acting on the verdict — routing mechanism & granularity

The router *decides* per frame (the `FrameRouter` scores each frame's
Tier-2 vs Tier-3 cost). But **acting** at per-frame granularity is the
wrong mechanism, and the distinction is the central design decision here.

**Rejected: per-frame dual-hot dispatch.** Naively, the decorator would
hold *both* backends, mirror every resource-creation op to both (so either
can render any frame), and send each frame's `submit_frame` to the tier
the router picked. Two fatal costs:

1. **It fights the hysteresis we model.** The whole `GpuPowerModel` exists
   because the GPU has a tens-of-ms residency cost. Flipping tiers per
   frame (8–16 ms) is exactly the chatter the residency timer + the
   router's deadband are meant to *prevent*. Per-frame dispatch re-creates
   the disease the model diagnoses.
2. **Every resource lives twice.** Mirroring uploads/pipeline-compiles to
   both backends doubles memory + bandwidth + compile time for resources,
   most of which only ever render on one tier. The cost is paid per
   resource; the benefit (instant per-frame switch) is one we just argued
   against wanting.

**Chosen: per-surface assignment with slow migration.** Each Fresco
surface/window is *assigned* to one tier. Its resources live only on that
tier's backend. The router's per-frame verdict is not a dispatch — it is a
**vote**, EWMA-smoothed per surface. A surface **migrates** to the other
tier only when its smoothed verdict crosses the (already-built) hysteresis
band, at the residency timescale — not per frame. This:

- matches the timescale the GPU power model already describes (migrate
  slowly, the way the hardware itself transitions power state);
- keeps each surface's resources single-homed — migration re-creates them
  on the destination once, amortized over the many frames before the next
  migration;
- reuses the `FrameRouter` verbatim as the per-frame scorer; only the
  *consumption* of its verdict changes (accumulate, don't act).

**The dispatch object.** A `RoutedSurface` owns `{ tier, backend,
resource_set, smoothed_verdict }`. A surface's `submit_frame` goes to its
assigned backend; its readback comes from the same backend (rendered
output is single-homed, so there is no "which tier rendered this?"
ambiguity). On migration, the surface's retained resource set is replayed
to the destination backend, then the assignment flips.

**Tier-equivalence is the gate, per pipeline.** A surface may migrate only
if its pipelines are *certified* tier-equivalent by the differential
harness (Tier-2 ≈ Tier-3 ≈ interpreter). An uncertified pipeline pins its
surface to one tier — routing degrades to "stay put," never to a wrong
pixel. Certification is per-pipeline and cached.

**Where it lives.** A routing layer in `aqueduct-gpu-host` that owns both
`Tier2Backend` and `MoltenVkBackend` and consumes the `FrameRouter`
verdict. The `CostModelBackend` decorator stays purely observational (it
already scores + tallies + calibrates); acting is a *separate* layer so
the model and the mechanism don't entangle — the same "coordinated, not
coupled" discipline applied one level down.

**Staged rollout.**
1. *Observe per surface* — extend the per-frame verdict to a per-surface
   EWMA of votes (the `FrameRouter` already emits the per-frame verdict;
   add the surface-keyed smoothing + a migration-threshold log). Still
   zero dispatch change.
2. *Mechanism behind a flag* — `RoutedSurface` + migration, gated, and
   validated against the differential harness on a known tier-equivalent
   pipeline before it is allowed to migrate anything.
3. *Default per surface* — once equivalence certification is wired and the
   migration cost is measured to amortize, routing acts by default; the
   mode signal biases the migration threshold.

## Single-homed resource residency (the discrete upload win)

`RoutingBackend` as built **mirrors** resource creation/upload to both
backends — the simplest correct mechanism (correctness comes from slow
migration, not from how resources are homed). On UMA that is free (one
physical RAM). On a **discrete** part it is the very waste the router
exists to eliminate: a CPU-routed surface still DMAs its textures/buffers
to VRAM over PCIe, burning the bandwidth + energy the routing decision was
trying to save. The fix is to home each surface's resources only on the
tier it runs on, and to materialise them on the other tier *only* if it
migrates there.

**Mechanism: deferred materialisation.** Instead of forwarding a resource
op to both backends, `RoutingBackend` *records* it and forwards it only to
tiers already **live** (materialised). A tier goes live the first time a
frame dispatches to it, at which point the recorded ops are replayed to it.
Net effect: a tier that is never used receives **zero** uploads — the
common always-CPU case uploads nothing to the GPU.

**The load-bearing decision: how to replay on migration.** Two ways to make
a resource present on a newly-activated tier, with opposite cost profiles:

- *Retain-log* — keep the ordered op log (including upload bytes) and
  replay it. Simple and exact, but it **pins the upload data in host
  memory** for as long as a tier might still activate. In the always-CPU
  case (the case we optimise for) the GPU never activates, so the log is
  retained forever → an unbounded memory cost that scales with total
  texture/buffer bytes. Unacceptable as-is.
- *Readback-on-migrate* — retain only resource *identities* + metadata
  (cheap); when a tier activates, reconstruct each resource by reading its
  current state back from the already-live tier and uploading to the new
  one. No upload bytes retained, but migration pays a readback (the
  asymmetric direction on discrete) and needs readback support for every
  resource kind.

**Recommendation:** *retain-log, but drop it the moment both tiers are
live* (then no replay can ever be needed), **and bound it** — once a tier
is live, collapse superseded writes (keep only the latest state per
resource, not the full history). In the always-CPU steady state only one
tier is live, so the log is retained — but bounded to *current* resource
state, not history, which is the irreducible cost of being able to migrate
at all. Readback-on-migrate is the fallback if even bounded retention is
too much memory; it trades memory for a one-time migration readback, which
the slow-migration cadence already amortises.

**Frame resource-set introspection — and why coarse granularity fails.**
To materialise a frame's resources on its dispatch tier, the router must
know which resources the frame touches. The frame walk already sees the
directly-referenced ones (render-target image, pipelines, copy-dst
buffers); indirectly-referenced ones (sampled textures, vertex/uniform
buffers bound via separate commands) are not all visible at this layer.

The tempting simplification — *whole-world-per-tier*, materialise the
entire resource set the first time a tier is used — **does not actually
deliver the win, because of certification.** Certifying a pipeline renders
a probe on *both* tiers (that is how tier-equivalence is proven). Under
whole-world materialisation, that first probe makes *both* tiers live and
replays *everything* to the GPU — so the moment any pipeline is certified
(i.e. the moment any surface becomes eligible to migrate), single-homing is
gone for the whole world. The surfaces that can migrate are exactly the
ones coarse residency fails to keep single-homed.

So **per-resource residency is required, not optional.** Each resource
tracks which tiers it is resident on; a frame (or the certification probe)
materialises only *its* resources on the target tier. Certification uses a
tiny synthetic probe whose resources are its own — proving a pipeline
equivalent must not drag a surface's textures onto the GPU. This needs the
frame's resource closure to be introspectable (the directly-referenced set
is a start; sampled/bound resources need the bind commands tracked or the
wire to carry the set). That introspection is the substantive work, and the
reason this is its own effort rather than a tail increment.

**Invariant:** materialisation must complete *before* the frame is
dispatched to a tier — a draw that samples an unmaterialised texture is a
wrong pixel, the one thing routing must never produce. So `submit_frame`
calls `materialise(tier)` before `t{2,3}.submit_frame`.

## Transport

The signals cross the **kernel ↔ userspace boundary** in both
directions, so the channel is a deliberate hybrid: **Aqueduct for the
userspace coordination tier, sysctl (+ the atrium-gpu ABI) at the kernel
edge.** Laminar is kernel-resident and cannot be an Aqueduct client, so
Aqueduct alone can't carry everything.

| Signal | Source → consumer | Channel |
|---|---|---|
| Policy mode / headroom → **Laminar** | authority (userspace) → scheduler (kernel) | **sysctl** (e.g. `kern.sched.laminar.energy_mode`) — one scalar, read in-kernel as a bias |
| Policy mode / headroom → **router, compositor, display** | authority → userspace layers | **Aqueduct** — a small `energy` dictionary, async-event broadcast (fan-out, like fresco-protocol's ASYNC_EVENT) |
| **GPU power / residency state** → router | kmod (kernel) → router (in `aqueduct-gpu-host`) | **atrium-gpu ABI** (the `/dev/atrium-gpu0` cdev the daemon already holds) or `hw.atrium_gpu.N.power_state` sysctl — read locally, no extra hop |
| GPU state / AC / thermal → **authority** | kmod + ACPI/sensors (kernel) → authority (userspace) | **sysctl** (+ ABI) |

Why the split:

- **Laminar is in the kernel.** Making the scheduler an IPC client is a
  layering violation and a non-starter for an upstreamable FreeBSD
  scheduler. sysctl is the native, zero-dependency kernel knob Laminar
  already uses (`kern.sched.ctrl_enable`); reading *one scalar mode*
  keeps it within the single-cost-RLC charter (a bias input, not a
  renderer term).
- **The router already owns the GPU fd.** GPU power state is a kernel
  fact the `aqueduct-gpu-host` daemon can read directly over the
  atrium-gpu ABI — don't bounce it out to a userspace service and back.
- **The transport shape enforces "coordinated, not coupled."** Both
  channels are inherently *fan-out, read-only*: sysctl is
  write-by-authority / read-by-others; Aqueduct async-events broadcast
  from the authority. Neither lets a layer subscribe to another layer's
  *instantaneous control output* — the exact property that keeps the
  loops from coupling. The transport mechanically prevents the failure
  mode the policy forbids.

The **energy-policy authority** is the single userspace Aqueduct service
that bridges both transports: it reads kernel sensors + GPU state
(sysctl / ABI), computes mode + headroom, then **writes the Laminar
sysctl** *and* **broadcasts over Aqueduct**. One source of truth, fanning
out across both channels.

## Ownership

| Concern | Owner |
|---|---|
| Policy mode + budget allocation | energy-policy authority (userspace service; name TBD) |
| CPU placement / parking / DVFS | Laminar (kernel) — reads mode as a bias only |
| Tier-2 vs Tier-3 routing | renderer router — reads mode + GPU state |
| GPU power/residency state (publish) | aqueduct-gpu / kmod |
| Backlight / refresh / idle | display + compositor |

No box drives another box's loop. Arrows are read-only signals + slow
budget constraints, fanning *out* from the authority.

## Phasing

1. **Read-only signals first.** Publish the policy mode + GPU
   power/residency state. Let the router make purely *local* decisions
   against them. Prove the router (does it actually cut GPU wakes on the
   damage-frame workload without hunting?). This is the smallest step
   that delivers the energy win and validates the model.
2. **Mode as a Laminar bias** — only a single scalar input to the
   existing RLC loop, never a new cost-function term.
3. **Budget-allocating authority** — add *only if* measurements show
   real CPU↔GPU budget contention (shared thermal envelope being
   fought over). Even then it sets constraints; it does not drive loops.

Do **not** build the global broker first — that is the over-coupled
trap. The default is independence with shared read-only intent; tighten
toward a budget authority only when data demands it.

## Non-goals / guardrails

- **No renderer term in Laminar's cost function.** Laminar stays
  upstreamable + single-cost-RLC. Renderer/GUI energy logic lives in
  Atrium layers.
- **No bidirectional control between layers.** Signals fan out from the
  authority; layers do not read each other's instantaneous outputs.
- **No routing without tier-equivalence.** If an op can't be shown
  identical across tiers, it is pinned to one tier, not routed.

## Status

**The local router is built, acting, productionised, and verified on
hardware — observe → decide → gate → certify → dispatch all implemented
(`aqueduct-gpu-host`). What remains is two scoped optimisations, not new
design.**

The cost model (`CostModelBackend`, `docs/spec/gpu-device-model.md`):
per-op transfer + Layer-2 exec roofline, IR-based shader cost via
`atrium-spv-ir`, `DeviceProfile` (UMA vs discrete) + `CalibrationProfile`
(3 measured efficiency scalars, fit online against `VkQueryPool` GPU time,
per microarch family — not per chip), and the discrete-topology terms two
design questions forced: an **asymmetric** migrate-to-GPU transfer cost
(revert is ~free — re-render from the host-memory originals) and a
**`ScanoutDomain`** present cost (CPU content reaches the dGPU's display on
the cheap copy/DMA path, *not* a compute wake — `GpuPowerModel` models the
expensive shader-array domain only).

The router (`router.rs`): `tier2/tier3_exec_cost`, the per-mode `route()`,
the `Router` hysteresis band, the `GpuPowerModel` residency signal, and the
`FrameRouter` per-frame verdict; then the *acting* layer — `SurfaceRouter`
(EWMA-smoothed per-surface vote → slow migration at the residency
timescale, asymmetric on discrete), the `CertificationRegistry` + gate, and
`RoutingPolicy` (gated effective tier + single-homed readback).

Acting (`routing_backend.rs` + `certify.rs`): `RoutingBackend` owns a real
Tier-2 (`Tier2Backend`) and Tier-3 (`MoltenVk`) backend, dispatches each
surface's frames to its assigned tier, and routes readback to the tier that
rendered it. `differential_certify` renders a probe on both backends and
compares — a pipeline migrates only once **certified** tier-equivalent;
uncertified pipelines pin their surface (degrade to "stay home", never a
wrong pixel). The verdict cost is paid only where it can pay off: pinned
surfaces and surfaces long-settled on the CPU are dispatched without
re-scoring (`decision_stats()` makes the overhead observable).

Live in the daemon, all opt-in, both the Unix-socket and Carillon (VM)
transports: `--device-profile` (model in the data path), `--route` (score +
tally, observational), `--calibrate` (online fit vs measured GPU time), and
`--backend routing` (the router *acting* over Tier2Backend + MoltenVk).

**Verified on Apple M4 Max:** the tier-equivalence convention precondition
holds empirically — Tier-2 (software) and MoltenVK render pixel-identical
output across the convention-risk colour space (pure channels → no BGRA
swap, 0/255 extremes → no clamping divergence, partial alpha → no
premultiply mismatch). `tests/cross_tier_certify.rs`.

**Remaining (scoped optimisations, no new design):**
1. **Single-homed resource residency.** `RoutingBackend` currently mirrors
   resource creation/upload to *both* backends (simplest correct mechanism;
   correctness comes from slow migration, not homing). The optimisation is
   lazy per-tier residency + replay-on-migration, so a CPU-routed surface
   never uploads to VRAM — directly an energy win on discrete. Its own
   effort (resource-residency tracking + frame resource-set introspection).
2. **Shaded per-pipeline certification.** The flat-colour convention is
   verified; full equivalence through interpolated varyings / `gl_FragCoord`
   is the `atrium-spv-differential` harness's domain (Tier-2 does not yet
   wire `FragCoord`). A rendering-correctness effort, not a router change.

**Tier-3 render path — in progress.** The router only has meaning when
there are *two* working backends to choose between. `aqueduct-gpu-host`
has the software `Tier2Backend` (real) and `MoltenVkBackend` (Tier-3,
Metal via MoltenVK).
- **Tier-3 level-1 DONE (421b67f):** `MoltenVkBackend` is no longer a
  stub — it materialises guest images/buffers as `VkImage`/`VkBuffer`
  and replays the frame op stream as real Vulkan on Metal. A
  render-pass *clear* + image→buffer *readback* run on the Apple M4 Max
  and read back the exact clear colour (the mirror of tier2 level-1).
- **Tier-3 level-2a DONE (60002aa):** real graphics-pipeline DRAW.
  `MoltenVkBackend::draw_and_copy` builds a Vulkan pipeline from VS+FS
  SPIR-V (MoltenVK → Metal), records a render-pass clear + `vkCmdDraw` +
  image→buffer copy, and reads back the rendered colour on the M4 Max
  (`draw_triangle_through_metal` test). The hard "can Tier-3 draw?" is
  answered — yes — and the pipeline/render-pass/framebuffer/shader-module
  helpers exist.
- **Tier-3 level-2b-i DONE (0670cfb):** the full FrameOp draw replay
  runs through `submit_frame` — a registered pipeline +
  BeginRenderPass/BindPipeline/Draw/EndRenderPass/CopyImgToBuf renders
  on Metal (`frameop_draw_replay_through_metal`). `submit_frame` is now
  a real render-pass replay (per-frame render pass + framebuffer +
  dynamic viewport/scissor + `vkCmdDraw`); `create_pipeline` registers
  pipelines from SPIR-V. This is the exact interface the daemon will
  drive.
- **Tier-3 level-2b-ii DONE (031b0f4):** the daemon now routes
  `OP_GPU_PIPELINE_CREATE` to the hardware backend. A `Backend
  ::pipeline_created(id, vs, fs)` hook (default no-op) + session SPIR-V
  retention (`ShaderRecord.spirv`) + handle_pipeline_create routing →
  `MoltenVkBackend::create_graphics_pipeline`, which stashes the SPIR-V
  and materialises the `VkPipeline` lazily at first draw (format from
  the render target — the format-propagation crux). 93/93 daemon lib
  tests + 81/81 smoke (tier2 unaffected).
- **Tier-3 level-2b-iii still pending:** daemon backend *selection*
  (run with `MoltenVkBackend`) + a graphics-draw app to exercise
  ICD→daemon→Metal end-to-end (the compositor's rect uses compute too,
  so a pure-graphics app is the cleaner first end-to-end).

**Next step is level-2b-iii** (backend selection + end-to-end draw via
the ICD). Only then:
- measure the crossover (submit/latency on Tier-3 vs CPU cost on
  Tier-2; note wall-clock on a warm host GPU captures the
  *submit/latency* component but not silicon *wake* energy — that
  needs real hardware power measurement / a cold GPU);
- then phase 1 (publish GPU power state + mode, local router decision)
  becomes implementable + provable.

Building the router or a crossover bench *before* Tier-3 renders would
be measuring/routing against a no-op stub — explicitly out of scope.

See `docs/spec/tier2-renderer-perf.md` (router framing),
`docs/spec/aqueduct-gpu.md` §6.5 (tier-3), and the Laminar scheduler
design for the mechanisms this coordinates.
