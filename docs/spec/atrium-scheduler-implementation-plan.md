# Cohesive scheduling — implementation plan

**Status:** plan, 2026-06-11. Executes
[`atrium-scheduler-federation.md`](atrium-scheduler-federation.md) (the settled
two-layer design: progress-fair lanes within members, watt budget across) on the
real system: `sched_laminar.c`, the brokers (frescod, later lyrad), Aqueduct,
the atrium-gpu-amd kmod, and the gpusim/engine model as the proof harness.
Method is the project's standing one: **prove the math deterministically in the
model first, then build the kernel piece, then verify in-VM, gated by benches.**

## 1. Decisions register (the §8 opens, resolved)

| # | open | decision | rationale |
|---|---|---|---|
| D1 | per-class *charge*? | **No.** One charge (engine-time) everywhere. POSIX `rtprio` stays untouched as the compatibility escape hatch (static-priority RT class above both lanes, as today). Classes differ only in latency priority + budget exposure. | No residual case found: every "this class needs a different charge" candidate decomposed into priority (latency) or budget (energy/thermal). |
| D2 | GPU greedy throughput-max regime? | **No currency/weight change ever.** Even "single-tenant fullscreen" has ≥2 clients (game + compositor). An operator wanting throughput bias sets weights — policy, documented inversion risk. | Closed in the federation doc; nothing new emerged. |
| D3 | efficiency weight | **Out, on both members.** Revisit trigger: a *measured* externality the budget layer cannot express. | Settled (federation doc §3). |
| D4 | memory-BW member granularity | **Member-level first.** Per-client BW attribution is deferred behind a trigger: a gpusim scenario demonstrating member-level squeeze punishing innocents while the hog persists. No new machinery until that scenario exists. | Per-client BW accounting needs counters we neither model nor have; don't build ahead of evidence. |
| D5 | admission model | **CBS-lite, per entity, per-CPU.** Broker declares `(Q budget, T period, anchor)`. Admission: Σ Q/T ≤ `U_lane` per CPU (default **0.75**) and ≤ the jail's manifest cap. Overrun (budget exhausted) → **throttle to WFQ lane until replenishment** (the CBS guarantee: a greedy renderer cannot eat the audio deadline). Replenishment by per-entity callout at period boundary. | Full CBS semantics where they matter (overrun isolation), no more. The 25% WFQ floor *is* the starvation guarantee (D8). |
| D6 | deadline-miss semantics | **Kernel counts, broker decides.** Miss (deadline passed, budget remained — e.g. blocked) ≠ overrun. Kernel records per-entity miss/overrun counters and raises a **kqueue event to the sponsoring broker**; no kernel auto-policy beyond the CBS throttle. frescod skips/retargets a frame; lyrad grows the buffer or surfaces it. | The broker has the domain knowledge; the kernel has none. Mirrors "mechanism in kernel, policy in broker." |
| D7 | broker API shape | **A small syscall family + kqueue, capability-gated.** `deadline_sponsor(tid, Q, T, anchor)` / `deadline_update` / `deadline_withdraw` / `deadline_yield` (end-of-period hint), events via `EVFILT_DEADLINE` (miss/overrun/throttle). Authority: a jail manifest capability (`deadline_broker`) checked via priv(9)+jail param; frescod/lyrad hold it, apps never do — apps *receive* sponsorship. Deadlines are absolute `CLOCK_MONOTONIC` + period; the broker does the vblank/buffer math. | One API, two brokers (federation doc §1); audio's tight periods are the proving case. |
| D8 | two-lane starvation | **`U_lane` is the guarantee** — WFQ keeps ≥ 1−`U_lane` (25%) of every CPU by construction. Prove deterministically in the model (P1) before the kernel build. | The cap is admission-enforced, so the proof is of the *scheduler honoring the cap under load*, which the model can do exactly. |
| D9 | deadline inheritance | **Two stages.** (a) *Priority-band PI now*: lane entities run in a dedicated kernel priority band above timeshare, so the **existing turnstile PI already lifts a WFQ lock-holder** blocking a lane thread — zero new mechanism, kills the classic inversion. (b) *True deadline lending later*: lend the deadline value (EDF among lenders) for correct ordering when *multiple* lane threads block, plus **Aqueduct deadline adoption** (a context field on calls; the server thread adopts the caller's deadline and **charges the caller's budget** — charge-back closes the gaming hole). | (a) is ~free and covers the audio chain's lock legs; (b) is the principled completion and the only genuinely new kernel mechanism in the plan. |
| D10 | undeclared-tier validation | Run as **validation, not design driver** (P0). Acceptance: wake-latency tails (p99/p99.9) and interactivity-under-load within parity band of ULE on the existing suite + a new tail bench; EEVDF compared via a Linux 6.6+ reference VM on the same host *(qualitative if the rig fights us)*. Only a demonstrated gap admits the per-task slice knob (paper-clean, WFQ lane only). | Keeps EEVDF-chasing out of the design while staying honest. |
| D11 | modulate-`L` metrics | WFQ lane: **involuntary CSW rate at equal tail latency** (the thrash cut). Lane: **deadline-meet rate under contention** with/without the gate. Both measured in P5; gpusim `rlc.rs` already proves the math. | Defined so P5 is a measurement, not an argument. |
| D12 | placement bimodality | **Chase in P0** (before any balancer claims). If un-fixed by one focused pass, document the artifact and drop the "beats PELT" claim until it is. | Honesty gate from the federation doc. |

**Structural decisions (new, needed for build):**

- **S1 — lane data structure:** per-tdq **small array of deadline entities**
  (cap ~32; media threads are few), linear EDF scan (earliest absolute deadline
  among released, budget>0) *before* the WFQ shard pick. **No change to the
  sharded WFQ reduction.** The lane is per-CPU; entities are placed at admission
  and may migrate only at replenishment boundaries (no cross-CPU EDF locking).
- **S2 — budget enforcement:** charge on context-switch-out + statclock tick
  (`cpu_ticks` granularity); replenishment + throttle-release via `callout(9)`
  per entity. This is the main new kernel machinery; precision under
  QEMU/HVF virtual timers is a named risk (R2).
- **S3 — everything sysctl-gated** per the existing Laminar phase discipline
  (`kern.sched.laminar.deadline_enable`, `…deadline_util`, `…inherit_enable`),
  each phase independently revertible, benched before/after.
- **S4 — clean-room:** anything EEVDF/CBS-flavored is implemented from the
  papers (Stoica & Abdel-Wahab; Abeni & Buttazzo for CBS) — **never Linux
  `fair.c`/`deadline.c` (GPL)**.

## 2. Phases

Laminar continues its letter-phase discipline (…H = DVFS). New phases:

### P0 — baselines & debts *(small, do first)*
1. **gpusim `SchedRegs` charge fix**: charge **time**, not energy (one line +
   re-read the in-VM test as mechanism-validation; Joule counters stay as
   telemetry). Re-run engine + in-VM suites.
2. **Latency-tail bench**: add a wake-latency tail microbench (p99/p99.9 under
   load mixes) to `tools/test/sched-laminar/bench`; record Laminar-vs-ULE
   baseline. Optional Linux 6.6 reference VM for EEVDF numbers (D10).
3. **Placement bimodality** (D12): one focused pass on the post-fork artifact.
- *Gate:* baselines recorded; no regressions introduced; bimodality fixed or
  documented.

### P1 — model proofs (gpusim engine; deterministic, no VM)
1. **`lane.rs`** — two-lane scheduler sim: EDF+CBS lane over the WFQ core,
   mixed real periods (2.7 ms audio + 16.7 ms frames + WFQ background).
   *Prove:* (i) zero misses when admitted ≤ `U_lane`; (ii) WFQ floor ≥
   1−`U_lane` held under full lane load (D8); (iii) overrun isolation — a
   budget-violating "frame thread" throttles, audio unaffected (D5); (iv)
   mixed-period EDF beats static-tier ordering on the same input; (v) the
   inversion scenario with/without inheritance (D9 math).
2. **`audio.rs`** — audio device model on the Timeline: ring consumed at sample
   rate in virtual time, **underrun = referee fault** (audio analog of the
   tear), mirroring D-display-1. Feeds the minimum-reliable-buffer benchmark
   later, and `lane.rs` scenario (i).
- *Gate:* all five lane proofs + underrun-iff-deadline-missed, deterministic.

### P2 — kernel lane core *(Laminar phase I)*
- Per-tdq entity array + EDF-before-WFQ pick (S1); CBS budgets + callouts (S2);
  the syscall family + `EVFILT_DEADLINE` (D7); `U_lane` + per-jail caps
  (manifest plumbing can stub to a sysctl until Portcullis wiring);
  priority-band placement of lane threads (sets up D9a for free).
- In-VM **metronome test**: synthetic clients at (Q,T) of audio shape (2.7 ms)
  and frame shape (16.7 ms, bursty) under `bench_skew`-style background load.
- *Gate:* zero misses at admitted utilization; WFQ floor holds (measured);
  overrun throttling observed; all existing benches regression-free with
  `deadline_enable=0` **and** =1-with-empty-lane.

### P3 — brokers *(Laminar phase J + userspace)*
- **frescod**: sponsor clients' frame threads with vblank-anchored deadlines
  (it already holds display timing); choose target-vblank policy; consume
  `EVFILT_DEADLINE` (skip/retarget on miss).
- **Audio**: lyrad doesn't exist yet (D4 roadmap) — validate the audio shape
  with the synthetic client + `audio.rs` model now; the broker API is designed
  against both shapes (D7), so lyrad slots in at its own milestone with no
  kernel change.
- **Aqueduct**: specify the deadline-context field on calls now (wire format
  only; adoption lands in P4).
- *Gate:* compositor demo under load — **frame-time variance** improves vs P0
  baseline with sponsorship on; no WFQ-tier regression.

### P4 — inheritance *(Laminar phase K)*
- **K-a**: confirm turnstile PI through the lane priority band (test: lane
  thread blocks on a mutex held by a WFQ hog under load — audio metronome stays
  clean). Mostly verification, given P2's band placement.
- **K-b**: deadline lending (EDF among lenders) + Aqueduct adoption with
  **charge-back to the caller's budget** (closes the gaming hole: a server
  can't be used to launder lane time).
- *Gate:* the inversion test passes with K-a; cross-IPC pipeline (synthetic
  client → echo server) holds deadlines with K-b; charge-back observed.

### P5 — modulate-`L` *(Laminar phase L)*
- The preemption-cost gate: lane preemptions priced against real slack;
  WFQ-lane preemptions against a conservative default `L` (cache-warmth proxy =
  time-since-switch-in, PMC footprint later).
- *Gate (D11):* involuntary-CSW rate drops at equal tails (WFQ);
  deadline-meet rate non-regressing (lane). If the gate shows nothing on CPU,
  *say so* — it remains load-bearing on the GPU regardless.

### P6 — energy & the federation budget *(Laminar phases M/N)*
- **M**: swap `laminar_dvfs_step`'s load-proportional target for energy-optimal
  `f*` (fit `k_dyn`/`P_static` from the cpufreq level table; `cpufreq_mock`
  validates; race-to-idle floor at `f*`, deadline pressure may exceed it).
- **N**: `kern_energy_budget.c` — members register (CPU = Laminar, GPU = the
  kmod, whose `SCHED` interface already exposes weights+Joule counters);
  `water_fill` in watts (the pure function, ported); mock thermal source first
  (the `cpufreq_mock` pattern); urgency hook for latency bursts. Laminar
  consumes its allocation as a DVFS ceiling — the gpusim `cascade.rs` shape.
- *Gate:* in-VM cascade behavior mirrors the model: turbo→throttle→steady,
  member shares preserved under cap, lane deadlines still met within budget.

### P7 — the measurement story
- Full benchmark narrative on one rig: **frame-time variance under load** +
  **underrun count / minimum reliable buffer** (the two headline metrics) +
  undeclared-tier tails vs ULE (and the EEVDF reference if P0 stood it up) +
  modulate-`L` deltas. Whitepaper update (`scratch/sched-sim/WHITEPAPER.md`
  lineage) framed per federation doc §7: Atrium axis first, never "EEVDF done
  better."

## 2.5 Status as built (2026-06-13)

Branch `atrium/scheduler-phase-A` (freebsd-src), `frescod`/`atrium-gpu-amd`
(bsd), gpusim. Running results: `tools/test/sched-laminar/bench/results/
LAMINAR-vs-ULE-2026-06-12-waketails.md`.

| Phase | Status | Where / lessons not anticipated by the plan |
|---|---|---|
| P0 | ✅ | SchedRegs time-charge fix (`02ac26a`); wake-tail bench + the **RT-blind placement** discovery (`rt_occupied_cost`, ~500×) — the first cut gated on `is_timeshare` did nothing because pipe wakers run at kernel sleep priority pre-userret; bimodality dispositioned as the host/HVF stall class (see P7 blocker). |
| P1 | ✅ | `lane.rs` 5 proofs + `audio.rs` underrun referee (`637c4d5`). The inversion proof itself surfaced the broker-API consequence: **Q must be sized ≥ exec+hold**. |
| P2 (phase I) | ✅ | `90b417537fab`. Three fixes the plan didn't foresee: replenish callout must be **`C_DIRECT_EXEC`** (8–227 ms lateness was softclock-swi *scheduling*, not eventtimer drift); the wake-preempt gate must **not** test `le_yielded` (YIELD backstop + replenish expire same-instant); entity teardown must be the **cdevpriv dtor** (UAF panic otherwise). Statclock budget-charge mis-throttled 15% idle → precise sbinuptime charge at the switch boundary. |
| ULE A/B | ✅ | `f066fc4a252e`. Lane = the only *unprivileged* near-RT path. ULE wins undeclared p99 (interactivity guess); Laminar's bounded-lag clamp wins the extreme tail ~5×. Doc claim "competitive on undeclared tails" holds at p99.9/max, not p99. |
| P3 (phase J) | ✅ | `SPONSOR_FOR` + vblank-anchored grids (`245df76f`); kqueue **miss feed** (`5e053425`) — two interrupt-context locking lessons: no `taskqueue_enqueue` under the tdq lock (the swi wake re-enters `sched_add`), only `taskqueue_fast` from a direct-exec callout. frescod is the real broker, end to end (bsd `af1912d`): `OP_LANE_REQUEST` + `LOCAL_PEERCRED` + live miss feed; `xucred`'s pid union is 8-aligned. |
| P4 K-a | ✅ | `f39bf7f7`. **Key lesson:** lent priority is *invisible* in the WFQ tier (vruntime-ordered), so the band must sit **below** `PRI_MIN_TIMESHARE` to route a boosted holder onto the rt-path bucket where it wins *selection* — the first cut at `==PRI_MIN_TIMESHARE` measured PI *worse* than the plain-mutex control. lane-pi: 10.65% → 0.00% misses. |
| D9 | ✅ | `58b58da2`. `deadline_broker` = `kern.sched.deadline_brokers` (host-root-only grant on the phase-E jail table; self-grant EPERM; `p_cansee`-filtered targets). Deadlines are Portcullis capabilities end to end. |
| P4 K-b | ✅ | `22da0f3e` (kernel) + bsd `925c8e9` (frescod). `ADOPT`/`DROP`: a server adopts the client's entity, band for selection + **charge-back to the client's CBS budget**. Gate-found: charge must stamp at ADOPT (a band burn may never switch); lane wakes must preempt **band** incumbents (a 53% miss storm otherwise). frescod reader/writer adopt = first real Aqueduct deadline-context, no wire change. `thread0_storage` reserve 10→14 u64s. |
| P6-M | ✅ | `8160db8d`. Energy-optimal DVFS floor `f*` = argmin (P−idle)/f over the cpufreq table (no model constants); powersave settles AT the floor. |
| P6-N | ✅ | `a9848ab0` (kernel + `sys/sys/energy_budget.h`) + bsd `c9e7e2b` (GPU member). In-kernel `water_fill` splits `kern.sched.energy_cap_mw` by weight across the CPU member (DVFS ceiling) and the GPU member (gpusim power regs). Two fixes: the budget ceiling must **hard-override** the load-driven controller (a cap beats what load wants); the release path must push budget=0 when the cap clears (else a member stays throttled). cap 1500 → cpu 224 + gpu 1276, work-conserving; cap beats perf-policy+load. |
| P5 | ⏸ deferred | The lane already meets deadlines via immediate band preemption; the CPU value of slack-aware deferral is the marginal CSW-rate cut the plan itself hedges ("if the gate shows nothing on CPU, say so"). The gpusim `rlc.rs` math stands and is load-bearing on the GPU. Revisit when a clean rig can measure involuntary-CSW deltas. |
| P7 | ✅ both headlines landed | **Audio underrun / min reliable buffer** (`lyra/scripts/L6-RESULTS.md`, 2026-06-14): lane holds 0 underruns at the 5.3 ms hardware-min buffer where timeshare needs 128 ms = 24×. **Frame-pacing / dropped frames** (`tools/test/sched-laminar/bench/PACE-RESULTS.md`, 2026-06-14, `metronome` + `pace-sweep.sh`): under 16 spinners the lane drops 0% of frames at every depth while timeshare drops 42–88% once a frame costs ≳ 4 ms — the lane refuses past the 75% admission duty (won't promise what it can't keep). The host/HVF stall blocker (a) is **sidestepped by methodology**: both metrics are **kernel/device-counted** (OSS `play_underruns`, `LAMIOC_STATS` misses), which the episodic stalls can only *inflate*, so min/median over reps recovers the true floor — the L6 honesty trick. Blocker (b) cleared: lyrad shipped. Remaining: cross-OS reference + pro-path end-to-end latency. |

The within-member arc (declared lane → brokered → inversion-proof →
capability-gated → lending), the across-member arc (watt federation), and
**both P7 headline measurements** (audio underrun + frame pacing, lane vs
timeshare) are complete and in-VM-verified. What remains is P5 (low-ROI on CPU)
and the cross-OS reference comparison + pro-path latency tails.

## 3. Dependency graph & sizing

```
P0 ──► P1 ──► P2 ──► P3 ──► P4(K-a) ──► P4(K-b)
                 │            └────────► P5
                 └──────────────────────► P6(M) ──► P6(N) ──► P7
```

P1 is pure model work (fast, the cheapest place to be wrong). P2 is the big
kernel item (~the size of a previous Laminar phase: entity array + callouts +
syscalls). P3 is mostly frescod. K-a is nearly free; K-b and N are the two
genuinely novel kernel pieces (deadline lending; the budget subsystem). P5/P6-M
are small. Estimated order: P0+P1 together first (one stretch), then P2 as its
own stretch, then P3 onward each gated.

## 4. Risks

- **R1 — admission realism:** frame threads' bursty arrivals make Q/T
  declarations guesses; CBS throttling converts bad guesses into per-period
  demotions (safe), but frescod needs a sane Q policy. Mitigate: start
  conservative (Q from observed p95 render time), iterate in P3.
- **R2 — timer precision in the VM:** callout-driven replenishment at 2.7 ms
  periods under QEMU/HVF jitter. Mitigate: validate the metronome on bare
  `eventtimer` resolution early in P2; the *model* proofs (P1) are exact
  regardless; final numbers on hardware.
- **R3 — lane lock footprint:** keep the entity array under the existing tdq
  lock; entities are few; no global EDF structure (S1). Watch for tdq-lock hold
  time growth in P2 benches.
- **R4 — scope creep into RT-kitchen-sink:** the admission rule (declared,
  hardware-anchored or inherited — never inferred) is the firewall; review every
  lane addition against it.
- **R5 — clean-room (S4):** standing.

## 5. Standing constraints (unchanged)

Kernel = C; kmods + kernel build **in-VM** (`make clean && make`; 9p mtime
discipline); never `kill -9` QEMU (monitor `quit`); gpusim server builds on the
host; permissive licenses only; every phase sysctl-gated + benched; gpusim
deterministic tests are the referee before any VM work.
