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

Design decision recorded; **not yet implemented, and blocked on a
working Tier-3 backend.**

In place: P4 (compositor renders on Tier-2) + PT (cheap small-frame
transport) + the tier-equivalence discipline.

**Tier-3 render path — in progress.** The router only has meaning when
there are *two* working backends to choose between. `aqueduct-gpu-host`
has the software `Tier2Backend` (real) and `MoltenVkBackend` (Tier-3,
Metal via MoltenVK).
- **Tier-3 level-1 DONE (421b67f):** `MoltenVkBackend` is no longer a
  stub — it materialises guest images/buffers as `VkImage`/`VkBuffer`
  and replays the frame op stream as real Vulkan on Metal. A
  render-pass *clear* + image→buffer *readback* run on the Apple M4 Max
  and read back the exact clear colour (the mirror of tier2 level-1).
- **Tier-3 level-2 still pending:** draws / pipelines / SPIR-V→Metal.
  The compositor's rect needs *draw* rendering, so the router is still
  blocked on level-2 — but the GPU command path now exists to build it
  on (no longer a from-scratch stub).

**Next step is completing the Tier-3 draw path** (level-2: `vkCmdDraw`
pipelines + SPIR-V shader modules — MoltenVK compiles SPIR-V→Metal
internally, so this is mostly pipeline/renderpass plumbing, not a
hand-rolled cross-compile). Only then:
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
