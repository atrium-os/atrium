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
layer and keeps progress-fairness intact. And before any of it earns adoption, the
bare CPU scheduler has to **survive the A/B against ULE and EEVDF** (§1) — the
federation is the payoff, not the entry ticket.

## 1. The isolation gate (must hold *before* the federation matters)

A reviewer will strip the story and ask: *is this a better CPU scheduler than
ULE/EEVDF on plain workloads?* If no, nothing else is heard. Honest scorecard:

- **vs ULE** (FreeBSD incumbent — per-CPU queues + interactivity *heuristic*):
  Laminar wins on model cleanliness (principled WFQ vs a tuned sleep/run ratio),
  on the control-theoretic load balancer, and on integrated DVFS; benched against
  ULE it is **competitive, with wins** (most gaps closed, leads in several — not a
  clean sweep). ULE's remaining edge is **maturity**.
- **vs EEVDF** (Linux 6.6 SOTA — lag-gated *eligibility* + earliest *virtual
  deadline* + per-task *slice*): on the **single-runqueue ordering model, plain
  min-vruntime Laminar is CFS-class, and EEVDF strictly dominates CFS.** EEVDF's
  eligibility stops a freshly-woken low-vruntime task from monopolizing, and its
  per-task slice gives principled latency that min-vruntime only approximates with
  a global wakeup-granularity. **On bare ordering, Laminar is behind.** Pretending
  otherwise loses the argument at the first latency benchmark.

**Prerequisite — re-base Laminar's selection on EEVDF.** This is a *compatible*
change, not a rewrite: EEVDF is still lag/vruntime-based weighted fair queueing,
so it slots into the same sharded reduction (`atrium-gpu-scheduler.md` §10) — you
reduce by `(eligible, virtual_deadline)` instead of by `vruntime`. The
"min-vruntime reduction" generalizes to a "min-virtual-deadline-among-eligible
reduction" with no structural change to the picker or its portability.

With EEVDF ordering in place, Laminar keeps **three differentiators EEVDF lacks**:

1. **Preemption-*cost* awareness (modulate-`L`).** EEVDF preempts whenever a
   better task becomes eligible; with small slices for latency that means *many*
   preemptions = cache/TLB thrash. modulate-`L` is an explicit gate — *don't tear
   down expensive in-flight work to honour a slack deadline* — layered on EEVDF
   selection. Latency where the deadline is tight; no thrash where it is loose.
   *Anticipated objection:* "isn't that just dynamic slice extension / adaptive
   `RUN_TO_PARITY`?" No — the slice is a **static per-task** latency parameter;
   `L` is **state-dependent**: it prices the *running* work's actual teardown
   cost (occupancy, cache footprint, save/restore size) at this instant. EEVDF
   knows what the waiter is owed; modulate-`L` also knows what the switch
   *destroys*. Complementary, not redundant.
2. **Control-theoretic placement.** Lead-compensated control loop vs PELT +
   periodic heuristic balancing — plausibly better on NUMA / heterogeneous
   (big.LITTLE). *Plausibly*: the post-fork placement bimodality is still
   un-chased; this is promising, not proven.
3. **Integrated scheduling + frequency.** ULE and EEVDF both *delegate* DVFS to a
   separate governor that fights the scheduler; Laminar folds frequency into the
   same loop with an energy-optimal `f*`. On a power-limited SoC this is structural.

**Verdict:** EEVDF-class ordering is the *price of admission*; modulate-`L` +
control placement + integrated DVFS is the *edge*; the federation is the *payoff*.
The isolation gate is therefore a precondition, not a distraction.

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

Weighted fair queueing, EEVDF-keyed — and **identical on CPU and GPU**:

- **Charge** = **time on the engine** (time-on-core / time-on-GPU). *Never
  energy* — and not "work" either: work is not measurable on an opaque command
  stream, while engine-time is. One charge unit for every member.
- **Selection** = earliest-virtual-deadline-among-eligible, computed by the
  portable sharded reduction.
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
- **Preemption** = modulate-`L` (`atrium-gpu-scheduler.md` §6): near a deadline,
  ramp the inductance `L` (the reluctance to discard in-flight work) smoothly to
  zero — faster *and* more damped at once — for a clean switch; far from one, keep
  `L` high and resist costly preemption. Same overlay on CPU and GPU.

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
| portable min-(deadline) reduction | `reduce.rs` | within-member selection (EEVDF-keyed) |

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

1. **Re-base selection on EEVDF** (eligibility + virtual deadline + per-task slice),
   keeping the sharded reduction. *Credibility gate; do this first.*
2. **Keep modulate-`L`** as the preemption-cost gate over EEVDF selection.
3. **Keep** the control-theoretic placement and swap load-proportional DVFS for the
   energy-optimal `f*` model.
4. **Add the cost model as a *budget input*** (per-thread energy estimate feeding
   the member's power demand) — **not** as a vruntime charge.
5. **Wire to the federation `water_fill`**: Laminar draws its CPU power allocation
   from the shared cap, signals urgency for latency bursts, and is throttled by the
   shared thermal loop (proportionally, shares intact).

The result is one mechanism (EEVDF WFQ + modulate-`L`) across CPU and GPU — same
charge (engine-time), same weight rule (priority), differing only in the
**`L`/`R` preemption-cost constants** and **per-class budget exposure** — "same
kernel, retargeted" in the literal sense — federated by a shared **watt** budget.
Which is exactly what *coordinated, not coupled — share intent, independent
mechanisms* always said.

## 7. Framing (doc & pitch order)

Lead with the **isolation pitch** (EEVDF-class ordering + cost-aware preemption +
integrated energy-optimal DVFS — better than EEVDF, clearly better than ULE), then
introduce the **federation** as the payoff. Leading with the federation invites
"nice story, but your CPU scheduler is behind" — and on bare ordering, *today*, it
would be. Pass the gate first.

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
- **EEVDF + modulate-`L` interaction:** does the cost-gate measurably cut EEVDF's
  small-slice preemption thrash without hurting its latency bound? This is the
  headline isolation win to *measure*, not just argue.
- **Placement bimodality:** the un-chased post-fork artifact — resolve before
  claiming the control-balancer beats PELT.
