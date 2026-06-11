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
  on the control-theoretic load balancer, and on integrated DVFS; it has been
  benched against ULE favorably. ULE's only edge is **maturity**.
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
   Complementary to EEVDF, not redundant.
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

- **Within a member** (CPU threads; GPU clients) there *is* a fine common progress
  unit — **time** on a CPU, **work/time** on a GPU. Fair-divide *that*. Energy is
  the wrong unit here because it decouples from progress.
- **Across members** (CPU vs GPU vs memory vs display) there is **no** common
  progress unit — CPU-seconds and GPU-frames are incommensurable. The *only* shared
  currency is **watts**. So energy is not a *choice* at the federation level; it is
  the unique commensurable unit, and fair-dividing it is correct.

The GPU doc's error was a **layer-violation**: it took the across-member currency
(energy, correct *there*) and applied it to within-member fairness (where a finer,
progress-faithful unit exists). Two layers, two currencies — each correct.

## 3. Within-member layer (fine timescale, µs)

Weighted fair queueing, EEVDF-keyed:

- **Charge** = the member's **progress unit** (time-on-core for CPU; work / time-
  on-engine for GPU). *Never energy.*
- **Selection** = earliest-virtual-deadline-among-eligible, computed by the
  portable sharded reduction.
- **Weight function** — the *only* per-member difference:
  - **CPU:** `weight = priority(nice)` only. **No efficiency term** — memory-bound
    is *legitimate* (a DB query is not "wasteful"), so efficiency-weighting a CPU
    thread would re-introduce the inversion.
  - **GPU:** `weight = priority × efficiency`. A VRAM-thrasher (low arithmetic
    intensity, high energy, *low useful work*) gets low weight → its virtual time
    advances fast per unit GPU-time → it is throttled. This is **strictly better
    than energy-in-the-charge**: it throttles by *inefficiency* (genuine wasters /
    bandwidth hogs), sparing the efficient-but-power-dense kernel that energy-
    fairness wrongly hit. Energy enters only as an *input to the weight*, not as the
    charge.
- **Preemption** = modulate-`L` (`atrium-gpu-scheduler.md` §6): near a deadline,
  ramp the inductance `L` (the reluctance to discard in-flight work) smoothly to
  zero — faster *and* more damped at once — for a clean switch; far from one, keep
  `L` high and resist costly preemption. Same overlay on CPU and GPU.

Energy does not appear in selection, charge, or preemption ordering anywhere in
this layer.

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
| cost model (duration + energy) | `cost.rs` | budget input + GPU efficiency-weight input |
| deadline-modulates-`L` preemption | `rlc.rs` | within-member preemption (both CPU & GPU) |
| energy-optimal DVFS / race-to-idle | `dvfs.rs` | budget actuator |
| thermal RC outer loop | `thermal.rs` | budget source + burst headroom |
| `water_fill` budget allocation | `federation.rs` | across-member budget (watts) |
| portable min-(deadline) reduction | `reduce.rs` | within-member selection (EEVDF-keyed) |

The energy-fair `EnergyScheduler`/`SchedRegs` demonstrations remain *valid as the
GPU's efficiency-weighted WFQ* once re-read as "time-charged, efficiency-weighted"
rather than "energy-charged" — the in-VM result (equal-weight different-power
queues) is then the *efficiency-weighting* story, not an energy-fairness one.

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

The result is one mechanism (EEVDF WFQ + modulate-`L`) across CPU and GPU,
differing only in the **weight function** and **per-class budget exposure** —
"same kernel, retargeted" in the literal sense — federated by a shared **watt**
budget. Which is exactly what *coordinated, not coupled — share intent, independent
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
  WFQ-with-efficiency-weights, not greedy throughput-max. Confirm there is no
  single-tenant regime that wants pure greedy max (and if so, it is an *overlay*,
  not a currency change).
- **Efficiency weight on CPU:** decided *out* (memory-bound is legitimate). Revisit
  only if a measured externality (cache/BW pollution) justifies a *mild* term — and
  if so, as a weight nudge, never a charge.
- **EEVDF + modulate-`L` interaction:** does the cost-gate measurably cut EEVDF's
  small-slice preemption thrash without hurting its latency bound? This is the
  headline isolation win to *measure*, not just argue.
- **Placement bimodality:** the un-chased post-fork artifact — resolve before
  claiming the control-balancer beats PELT.
