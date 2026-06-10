# Atrium GPU scheduler — fairness, preemption & energy-denominated RLC control

**Status:** notes, 2026-06-10. Non-binding; the scheduler/energy-control
position settled in discussion. Companion to
[`atrium-gpu-driver-architecture.md`](atrium-gpu-driver-architecture.md),
[`atrium-display-architecture.md`](atrium-display-architecture.md), and the
Laminar CPU scheduler (kernel `SCHED_LAMINAR`); grounds against the gpusim
model (separate repo). Energy stance:
[`feedback_energy_policy_coordinated_not_coupled`].

## 0. Thesis (lead with the payoff)

The GPU scheduler and the energy router are **the same object**. Fair
time-sharing, preemption, and frame deadlines are not three mechanisms bolted
together — they are one **RLC closed-loop controller denominated in Joules**.
Generalize it and Atrium scheduling becomes a **federation of identical RLC
controllers, one per contended resource** (CPU = Laminar, GPU, memory bandwidth,
display refresh), each with resource-specific cost terms, **coordinated only by
sharing the energy currency, never shared state**. That is the strongest form of
"coordinated, not coupled": not ad-hoc schedulers that talk, but *one
mathematical language in one currency*, instantiated per resource.

The rest of this document derives that position from the concrete problem (fair,
jailable GPU time-sharing) and records the control-theory result that makes it
fall out cleanly.

## 1. Why this exists: jailing the UMD needs GPU fairness

The user-mode driver is **untrusted** — per-app code in the app's address space,
assumed malicious. User-mode queues (M9e/M17) put the per-submit hot path in
userspace; the kernel is not on it. Isolation is already built — per-process
VMs + per-VMID page tables (M9c), per-queue doorbell pages (M9e/M17), the opaque
command stream interpreted by firmware *under the jail's VMID*, reset/timeout
(M11). A malicious submit's blast radius is its own queue + its own memory.

The one piece of "jail the UMD" still open is **fairness/quotas** — bounding what
a jail consumes when jails contend. It splits cleanly into two problems with two
homes:

- **Space (quotas)** — how much a jail can *allocate*.
- **Time (fairness)** — how much GPU *execution* a jail gets under contention.

Conflating them is the trap. They are different mechanisms.

## 2. Space → kernel metering (the easy half)

Allocation is on the kernel path (`BO_ALLOC`, `VM_CREATE`, `QUEUE_MAP`,
CWSR save-area alloc), so quotas are straight kernel accounting: **per-prison
counters** (VRAM, GTT, BO count, VM count, queue count, page-table bytes,
save-area bytes), **limits from the `atrium.toml` capability manifest**
(`gpu = { vram = "256MB", queues = 4, … }`), checked at the allocation ioctls →
`EDQUOT`. It is the GPU extension of Portcullis default-deny, and **BSD-native**:
it rides FreeBSD's jail / `rctl` / `racct`, not a cgroup-shaped invention. We
already keep global `bo_count`/`vm_count` (detach safety); this is the same
counters keyed by `cr_prison`.

## 3. Time → firmware time-slice under *revocable* kernel admission

The hot path is user-mode, so the kernel cannot schedule each submit. Resolution
is a two-level structure:

- **Coarse admission = kernel, via revocable queue *mapping*.** A doorbell only
  acts while its queue is mapped into a finite HQD slot. When more jails contend
  than slots, the kernel/firmware rotate which queues are mapped, and the kernel
  can **unmap** a misbehaving jail's queue — which **revokes its doorbell**. This
  is the point people miss about user-mode queues: *the doorbell-as-capability is
  fast but revocable.* The kernel hands out the fast path and reclaims it at will.
  Admission/eviction is the kernel's leash; it never loses authority despite
  being off the hot path.
- **Fine time-slice = firmware**, among the mapped queues: weighted scheduling
  with preemption. The kernel sets **policy** (per-jail weights/priorities); the
  firmware enforces **mechanism**.
- **Backstop = timeout/reset (M11).** A non-preemptible runaway is device-lost
  *for that jail* → reset its queue; the rest survives.

Policy weights come from: the **manifest** (baseline GPU share), the **window
manager/compositor** (foreground priority → responsiveness), and the **energy
router** (throttle on battery/thermal).

## 4. Preemption granularity — the GPU's defining asymmetry

```
drain-to-completion → packet boundary → wave boundary (CWSR) → instruction
   (one dispatch       (between draws/    (mid-dispatch, save/   (exotic)
    blocks all)         dispatches)        restore wavefronts)
```

The realistic choice is **packet-boundary** vs **wave-boundary (CWSR — compute
wave save/restore)**:

- **Packet-boundary** — switch *between* PM4 packets; a single `DISPATCH`/`DRAW`
  runs to completion. Cheap (clean boundary, nothing to save). Worst-case
  neighbor wait = **the longest single dispatch** (a 50 ms kernel makes an
  interactive jail wait up to 50 ms).
- **Wave-boundary (CWSR)** — preempt *inside* a dispatch at wavefront
  boundaries, spilling wave state (VGPRs, LDS, scalar regs) to a per-queue VRAM
  save area. Worst-case wait ≈ **a wavefront + the save/restore cost** —
  bounded regardless of kernel size. This is what bounds interactive latency
  under a compute-heavy neighbor.

**The asymmetry that makes the GPU scheduler ≠ "Laminar for the GPU":** a CPU
context switch is ~free, so Laminar can preempt at a fine quantum almost for
nothing. **GPU preemption is expensive** — saving a wavefront is hundreds of KB
to MBs of memory traffic, needs a pre-allocated VRAM save area, and *costs
power* (memory-controller Joules). The overhead pressure toward *coarse*
preemption is far stronger than on the CPU; fine preemption is a resource you
spend, not a free knob.

**Space/time recombine.** Wave-granular preemption costs the CWSR save area —
so the *time* concern (latency) is paid in the *space* budget (the quota). A
batch jail → packet-granular, no save area, high-latency-tolerant; an
interactive jail → wave-granular, save area allocated and quota-charged, low
latency.

**The display tie-in (why wave-granular earns its cost).** The compositor's
render must finish before vblank to flip without stutter. If a background jail
is mid-50 ms-dispatch and preemption is packet-granular, the compositor's render
can't get on the GPU in time → the flip's in-fence misses vblank → the exact
"render overran the frame" stutter. Wave-granular preemption is what lets the
foreground compositor evict the hog at a wavefront boundary and hit its frame.
Preemption granularity sets the **frame-pacing-under-contention floor**; the
display timeline is where its payoff is observable.

## 5. The unified controller: RLC with a switching-cost term

Reuse Laminar's RLC framework and **put the switching cost into the cost
function**. Then the thing you want falls out for free: the controller preempts
only when the **fairness benefit exceeds the switching cost** — the switching
*frequency emerges* from the balance, with no hardcoded quantum or granularity.
Cheap save/restore → fine sharing; expensive → the controller batches (runs a
queue longer). The "miserly preemption" the GPU needs is produced by the math.
It also threads the energy router through one term: raise the switching cost in
a low-power state → the controller switches less and burns less save/restore
bandwidth automatically.

Two GPU-specific bends the cost term must absorb:

**Bend 1 — the switching cost is state-dependent and bimodal, not a scalar.**
On the CPU `R` (switching resistance) is ~constant. On the GPU the cost of
preempting *now* swings: ~free at a packet boundary; a full save/restore
mid-dispatch; and even mid-dispatch it scales with **occupancy** (a heavily
loaded dispatch is dear; a nearly-finished one is nearly free — just drain it).
So the switching-cost term is `f(execution position, occupancy)`. A
"**switch when it's cheap**" behavior emerges: the controller coasts to the next
packet boundary and switches there *unless* the error is large enough to justify
a mid-dispatch save/restore. The actuator becomes discrete (wait-for-boundary /
pay-for-mid-dispatch / hold) — threshold the controller's continuous output
against the current (varying) cost.

**Bend 2 — the GPU lives in a high-`L`, high-`R` corner of the same parameter
space.** A dispatch in flight has *sunk work* (cycles already spent + the
save/restore) weighing against preemption — physical inductance. The GPU is not
a different model; it is the **same RLC tuned into high inertia, heavily
damped, reluctant to switch.** Laminar's L/C/R tuning intuition transfers; you
just sit in a different corner. The defining GPU property (expensive preemption)
maps to a sensible region of a space we already understand, not new structure.

## 6. Deadlines: modulate the inertia, not the forcing

Fairness is a soft, average objective; the compositor's vblank is a **hard**
deadline. A pure share-driving controller would let the compositor miss a frame
for long-run fairness — the stutter we are killing.

The naïve fix is to **spike a deadline cost term** (shout the setpoint louder).
On a high-`L` system that is exactly wrong: a big step input on a heavy,
underdamped system overshoots and **rings**.

The correct actuator is to **reduce `L` (inertia) as the deadline approaches**.
Reframe: *don't change the target — change the dynamics.* The fairness error
already encodes that the compositor needs the GPU; you just let the controller
reach that target fast and clean by removing the inertia.

The RLC math is unusually kind. With `ω₀ = 1/√(LC)` and `ζ = (R/2)·√(C/L)`,
lowering `L` does **both** desirable things at once:

- lower `L` → **higher `ω₀`** (faster response), and
- lower `L` → **higher `ζ`** (more damped).

A *faster and better-damped* transition in one move — "snap to it without
ringing." Overdriving the forcing on the original high-`L` system does the
opposite (same sluggish `ω₀`, low `ζ`, overshoot).

**GPU semantic (almost literal).** `L` *is* the reluctance — the work-in-flight
inertia (sunk cycles + save/restore cost). "Reduce `L` near the deadline" reads
as **"the deadline dissolves the reluctance to discard the running work."** Far
from vblank, high `L` is correct (sluggish, miserly, ignores short transients,
won't thrash). As vblank nears, `L` ramps down and the GPU's characteristic
reluctance-to-preempt evaporates; the controller decisively hands the GPU to the
compositor. The same controller passing from heavy/patient to light/decisive —
**gain-scheduled on deadline proximity**.

Two things to get right:

1. **Ramp `L`, don't step it.** An inductor stores `½L·i²`; a discontinuous `L`
   injects its own transient. `L(deadline_proximity)` is a smooth shaped ramp,
   and the *shape* (early/gentle vs late/steep) is a tuning surface — shaped
   against a replayable adversarial trace in the deterministic sim, not by
   intuition.
2. **Target critical damping (`ζ ≈ 1`) at the deadline, not `L → 0`.** Critical
   is the fastest response with no overshoot; over-damped is safe but needlessly
   slow; `L → 0` is ill-conditioned (infinitely reactive, noise-sensitive). The
   ramp aims `L` at the value making `ζ = 1` for the current `R, C` right at
   vblank — the provably-fastest clean switch.

This keeps the deadline mechanism **inside** the single-cost-function framework:
no priority-boost special case, no `if`. The flywheel gets lighter precisely
when the frame becomes worth more than the work you'd waste — a continuous,
tunable function.

## 7. Energy as the common currency → scheduler *is* the energy router

`L` was already *encoding the save/restore energy cost* — the reluctance to
preempt is "preempting costs save/restore Joules." So ramping `L` down as the
deadline nears is exactly the statement **"the energy cost of a dropped frame
has now exceeded the energy cost of the save/restore"** — the crossover,
expressed as a change in dynamics, not a branch.

Push it all the way: **denominate every RLC element in Joules.**

| element | role | energy meaning |
|---|---|---|
| `L` (inductance / inertia) | reluctance to change allocation | **save/restore Joules** — the switch energy cost |
| `C` (capacitance / integral) | accumulated unfairness | **Joules of deserved-but-undelivered work** |
| `R` (resistance / damping) | instantaneous switching cost | **marginal Joules to switch right now** at the current boundary |
| forcing (error) | drive toward the setpoint | Joules needed vs delivered; a deadline's worth (dropped-frame Joules) |

Then the controller has **no free tuning constants** — its dynamics fall out of
the device's actual energy model (the per-op / save-restore / refresh energy
profile). And the consequence: the scheduler and the energy router are not
*coordinated* — they are the **same object**. "Coordinated, not coupled" becomes
"same currency, separate budgets," which becomes "same object, per-resource
budget."

## 8. The federation

One RLC controller per contended resource — **CPU (Laminar), GPU (this), memory
bandwidth, display refresh** — each instantiating the *same* control framework
with **resource-specific cost terms** (the GPU's being switch-cost +
deadline-via-inertia), all denominated in the **shared energy currency**, and
coordinated *only* by that currency, never shared state. Laminar's CPU RLC stays
clean — **no renderer term** — consistent with the settled energy stance, now
strengthened: not three schedulers that happen to talk, but one mathematical
language in one currency. The energy router is not a fourth thing reading
intent; it is the currency the controllers minimize.

## 9. Placement — where the controller runs, and the portable kernel

"Scheduling logic" smears two things that want different homes:

- **Control** — sequencing (who runs next), driving the command processor,
  handling preemption boundaries, admission/eviction, *acting on* the decision.
  Stateful, privileged, hardware-touching.
- **Computation** — the min-vruntime reduction, the per-queue RLC update, the
  normalization. The data-parallel *math* that produces the decision.

**Control lives in firmware** — the scheduler microcontroller (MES-class), on
the GPU, off the shader cores: the trusted sequencer that drives the CP. It is
never a shader. **The computation is a SIMD reduction**, and that is the part
that is interesting, because two constraints decide where it runs:

1. **Near the execution path.** Fine decisions happen at packet/wave boundaries
   (sub-µs). A host round-trip per decision is orders of magnitude too slow —
   which is *why* it cannot be the kernel, and reinforces user-mode queues
   (keep the host off the hot path). So: on-GPU.
2. **Must not consume the resource it schedules.** A persistent scheduler kernel
   squatting on a CU competes with the work it arbitrates. So a shader-resident
   scheduler is only sane if it is **tiny and infrequent** — runs *at* a decision
   point (a reduction over N queues, microseconds on a couple of wavefronts),
   then yields — never a resident daemon.

So:

- **Small N (dozens of queues):** the microcontroller's vector unit does the
  reduction inline; no shader.
- **Large N (many-tenant, thousands of queues):** the microcontroller
  **dispatches a tiny privileged scheduler shader** — min-reduction + RLC update
  over all queues in parallel — reads the result, acts. The conductor (firmware)
  calls the parallel calculator (shader) only when the arithmetic gets wide
  enough that SIMD beats the serial loop.

**The payoff — it is the *same kernel*.** Laminar's min-vruntime is already a
SIMD reduction on the CPU; the GPU controller's is a SIMD reduction on the GPU.
So the federation's controllers share not just the *math* but the **kernel**,
retargeted per resource's parallel hardware:

```
energy-RLC reduction kernel   ──▶  CPU SIMD            (Laminar's controller)
(per-queue RLC update +       ──▶  GPU shader          (the GPU controller, large N)
 min-vruntime + normalize)    ──▶  µcontroller vector  (the small-N path)
```

One implementation of "reduce the queue set under the energy-RLC cost," several
backends. The deepest form of the unification: not "same equations, separate
code" but **same code, retargeted**. The self-reference is the point — a SIMD
machine's fair-scheduler *is* a SIMD reduction, so it runs on the machine it
schedules.

**The RLC state fits this cleanly.** The kernel is two phases: a *per-queue* step
(one lane per queue — read `L/R/C` + energy counters from the queue descriptor /
MQD, advance the controller, write back) and a *reduction* (min-vruntime, total
weight, the global decision). Both embarrassingly parallel; the per-queue state
is in the MQDs the energy model already maintains — the Joule counters the
display/render-timing model produces (`save/restore → L`, `unfairness → C`) are
*exactly* this kernel's per-lane inputs.

**A scheduler shader is firmware-owned and TCB, not a user shader.** It reads
every queue's state to arbitrate them — cross-jail visibility by necessity —
which is fine *because* it is part of the trusted scheduler, not a jailed UMD
kernel. But the firmware↔scheduler-shader boundary is inside the TCB, and the
scheduler shader must be as verified as the MES firmware. It is "GPU code in the
TCB" — unusual, and called out deliberately (see §10).

**The model is agnostic to placement.** gpusim captures the scheduling
*function* in virtual time — given the queue set + their `L/R/C/energy` state,
what decision emerges — independent of whether it physically runs on host,
microcontroller, or shader. So we design and verify the *function* (§11) and
treat firmware-vs-shader-vs-µvector as a backend choice; the radical
shader-resident option stays open without being forced. Prove the math before
committing the silicon placement.

**The scheduler must budget itself (a fixed-point).** If the large-N decision
runs as a shader, it occupies execution units for those microseconds — so it
competes with the very deadline it is protecting, and the bound on *its own*
runtime becomes part of the worst-case latency it computes. The scheduler's cost
is therefore a term in the cost it minimizes: it must pay for itself in its own
energy/time currency. A small, real fixed-point — and a hard ceiling on how
heavy the controller may become (an over-clever scheduler that no longer fits in
its own slice has failed).

**Recommendation:** write the computation as a **retargetable SIMD kernel from
day one** — that is what makes Laminar's min-vruntime transfer cleanly and turns
the federation from "same idea" into "same code." Run it on the microcontroller
for the common case; keep the shader-dispatch path for large-N scaling; do not
hard-commit to shader-resident.

## 10. Trust / TCB

- **Firmware joins the TCB** — it enforces per-VMID translation, performs
  save/restore, and runs the schedule. Same as real silicon (the MES enforces
  VMID isolation). gpusim models this as *referee invariants*, not trust.
- **Kernel owns** space (quotas), admission (revocable queue mapping), policy
  (weights), revocation, and reset.
- **UMD is untrusted**; blast radius = its own queue + its own memory.
- **Preemption transparency is a safety invariant**, not a perf one: a preempted
  dispatch must be **bit-identical** to an uninterrupted one. A buggy save/restore
  (a dropped LDS bank, a lost scalar reg) corrupts results *only under
  contention* — the nastiest field bug. A deterministic model that runs the same
  dispatch with and without an injected preemption and diffs the output makes it
  a unit test.

## 11. Modeling & verification (gpusim)

This is gated on **GPU render-timing** — work must take virtual time, or there is
no contention to schedule — which sits on the **virtual-time substrate the
display milestone introduces**. `sched.rs` (already a deterministic fixed-point
sweep, M10) becomes a **virtual-time weighted scheduler** over **preemptible
units** (packet-granular = whole packet; wave-granular = N wave-units with a
save/restore cost charged in virtual time *and* an energy counter), running the
RLC controller.

Referee invariants:

- **fairness** — each jail's GPU-time share within tolerance of its weight;
- **deadline** — interactive/compositor deadline misses bounded (zero under the
  energy budget); latency wait ≤ `bound(granularity)`;
- **no thrash** — switch rate stays sane when switching cost is high;
- **preemption transparency** — preempted output == uninterrupted output;
- **authority** — a revoked/unmapped queue stops; a wedged queue resets without
  starving others.

Tuning is done the Laminar way — shape the `L`-ramp + `R`/`C` against
**replayable adversarial traces** (compositor + background-compute hog) until
shares are fair, deadline misses are zero, and the switch rate does not thrash —
but now with reproducible *frame-level* timing.

## 12. Sequencing (the arc)

```
display timing (D-display-1)   → lays the deterministic virtual-time substrate
  → GPU render-timing          → puts dispatch/draw on it (work takes time)
    → GPU scheduler (this doc) → federated energy-denominated RLC on it
      → energy router          → the shared currency the controllers minimize
```

The display milestone is quietly the unlock for the entire timing / fairness /
energy axis.

## 13. Open questions

- **The device energy profile that yields `L`/`R`/`C`** — save/restore bytes ×
  memory Joule/byte; per-op "deserved work" Joules; refresh/PSR/VRR Joules
  (from the display model, §8 of the display doc). The controller is only as
  good as this profile.
- **Priority inheritance across fence dependencies** — the compositor (high
  priority) waits on a fence from a low-priority app's render; the fence must
  propagate a deadline term (an inertia reduction) onto the producer's queue, or
  the consumer stalls behind its own dependency. The GPU-fence version of
  priority inversion.
- **Multi-resource coupling** — a job bottlenecked on memory bandwidth vs GPU
  compute: which controller "owns" it, and how the federated controllers avoid
  fighting (the shared currency should make this fall out, but it needs proving).
- **Side channels** — a shared GPU leaks timing/contention signals between jails;
  hard on every GPU, deferred but real.

## 14. Summary position

GPU fairness is **space in the kernel (quotas), time in the firmware (time-slice
+ preemption) under revocable kernel admission and kernel-set policy**, backstopped
by reset. Preemption granularity (packet vs wave/CWSR) is the GPU's defining cost
— expensive, paid in VRAM, observable at the display's frame-pacing floor. The
fairness + preemption + deadline control collapses into **one RLC controller**,
where the deadline **modulates the inertia `L` (smoothly ramped to critical
damping)** rather than spiking a forcing term — and where `L`/`R`/`C` are
**denominated in Joules**, so the controller has no free constants and the
**scheduler and the energy router are the same object**. Generalized, Atrium
scheduling is a **federation of identical energy-denominated RLC controllers, one
per contended resource, coordinated only by the shared currency** — the cleanest
possible "coordinated, not coupled." All of it is gated on GPU render-timing,
which is gated on the virtual-time substrate the display work lays down.
