# GPU Device Model — UMA/PCIe cost modeling + virtual Tier-3 timing

> **Status.** Design. A measurement/simulation instrument layered into
> `aqueduct-gpu-host`: it charges every GPU memory + execution op what it
> *would* cost on a chosen device topology (UMA APU vs discrete PCIe
> dGPU), so the OS can do throughput / latency / energy analysis and the
> energy router can be built and validated **without owning the
> hardware**. Data still moves zero-copy through Carillon
> (`carillon.md`); this layer models the *cost* of that movement, it does
> not move data differently.
>
> **Companion docs.** `aqueduct-gpu.md` (the FrameOp wire + the `Backend`
> trait this decorates), `carillon.md` (the VM transport whose ops get
> costed), `energy-policy.md` (the router + power signals this feeds),
> `tier2-renderer.md` (the functional executor that, plus this model,
> becomes a *virtual Tier-3*).
>
> **Naming.** TBD — *do not adopt a name in code until the user picks.*
> This is an Atrium-runtime instrument (lives in `aqueduct-gpu-host`), so
> it *does* fall under the Latin / classical-architecture convention.
> Candidates, all Roman measuring/surveying instruments: **Chorobates**
> (the Roman levelling instrument used to measure the *gradient of
> aqueducts* — thematically exact: it measures flow/cost along the
> Aqueduct), **Groma** (the surveyor's sighting cross), **Gnomon** (the
> shadow-caster of a sundial — a pure measuring element). Placeholder in
> this doc: **"the device model."**
>
> **One-line summary.** A `Backend`-decorating cost model + selectable
> `DeviceProfile`s that turn the zero-copy Carillon/socket path into a
> *what-if device*: it answers "what would this frame cost on an M4-Max
> APU vs an RDNA3 card over PCIe 4.0?" in two modes — **accounting**
> (default; record modeled time/energy, zero perturbation) and
> **shaping** (opt-in; inject the modeled latency so the whole OS *feels*
> the device). The functional pixels come from the real backend
> (MoltenVk / Tier-2); the *timing + energy* come from the model.

---

## 1. Motivation — and the reframe that makes it cheap

The ambition is "a full GPU simulator so we can test Tier-3 without
hardware." That ambition contains **three distinct grails**, and
separating them collapses most of the cost:

| Grail | Question it answers | Needs register accuracy? | Status |
|---|---|---|---|
| **1. Render correctness w/o HW** | "right pixels?" | No | **Already solved.** Tier-2 executes our SPIR-V and produces correct pixels; tier-equivalence (`energy-policy.md`) guarantees they match Tier-3. |
| **2. Perf / energy fidelity w/o HW** | "what would this *cost* on device X?" | **No** — needs a *cost/timing model* | **This document.** Buildable now, Atrium-authored, analytic. |
| **3. Driver / kernel testing w/o HW** | "does the D5+ kmod drive the rings/registers correctly?" | **Yes** | Deferred. Appendix A — the multi-year piece, justified by *driver* bring-up, not Tier-3 rendering. |

The crucial observation: **Tier-2 is already a functional GPU simulator
at the SPIR-V level.** Register accuracy buys nothing for getting the
right pixels. What's missing — and what unlocks "measurement/tweak
capabilities in the OS" — is *timing and energy* fidelity. That is a
parameterised cost model, not a register-accurate device. So the
high-value 80% (Layers 1–2) is cheap and permissive; the register-
accurate 20% (Layer 3) is a separate, deferred grail.

**The immediate unlock:** the energy router is blocked on "a working
Tier-3 path to measure the Tier-2↔Tier-3 crossover" (`energy-policy.md`
blocker). The device model gives that crossover **analytically, for any
profile, today** — build and validate the router against modeled devices
now, calibrate constants against real MoltenVk/HW later.

---

## 2. Architectural placement

The model is a **decorator backend** — `CostModelBackend<B>` wraps any
real `Backend` (`aqueduct-gpu-host/src/backend.rs`). It is
**transport-agnostic**: it sits above the transport, so it costs ops
arriving over Carillon (the VM path) *or* the Unix socket (host dev)
identically.

```
  Session (FrameDecoder, resource tables)        ── unchanged
        │  Backend trait calls
        ▼
  ┌──────────────────────────────────────────────┐
  │ CostModelBackend<B>  (this doc)                │
  │   • DeviceProfile (UMA | discrete | …)         │
  │   • per-op cost(time, energy) via the model    │
  │   • accounting: record to FrameLedger          │
  │   • shaping (opt-in): defer completion by Δt    │
  └───────────────┬────────────────────────────────┘
                  │ forwards every call verbatim
                  ▼
  ┌──────────────────────────────────────────────┐
  │ B = real backend: MoltenVkBackend | Tier2Backend│  ← produces pixels
  └──────────────────────────────────────────────┘
```

It overrides the `Backend` ops that have a transfer/exec cost —
`buffer_created` (upload), `buffer_read_bytes` (readback),
`image_created`, `pipeline_created`, `submit_frame` (execution + the
render-pass's memory traffic) — computes a modeled `(time, energy)`,
records it, optionally delays, then **forwards to `B`** so the real
pixels are still produced. Functional behaviour is identical with the
decorator present or absent; only the ledger (and, in shaping mode, the
timing) changes.

Selected by daemon flags:
`--device-profile <name>` (default `passthrough` = zero modeled cost),
`--cost-mode accounting|shaping` (default `accounting`).

---

## 3. Layer 1 — transfer cost model (UMA vs PCIe)

### 3.1. DeviceProfile

A profile is data (TOML), seeded from public specs. It captures the
*topology* (the thing that makes UMA and discrete fundamentally
different) and the link/memory parameters.

```toml
[profile.uma-apple-m4-max]
topology       = "unified"     # one coherent pool, zero host↔device copy
mem_bw         = 546e9         # B/s  (M4 Max published UMA bandwidth)
mem_latency    = 100e-9        # s    (effective load latency)
coherent       = true
host_link_bw   = 0             # n/a — no PCIe hop
clock          = 1.4e9         # modeled GPU clock (Hz)
alu_lanes      = 5120          # modeled FP32 lanes
# energy
pj_per_byte_mem   = 8          # DRAM access energy
gpu_active_w      = 35
gpu_idle_w        = 0.4

[profile.discrete-rdna3-pcie4x16]
topology       = "discrete"    # separate VRAM; explicit copy across link
vram_bw        = 960e9
vram_latency   = 250e-9
host_link_bw   = 28e9          # PCIe 4.0 x16 effective (≈ raw 31.5e9)
host_link_lat  = 1.5e-6        # DMA setup + ring submit
host_visible   = "rebar"       # full | rebar(256MB+) | bar1-small | none
copy_engines   = 2             # async DMA queues → transfer/compute overlap
clock          = 2.5e9
alu_lanes      = 12288
pj_per_byte_link  = 60         # PCIe transfer energy (rough)
pj_per_byte_mem   = 5          # GDDR6 access energy
gpu_active_w      = 320
gpu_idle_w        = 8          # discrete cards have a real idle floor
```

The set ships with a small library of profiles (Apple M-series,
RDNA2/3 discrete, an APU like RDNA2-integrated, plus `passthrough` and a
deliberately-slow `pcie3-x8` for stress).

### 3.2. The analytic model

Bulk transfer is well-modelled by LogGP / roofline:
`t_transfer = fixed_latency + bytes / bandwidth`. Topology decides which
bandwidth and *whether a copy happens at all* — this is the whole point:

- **Unified (UMA/APU):** an "upload" is a coherent write into the shared
  pool — no link copy. Cost = `bytes / mem_bw` (often hidden by the
  producer's own store). A "readback" is a coherent read — `bytes /
  mem_bw + mem_latency`, no round trip. *This is why offloading small ops
  to an APU is cheap* — the property the router must exploit.
- **Discrete:** an "upload" is a host→VRAM DMA over the link:
  `host_link_lat + bytes / host_link_bw`. A "readback" is the asymmetric
  killer — VRAM→host over the link *again* (`host_link_lat + bytes /
  host_link_bw`), unless the target is in a host-visible BAR
  (`host_visible="full"|"rebar"` and the region fits), in which case it's
  a mapped read at `vram_bw`. *This asymmetry is why discrete GPUs punish
  readback-heavy / small-op workloads* — exactly the crossover the router
  needs to see.
- **Overlap:** with `copy_engines > 0`, transfers of frame N+1 overlap
  execution of frame N; the model exposes both the *serial* cost and the
  *pipelined* (steady-state) cost so analysis isn't misled by either.

Execution cost (the frame's own GPU work) is Layer 2 (§4); Layer 1 also
charges the **memory traffic of a `submit_frame`** (framebuffer
read/write bandwidth) since that's topology-sensitive too.

### 3.3. Two modes

**Accounting (default).** Each costed op appends a record to a
`FrameLedger`; the real completion fires on the real backend's schedule,
unperturbed. Zero timing distortion — the daemon behaves exactly as
today, plus a ledger. This is what the router queries and what offline
analysis consumes. Robust and always-on-able.

**Shaping (opt-in, `--cost-mode shaping`).** The decorator *defers the
completion doorbell* (Carillon `notify_peer`, or the socket reply) by the
modeled `Δt`, so the guest/client — and therefore Laminar's pacing, the
compositor's frame budget, app responsiveness — experience the modeled
device. Boot the OS "as if on an RDNA3 card over PCIe 4.0 x8" and watch
the whole stack react. **Honesty caveat:** shaping injects a single
lumped delay per op; it cannot faithfully reproduce fine-grained
async copy/compute *overlap* or queue contention (it approximates them
via the §3.2 pipelined term). Shaping is for *coarse "feel" and
closed-loop behaviour*, not cycle claims; precise numbers come from
accounting + calibration (§6). Hence accounting is the default.

### 3.4. Energy term

Every cost record carries energy alongside time:
`E = bytes·pj_per_byte_link (if a link copy happened) + bytes·pj_per_byte_mem
+ gpu_active_w · t_exec + gpu_idle_w · t_idle`. This is the term
`energy-policy.md` wants for the router's local decision and for the
read-only **GPU power/residency signal** — in simulation, the model
*drives* that signal (the modeled device "wakes" on first GPU op, idles
after a hysteresis window), so the router and the (future) budget
authority can be exercised against a realistic power curve with no GPU
attached.

### 3.5. Output — the ledger

Per-frame `FrameLedger`: per-op `(kind, bytes, topology-path, t_model,
e_model)`, plus frame totals (transfer time, exec time, energy,
serial-vs-pipelined). Emitted as (a) a live counter stream for dashboards
/ the router, and (b) a JSONL trace for offline analysis and CI
regression (`bench_*` harnesses, like the Tier-2 perf work).

---

## 4. Layer 2 — execution timing model (virtual Tier-3)

Layer 1 costs *transfers*; Layer 2 costs the *work*. For each draw /
dispatch, estimate execution time from a roofline:
`t_exec = max( flops / (alu_lanes·clock·2) , bytes_touched / mem_bw )`,
refined by an occupancy factor (waves in flight vs latency to hide). The
op's `flops`/`bytes_touched` come from the SPIR-V the daemon already has
(instruction mix is cheap to extract once at pipeline-create; per-draw
scales by invocation count / pixel count).

Combined with Tier-2 as the functional executor:

> **virtual Tier-3 = Tier-2 (correct pixels) + device model (timing +
> energy).**

Because tier-equivalence guarantees Tier-2 ≡ Tier-3 pixels, you get
**differential testing for free**: run a frame through Tier-2 (truth) and
through the model (cost); a divergence in pixels is a tier-equivalence
bug, a divergence in cost-vs-real-HW is a calibration miss (§6). This is
the elegant unification — it's ~80% built already (the SPIR-V executor
exists; the model is the new part).

---

## 5. What it unlocks

- **The energy router, now.** Tier-2↔Tier-3 crossover becomes a model
  lookup over the active profile — build/validate the router before any
  real Tier-3 HW is in hand; calibrate later.
- **A/B device exploration.** Same OS, same workload, swap
  `--device-profile`: quantify "how would Atrium feel/perform/draw-power
  on an APU vs a discrete card vs a slow PCIe 3.0 link?" — design-space
  exploration with no procurement.
- **Regression guard.** CI asserts modeled frame cost/energy stays within
  bounds per profile; a change that doubles readback traffic shows up as
  a ledger regression even on CPU-only CI.
- **Energy-policy realism.** Drives the GPU power/residency signal in
  simulation so the whole "coordinated, not coupled" machinery is
  exercisable headless.

---

## 6. Calibration & fidelity (honesty section)

Analytic models are only as good as their constants. Discipline:

- **Seed** from public specs (bandwidths, lane counts, clocks, PCIe gen).
- **Calibrate** the fixed-latency / overhead constants against the few
  real datapoints we *do* have: MoltenVk on the M-series host (real
  transfer + submit timings via `MoltenVkBackend`), and any real-HW spot
  checks when available. Store calibration deltas per profile.
- **State accuracy bounds:** bulk-transfer and bandwidth-bound kernels
  model well (single-digit-% achievable after calibration); latency-bound
  tiny ops and contention/overlap are approximate. The ledger flags
  low-confidence ops (very small, latency-dominated) so analysis doesn't
  over-trust them.
- **Never present shaping-mode timings as measurements** — they're a
  behavioural approximation (§3.3). Measurements come from accounting +
  calibration.

---

## 7. Dependencies & scope

- **Tier-equivalence** (`energy-policy.md`) is the precondition for the
  virtual-Tier-3 framing — the model only earns "pixel-correct timing"
  because Tier-2 ≡ Tier-3 pixels.
- **Dev/CI/host instrument, not a shipping-runtime path.** Default
  `passthrough` profile = zero modeled cost = today's behaviour.
  Production builds run with the model absent or in passthrough; it is a
  measurement/development tool, off by default in shipped images.
- All Atrium-authored, permissive, no Mesa/LLVM runtime dependency.

---

## Appendix A — Layer 3: register-accurate AMD/QEMU device sim (the driver grail)

Captured for completeness and to bound expectations. **This is a
separate, deferred grail with a different purpose than Layers 1–2.**

**Purpose.** Not Tier-3 *render* testing (Layers 1–2 cover that) — but
**driver/kernel testing**: developing the D5+ native atrium-gpu Vulkan
driver (an amdgpu-equivalent kmod) against a simulated device when no
card is on the bench. Its consumer is the *kmod*, not the renderer.

**Decomposition** (each independently large):
1. **Functional ISA simulator** — execute real RDNA shader ISA
   correctly. Note: for *pixels* this duplicates Tier-2, so its only
   added value is ISA-level driver/compiler testing.
2. **Timing micro-architecture model** — wavefronts, CUs, SIMD lanes,
   cache hierarchy, memory controllers. A cycle-ish model; a far heavier
   cousin of Layer 2.
3. **Register/MMIO/ring-accurate device** — emulate the BAR register
   map, PM4 command packets, ring buffers, doorbells, fences, interrupts
   — i.e. a **QEMU device model** the kmod opens and drives. This is the
   part "register-accurate from AMD open docs" actually means.

**Prior art & the licensing trap:**
- **Multi2Sim** *did* simulate AMD GCN (Southern Islands) ISA+timing —
  but it is **GPL-2, Southern-Islands-era, unmaintained**. GPL violates
  Atrium's permissive-only charter (`feedback_atrium_licensing_policy`);
  it cannot be vendored.
- **gem5** (BSD) and **GPGPU-Sim/Accel-Sim** (BSD-ish) are permissive —
  but **NVIDIA-shaped** (PTX/SASS), the wrong target.
- AMD's open docs (GPUOpen, RDNA ISA references, `umr`, open amdgpu) give
  the *information* to build a permissive RDNA model — but no maintained
  permissive *implementation* exists. So Layer 3 is **from-scratch,
  multi-engineer-year, permissive** work.

**When justified.** Only when D5+ native-driver bring-up needs a HW-free
dev loop, and even then likely scoped to sub-layer 3 (a register/PM4
device model in QEMU) driven by sub-layer 1 for functional results —
*not* a full cycle-accurate micro-arch sim. Until then: **deferred.**

---

## Phased implementation plan

Cross-compile on the macOS host; never `cargo --release` in the VM.

- **D-M0 — `CostModelBackend<B>` + passthrough.** Decorator skeleton
  wrapping `MoltenVkBackend`/`Tier2Backend`; `passthrough` profile;
  `FrameLedger` plumbed; daemon flags. Proves zero behaviour change.
- **D-M1 — Layer 1 transfer model + profiles.** UMA + discrete topology
  math (§3.2), the profile library, accounting-mode ledger. Validate:
  modeled upload/readback costs match hand-calc for known sizes; UMA vs
  discrete readback asymmetry visible in the ledger.
- **D-M2 — energy term + power/residency signal.** §3.4 + drive the
  energy-policy GPU power signal in sim.
- **D-M3 — shaping mode.** Opt-in completion-deferral; verify the guest
  frame pacing reacts; assert it's flagged as approximate.
- **D-M4 — Layer 2 execution model.** SPIR-V instruction-mix extraction
  + roofline/occupancy; "virtual Tier-3" differential harness (Tier-2
  pixels vs model cost).
- **D-M5 — router integration.** Energy-policy router phase-1 queries the
  ledger/model for Tier-2↔Tier-3 crossover; validate against modeled
  profiles, then calibrate vs MoltenVk on the M-series host.
- **D-M6 — calibration + CI regression.** Calibration deltas per profile;
  `bench_device_model` JSONL traces in CI.
- **(Deferred) Layer 3** — Appendix A, gated on D5+ driver bring-up.

## Open questions

- **Name.** §0 — Chorobates / Groma / Gnomon / other (user picks).
- **Per-op vs per-frame granularity for shaping.** Per-op deferral is
  more faithful but higher overhead; per-frame lumping is cheaper. Decide
  at D-M3 from measured overhead.
- **Instruction-mix cost source.** Extract from our SPIR-V IR at
  pipeline-create (preferred — we own it) vs sampling the Tier-2
  executor. Decide at D-M4.
- **Profile authority.** Ship profiles in-tree (versioned data) vs a
  user-editable profile dir. Likely both: in-tree library + override dir.
