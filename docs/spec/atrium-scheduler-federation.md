# Atrium scheduler federation — progress-fairness within, energy-budget across

**Status:** notes, 2026-06-11. Non-binding; settled in discussion. **Partially
supersedes** [`atrium-gpu-scheduler.md`](atrium-gpu-scheduler.md) §7/§9 — the
"energy-*denominated vruntime* / energy-fair as the universal fairness currency"
claim is **retracted** (it negates per-task fairness; see §5). Everything else
that doc built stands and is relocated to the correct layer here. Companion to the
Laminar CPU scheduler (`SCHED_LAMINAR`) and
[`atrium-display-architecture.md`](atrium-display-architecture.md); grounds
against the gpusim model (separate repo). Energy stance:
[`feedback_energy_policy_coordinated_not_coupled`].

## 0. What changed, and why

The GPU scheduler doc reached a clean thesis — *the scheduler and the energy
router are one object, denominated in Joules; a federation of identical RLC
controllers coordinated only by a shared energy currency.* Pressure-testing it
against a **CPU** scheduler broke one load-bearing claim and sharpened the rest:

> **Energy-fairness equals progress-fairness only when energy ∝ work.** That
> holds on a GPU (FLOPs ≈ Joules, by coincidence of a throughput device). It
> **fails on a CPU**, where power per second is set by the instruction mix, not by
> useful work — so "equal Joules" rewards the power-*cheap* (a pointer-chasing or
> memory-stalled thread) and throttles the power-*dense-but-efficient* (an AVX
> encoder doing real work). Energy-vruntime **inverts** the CPU's fairness contract.

The repair is a single governing principle (§2) that puts energy in the *right*
layer and keeps progress-fairness intact.

A second round of pressure-testing then broke a second claim: this doc originally
made **re-basing Laminar on EEVDF a prerequisite** ("match Linux's ordering or
lose the A/B"). That was *competitive mimicry* reasoning — adopting the answer to
**Linux's** question instead of asking Atrium's. §1 records the corrected
position: EEVDF is the best scheduler for an OS that *cannot know* which threads
need latency; Atrium's integrated stack (Fresco/Pergola/manifest/jails) *knows*,
and Laminar already carries EEVDF's main practical lesson (bounded lag at
wakeup). The right CPU design is real-deadline-aware where deadlines are real,
plain WFQ where they aren't — not synthetic deadlines everywhere.

## 1. The isolation question — answered on Atrium's axis, not Linux's

A reviewer will strip the story and ask: *is this a better CPU scheduler than
ULE/EEVDF?* The first draft answered by proposing to adopt EEVDF's ordering. That
was the wrong frame. Answer the question by examining what each scheduler is
*for*:

- **vs ULE** (FreeBSD incumbent — per-CPU queues + interactivity *heuristic*):
  Laminar wins on model cleanliness (principled WFQ vs a tuned sleep/run ratio),
  on the control-theoretic load balancer, and on integrated DVFS; benched against
  ULE it is **competitive, with wins** (most gaps closed, leads in several — not a
  clean sweep). ULE's remaining edge is **maturity**. Note ULE's interactivity
  score is *guessing* who needs latency — the same epistemic position as EEVDF,
  answered less rigorously.
- **vs EEVDF** (Linux 6.6 — lag-gated *eligibility* + earliest *virtual deadline*
  + per-task *slice*): EEVDF's deadlines are **synthetic** — `v_d = v_e +
  slice/weight` derives from a request-size parameter, not from anything in the
  world. It is proxy machinery, and it exists because of **Linux's epistemic
  constraint: a general-purpose kernel serving arbitrary userspace cannot know
  which threads need latency**, so it must guess as fairly as possible. EEVDF is
  the best known answer to *"latency-fairness under ignorance."*

**That is not Atrium's question.** Atrium owns the entire stack — Fresco *is* the
compositor, Pergola *is* the toolkit, apps live in Portcullis jails under
manifests, and the kernel already coordinates with display timing. The
latency-critical chain (input → app frame thread → compositor → flip) is
**legible** to this OS, with **real** deadlines (vblank, frame budget) declared
by components the manifest bounds. Importing EEVDF means importing machinery
built for ignorance into a system that has knowledge.

**What Laminar already has.** The main *practical* failure EEVDF fixed in CFS —
unbounded lag at wakeup (sleeper boost / fresh-waker starvation) — is already
solved in Laminar by the **symmetric bounded-lag clamp** (`sched_laminar.c`
~2849, with the multi-second wake-pick pathology it eliminated documented in the
comment). The remaining EEVDF delta — synthetic-deadline ordering + the per-task
slice knob — is precisely the *guessing* machinery Atrium needs least. Dropping
the re-base also dissolves its worst engineering risk (the `V`-dependent
shard-cache staleness a full eligibility implementation would force); the hot
path stays exactly as it is.

**The Atrium-native within-member design — three lanes:**

1. **Declared real-deadline lane** — really a *media-pipeline lane*, fed by
   **deadline brokers**: trusted, manifest-capability daemons that sponsor
   deadlines for their client threads. **frescod** (vblank → frame deadlines) and
   **lyrad** (audio buffer/rate → period deadlines) are instances of one API, not
   special cases; camera/capture joins later with the same shape. Admission is
   CBS-style: the **manifest caps a jail's deadline-lane utilization**, so
   deadlines are *capabilities*, not a gameable boost — the Portcullis shape.
   Within the lane, earliest-*real*-deadline ordering handles mixed periods
   (2.7–10 ms audio + 16.7 ms frames + 33 ms capture) without static tiers.
   modulate-`L` consumes **real slack** — the *same* deadline-dissolves-inertia
   mechanism on CPU (frame/audio thread vs its deadline) and GPU (dispatch vs
   vblank). Note **audio is the lane's best-posed client** — tighter period than
   vblank, worse miss consequence (an underrun is universally audible; a dropped
   frame is tolerable judder), and near-constant per-period work (the textbook
   CBS workload) — so the admission math is proven against audio even though
   graphics motivated the lane. Audio also surfaces the mechanism graphics let us
   miss: **deadline inheritance** — across locks (turnstile priority-PI
   generalized to deadlines) and **across Aqueduct IPC** (a server runs a request
   under the caller's deadline), since the audio chain (app callback → lyrad mix
   → driver ring) crosses both and is killed by classic inversion otherwise.

   **"Known deadline" — a three-way taxonomy, and the admission rule.** Lane
   clients are not uniformly "known"; they divide into: **(a) hardware-anchored
   periodic** (audio, capture) — the device dictates period *and* phase (DMA
   drain at `buffer/rate`, sensor cadence); the deadline is a physical fact,
   exact at stream-open. **(b) hardware-anchored sporadic** (frames) — the
   vblank *grid* is exact, but frames are event-triggered (an idle app has no
   deadline) and *which* vblank a frame targets (next vs next+1) is broker
   policy; sporadic CBS admission (budget / min-inter-arrival). **(c)
   derived/assigned** (input; brokered IPC work) — no intrinsic hardware
   instant; the broker *assigns* a deadline anchored to (a)/(b) (input inherits
   the frame it must influence), and inheritance propagates it. Crucially,
   **deadline-known ≠ work-known**: audio has both (≈constant DSP per period →
   near-guarantee); graphics has a known deadline but unknown WCET → the lane
   grants *priority to meet*, never a guarantee, and **this is why CBS rather
   than raw EDF** — budget exhaustion demotes an overrunning frame thread for
   the period, so a greedy renderer cannot eat the audio deadline. The boundary
   rule that keeps the lane honest: **admit only *declared* deadlines anchored
   to a hardware timing fact or explicitly inherited from one — never inferred
   ones.** If no anchor exists, the thread belongs in WFQ; inferring deadlines
   would be ULE's interactivity guess / EEVDF's synthetic deadline returning
   through the side door. (Edges: VRR makes the vblank grid elastic — the
   deadline becomes a window, deferred with the display doc's VRR work; network
   real-time is locally audio-shaped (jitter-buffer drain) but only the local
   leg is ours to schedule.)
2. **WFQ for everything undeclared** — current Laminar (min-vruntime + the
   bounded-lag clamp): shells, builds, daemons, legacy apps. The right tool where
   no deadline exists. EEVDF's per-task *slice* may later be adopted **here** as
   a cheap latency knob — taking the paper's idea where it applies, without
   making synthetic deadlines the foundation. (Clean-room note: anything taken
   from EEVDF comes from the Stoica & Abdel-Wahab paper, never Linux's GPL
   `fair.c`.)
3. **Idle/batch** — as today.

Laminar's standing differentiators are unchanged and now sharper:

1. **Preemption-*cost* awareness (modulate-`L`)** — and on Atrium it prices a
   switch against a **real** deadline's slack, not a synthetic one. It is
   state-dependent (the *running* work's actual teardown cost — cache footprint,
   occupancy, save/restore size), which no static slice parameter captures.
2. **Control-theoretic placement.** Lead-compensated control loop vs PELT +
   periodic heuristic balancing — plausibly better on NUMA / heterogeneous
   (big.LITTLE). *Plausibly*: the post-fork placement bimodality is still
   un-chased; promising, not proven.
3. **Integrated scheduling + frequency.** ULE and EEVDF both *delegate* DVFS to a
   separate governor that fights the scheduler; Laminar folds frequency into the
   same loop with an energy-optimal `f*`. On a power-limited SoC this is structural.

**Verdict & the honest A/B.** On *undeclared* workloads Laminar must be — and,
with the clamp, plausibly is — **competitive** with EEVDF on the pathologies that
matter (validate with the latency-tail suite; that A/B is run as *validation*,
not as the design driver). On the desktop's actual job — **frames on glass and
sound in the air, under load** — Atrium is **categorically** better positioned,
because deadlines are real and end-to-end (CPU deadline lane + GPU modulate-`L` +
display timing; audio callback → lyrad → DMA ring), which Linux structurally
cannot have without owning the compositor and the audio server. Two headline
benchmarks: **frame-time variance under load** (the gpusim frame-pacing arc
already built its instrumentation) and — crisper, because it is binary and
countable — **underrun count / minimum reliable audio buffer size under load**
(the number the Linux pro-audio world fights PREEMPT_RT for). *EEVDF is the best
scheduler for an OS that can't know; Atrium knows.*

## 2. The governing principle

> **Fairness is over the finest *progress-faithful, commensurable* unit available
> at that level. Budgeting/efficiency is over what is *constrained*. A resource
> being scarce never makes it the unit of fairness — scarcity argues for
> efficiency in it and for budgeting it, not for dividing it fairly.**

Two corollaries fix the whole design:

- **Within a member** (CPU threads; GPU clients) the claimants contend for one
  pool — the member's execution time — and a fine progress-faithful unit for it
  exists: **time on the engine**. Fair-divide *that*. Energy is the wrong unit
  here because it decouples from progress.
- **Across members** (CPU vs GPU vs memory vs display) the members do *not*
  contend for each other's time — CPU-time and GPU-time are separate pools, so
  there is nothing to fair-divide in those units. What they **jointly contend
  for** is the power/thermal envelope. Fairness divides the *shared contended
  thing*, and across members that thing is **watts** — so energy is not a
  *choice* at the federation level; it is what is actually being divided.

The GPU doc's error was a **layer-violation**: it took the across-member currency
(energy, correct *there*) and applied it to within-member fairness (where a finer,
progress-faithful unit exists). Two layers, two currencies — each correct.

## 3. Within-member layer (fine timescale, µs)

Weighted fair queueing with a real-deadline lane — **identical on CPU and GPU**:

- **Charge** = **time on the engine** (time-on-core / time-on-GPU). *Never
  energy* — and not "work" either: work is not measurable on an opaque command
  stream, while engine-time is. One charge unit for every member.
- **Selection** = two lanes over one picker (§1): threads with **declared real
  deadlines** (admission-controlled) order by earliest real deadline; everything
  else orders by min-vruntime WFQ with the bounded-lag wakeup clamp. Both are
  min-key scans, computed by the same portable sharded reduction — the key
  changes per lane, the reduction does not.
- **Weight** = `priority` — on **both** members, with **no efficiency term**.
  Memory-bound is *legitimate* on both: a DB query on the CPU and a texture-heavy
  or blit-bound pass on the GPU are low-arithmetic-intensity *honest* workloads,
  and the scheduler cannot distinguish them from "wasters." Any efficiency term —
  in the charge *or* the weight — re-introduces the inversion (penalizing the
  legitimately memory-bound), just less directly. The earlier draft gave the GPU
  `priority × efficiency`; that was the same mistake at one remove, and it is
  withdrawn.
- **Bandwidth hogging is an externality, handled at the budget layer.** The
  shared resource a "VRAM-thrasher" over-consumes is **memory bandwidth** — which
  is itself a federation *member* with its own budget allocation (§4;
  `federation.rs` already models `memory-bw`). A client that saturates the memory
  member gets squeezed by *that member's* allocation, not by a fairness-weight
  judgment inside the GPU's queue. Throughput-per-Joule bias on the GPU remains
  available as an explicit **operator policy** (a deliberate weight adjustment for
  a throughput-tenant device) — but it is policy, not a fairness principle, and it
  carries exactly the inversion risk documented here.
- **Preemption** = modulate-`L` (`atrium-gpu-scheduler.md` §6): near a **real**
  deadline, ramp the inductance `L` (the reluctance to discard in-flight work)
  smoothly to zero — faster *and* more damped at once — for a clean switch; far
  from one, keep `L` high and resist costly preemption. Same overlay on CPU and
  GPU, driven by the same kind of deadline (frame thread vs vblank; dispatch vs
  vblank).

Energy does not appear in selection, charge, weight, or preemption ordering
anywhere in this layer. With charge and weight now identical across members,
"same kernel, retargeted" is exact: the members differ only in their `L`/`R`
constants (preemption cost physics) and their budget exposure.

## 4. Across-member layer — the federation budget (slow timescale, s)

The shared power cap, divided in **watts** (the only commensurable currency):

- **Steady state:** weighted max-min fair allocation (`water_fill`) of the
  thermal-sustainable power budget across members. Work-conserving — a member
  demanding less releases the slack. (Already built + tested in gpusim
  `federation.rs`; it is *the same* `water_fill` MST uses for a DisplayPort link —
  one allocator, two currencies, by commensurability.)
- **Transient:** a member signals **urgency** upward (a latency-critical CPU burst:
  "give me power *now*"), and the budget reallocates transiently. The **thermal RC
  capacitor** provides the burst-above-sustained headroom (the "turbo" of
  `thermal.rs`) for ~τ before settling — which is what makes transient power-stealing
  physically possible. **DVFS** (`dvfs.rs`, energy-optimal `f*`, race-to-idle) is the
  actuator.
- **Externality, handled here — not by a fairness penalty.** Under thermal pressure
  the cap shrinks → DVFS lowers frequency → everyone slows, but **time-shares are
  unchanged** (WFQ still splits time equally; the pie shrinks, every slice's
  *fraction* is preserved). A hog is squeezed *more* only when another member's
  **urgency** demands it — a budget-layer reallocation, never a within-member share
  strip. So the thermal commons is shared, the slowdown is proportional and fair,
  and the hog pays through the constraint, not through a denial of its time share.

## 5. What is superseded, what stands

**Retracted** (from `atrium-gpu-scheduler.md` §7, and on both CPU *and* GPU):

- "Energy as *the* common currency ⇒ energy-*denominated vruntime* ⇒ energy-fair
  scheduling." Energy is the common currency *across members* (the budget), not the
  fairness denominator *within* a member. Energy-fair vruntime negates per-task
  fairness and has no clean home (every case that seems to want it is either
  energy ≈ work *coincidence* or a mislabeled budget/externality concern).

**Stands** (relocated to the correct layer — most of the GPU work):

| piece | gpusim | layer |
|---|---|---|
| cost model (duration + energy) | `cost.rs` | budget input (per-member power demand + Joule telemetry) |
| deadline-modulates-`L` preemption | `rlc.rs` | within-member preemption (both CPU & GPU) |
| energy-optimal DVFS / race-to-idle | `dvfs.rs` | budget actuator |
| thermal RC outer loop | `thermal.rs` | budget source + burst headroom |
| `water_fill` budget allocation | `federation.rs` | across-member budget (watts) |
| portable min-key reduction | `reduce.rs` | within-member selection (vruntime for the WFQ lane; earliest real deadline for the declared lane) |

**What the in-VM `EnergyScheduler`/`SchedRegs` demo does and does not prove.** Be
precise here, because the algebra bites: the implementation charges
`vruntime += energy/weight`, and a "time-charged, efficiency-weighted" scheduler
(`vruntime += time/(priority × efficiency)`) coincides with it **only if
`efficiency ≡ 1/power`** — i.e. "low-power = efficient," which is *precisely the
retracted inversion*. So the demo's *behavior* (equal Joules across
different-power queues) is the retracted policy and cannot be re-read as anything
else. What the demo **does** validate — and the corrected design still needs all
of it — is the **mechanism**: programmable per-queue weights from the kernel,
firmware-enforced WFQ selection through the register protocol, and readable
per-queue Joule counters (which the budget layer consumes as its demand/telemetry
input). Bringing the implementation in line with this doc is a one-line policy
change: charge **time**, not energy; keep the Joule counters as *observability*,
not as the charge.

## 6. Unifying Laminar (concrete sequence)

1. **Validate the undeclared tier** against the EEVDF-class latency-tail suite
   (the WFQ core + bounded-lag clamp as-is). This is validation, not a re-base —
   if a genuine gap shows, the cheap candidate fix is the per-task slice knob,
   paper-clean, in the WFQ lane only.
2. **Build the declared real-deadline lane**: Fresco→kernel deadline declaration
   (frescod already holds vblank timing), CBS-style admission with the manifest
   capping per-jail deadline-lane utilization, earliest-real-deadline pick via the
   existing sharded reduction.
3. **Add modulate-`L`** as the preemption-cost gate, consuming the declared
   lane's *real* slack (and a conservative default for WFQ-lane preemption).
4. **Keep** the control-theoretic placement and swap load-proportional DVFS for
   the energy-optimal `f*` model.
5. **Add the cost model as a *budget input*** (per-member power demand from
   DVFS-level power × utilization) — **not** as a vruntime charge.
6. **Wire to the federation `water_fill`**: Laminar draws its CPU power
   allocation from the shared cap, signals urgency for latency bursts, and is
   throttled by the shared thermal loop (proportionally, shares intact).

The result is one mechanism across CPU and GPU — same charge (engine-time), same
weight rule (priority), same two-lane structure (real deadlines + WFQ), same
modulate-`L` overlay — differing only in the **`L`/`R` preemption-cost
constants** and **per-class budget exposure** — "same kernel, retargeted" in the
literal sense — federated by a shared **watt** budget. Which is exactly what
*coordinated, not coupled — share intent, independent mechanisms* always said.

## 7. Framing (doc & pitch order)

Lead with the **Atrium-native pitch**: real deadlines end-to-end (input → CPU
frame thread → GPU dispatch → flip), with **frame-time variance under load** as
the headline metric — the axis Linux structurally cannot occupy and the one the
gpusim instrumentation already measures. Support it with the isolation evidence
(competitive with EEVDF on undeclared latency tails — measured, per §6 step 1 —
and ahead of ULE), then the federation as the payoff. Do **not** pitch "we
implemented EEVDF better than Linux" — competing on the opponent's axis with the
opponent's benchmark cedes the design ground that is Atrium's actual advantage.

## 8. Open questions / still to pressure-test

- **Per-class fairness currency: retracted.** All classes are progress-fair WFQ;
  classes differ only in **latency priority** (foreground preempts) and **budget
  exposure** (background squeezed first under thermal/battery pressure). Confirm no
  residual case needs a per-class *charge*.
- **GPU objective:** multi-client Atrium GPU needs a starvation floor, so it is
  priority-weighted WFQ, not greedy throughput-max. Confirm there is no
  single-tenant regime that wants pure greedy max (and if so, it is an *overlay* /
  operator policy, not a currency or weight-rule change).
- **Efficiency weight: decided *out* on both members** (memory-bound is
  legitimate on CPU *and* GPU; the bandwidth externality belongs to the memory-BW
  member's budget). Revisit only if a *measured* externality the budget layer
  cannot express justifies a mild term — and if so, as an explicit operator-policy
  weight nudge, never a charge, with the inversion risk documented.
- **Memory-BW member granularity:** the budget layer squeezes the memory member as
  a whole; does attributing bandwidth back to the *offending GPU client* need a
  finer mechanism (per-client BW accounting feeding its priority), or is
  member-level pressure sufficient in practice? Measure before adding machinery.
- **Undeclared-tier validation:** run the EEVDF-class latency-tail suite against
  the WFQ+clamp core. If a real gap appears, the candidate fix is the per-task
  slice knob in the WFQ lane (paper-clean) — *measure before adopting*.
- **Deadline-lane admission design:** CBS-style replenishment vs simpler
  utilization cap; per-jail budget accounting in the manifest/rctl shape; what a
  deadline-miss does (demote to WFQ for the period? signal the broker?). Design
  the **broker API** (frescod/lyrad sponsor deadlines for clients) once, against
  both brokers — audio's tight, regular periods are the proving case.
- **Deadline inheritance:** generalize turnstile priority-PI to deadlines (a
  lane thread blocked on a WFQ-held lock lends its deadline), and **propagate
  deadlines as context across Aqueduct calls** (lyrad/frescod run a request under
  the caller's deadline). Without this the audio chain dies of classic inversion;
  it is the genuinely new kernel mechanism the lane requires.
- **Audio timing model (gpusim):** a small audio device model on the existing
  Timeline substrate — a ring consumed at sample rate in virtual time, **underrun
  = the referee fault** (the audio analog of the tear) — gives deterministic
  underrun-iff-deadline-missed tests mirroring D-display-1, and the
  instrumentation for the minimum-reliable-buffer benchmark. Matches Lyra's
  slot/ring transport plan.
- **Two-lane starvation interplay:** the declared lane must not starve the WFQ
  lane under full admission — the admission cap *is* the guarantee; pick the cap
  and prove it (the gpusim deterministic harness can).
- **modulate-`L` measurement:** does the cost-gate measurably cut preemption
  thrash (involuntary switches at equal tails) on the WFQ lane, and does
  real-slack gating hold frame deadlines on the declared lane? Both are
  measurable in the existing bench + gpusim harnesses.
- **Placement bimodality:** the un-chased post-fork artifact — resolve before
  claiming the control-balancer beats PELT.
