# Atrium memory pressure — budgeting RAM under the unified posture

> **Status:** design + signal→brain→daemon built/verified 2026-06-22. Kernel
> PSI-`some` + avg10/60/300 live in-VM (#172, Phase 1a/1b); controller model tier
> (Phase 2) deterministic; `memoryd` first cut (Phase 5) reaps the right tier live —
> the loop is closed end to end. Applies the federation *machinery*
> ([`atrium-scheduler-federation.md`](atrium-scheduler-federation.md),
> [`atrium-power-posture.md`](atrium-power-posture.md)) to physical-memory
> **capacity** — but, per the federation doc's governing principle, as a
> **budgeting / efficiency** problem, not a fairness one. Memory capacity is a
> *separate currency* from watts; it is unified with power only at the single
> **posture** knob (`kern.sched.power_policy`).

## 1. The problem

At 8–16 GB+ the cap never binds and this is all inert. On a 4–6 GB phone, pressure
is real and the failure mode matters: FreeBSD's `vm_pageout_oom` kills the
largest-RSS process — blunt, lifecycle-blind, and it fires at the *cliff* rather
than throttling on the *slope*.

Atrium already has the hard parts of the Android answer:

- **the container** — jail-per-app + dedicated uid ([[project_atrium_app_isolation]]);
- **the lifecycle** — Portcullis launch, Insula bundles, Choragus sessions;
- **a cap mechanism** — FreeBSD **RCTL/RACCT** (`memoryuse`, `swapuse`, with
  deny/sig/log actions) is the per-jail `memory.max` analog;
- **caches that already know they're caches** — Tessera CAS is eviction-safe
  ([[project_fresco_recovery]]); the GPU BO/residency pool is fd-addressed
  ([[reference_render_paths]]).

What's missing for mobile: a **pressure signal** (FreeBSD has no mature PSI), a
**compressed-RAM tier** (no zram/zswap in base), a **cooperative trim channel +
cached-jail pool** (no `onTrimMemory` analog), and treating the CAS cache as a
member that *yields* RAM under pressure rather than sitting outside the loop.

## 2. Doctrine: memory capacity is budgeted, not fair-divided

The federation doc's principle decides the shape before any code:

> *A resource being scarce never makes it the unit of fairness — scarcity argues
> for efficiency in it and for budgeting it, not for dividing it fairly.*

So there is **no memory-denominated vruntime**, no fair-share of bytes. RAM is
scarce → we **budget** it (caps + protected floors) and **reclaim** it
**efficiently** (a cost-ordered cascade). Priority enters as *whose floor is
protected* and *what is reclaimed last*, never as a fair byte-split.

**Two faces of memory, two currencies — keep them apart (invariant #3):**

- **Memory's *power*** (DRAM refresh + bandwidth energy) already belongs to the
  **watts** energy federation — that is where memory contends with CPU/GPU/display.
- **Memory's *capacity*** (bytes) is **this** controller. Bytes never enter the
  watts split; watts never enter the byte budget.

### 2.1 What the posture does — and does NOT — do for memory (correction)

An earlier draft over-applied the CPU/GPU posture semantics ("powersave → be
aggressive") to memory reclaim. That is a **category error**, because gating
*removes* work (saves power) whereas **reclaim *is* work** (costs power: compress
burns CPU, swap burns flash, a refault pays twice). So:

- **The cascade tier choice is an ENERGY decision, not a posture knob.** Each tier
  (drop-clean / compress / swap / kill) has a per-page energy
  (`gpusim compress.rs::tier_energy_pj`, the same currency as the watts federation),
  and the controller picks the cheapest tier that relieves the pressure, bounded by
  the CPU-power budget. Compression is the lowest-energy response to pressure (it
  beats swap *and* kill on energy *and* stall), with **no posture input** — "powersave
  compresses more" is wrong. Codec choice (lz4 vs zstd) is also an energy trade, set
  by the CPU-power budget, not the posture.
- **Eager reaping does not save power** — a cached app is idle (≈0 W); killing it
  saves ≈0 and makes its later *cold restart* cost *more*. The reaping-tolerance knob
  is a **responsiveness / footprint** (user-intent) preference, not a power lever, and
  for battery may even warrant the *opposite* (keep caches, avoid cold-restart churn).
- **So the posture's honest role in memory is thin** — footprint-vs-responsiveness
  pacing — and `memoryd` follows `power_policy` for *user-intent alignment*, not
  because memory reclaim is a power lever like gating.
- **Where capacity genuinely meets watts** is **rank power-down via compaction** (pack
  the working set into fewer ranks, self-refresh the rest) — modelled in
  `gpusim compaction.rs`, which finds it a *narrow, fragile* win: it pays only for a
  sparse rank idle past break-even (≈0.5 s for a near-empty LPDDR rank, ~22 s for a
  full one), is blocked outright by a single pinned page (fragmentation), and saves
  far more on server DDR than on mobile LPDDR. Not in scope here; the bigger mobile
  memory-power win stays "keep the SoC idle."

So §3's cascade and its energies are the federation's; the posture meets memory only
at this thin pacing seam, never as the driver of the reclaim mechanics.

## 3. Model: floors, an elastic remainder, and a reclaim cascade

Each member exposes three numbers, not one:

- **`min`** — the protected working set (the `memory.min` analog; taking it →
  thrash). Hard.
- **`current`** — resident bytes (working set + its caches).
- **`reclaimable[tier]`** — what it could give back, *and at what cost-tier*.

Memory splits into **working set** (hard; thrashes if taken) and **elastic** (file
cache, CAS cache, cold anon → compressible/swappable). The give is the elastic.

**The reclaim cascade is a cost-ordered ladder — the same shape as the GPU gating
ladder** (`powergate.rs`: clock-gate → gfxoff → d3cold, each cheaper-to-keep but
costlier-to-exit). Memory's ladder, cheapest-to-reclaim first:

| Tier | Reclaims | Cost | Destructive? |
|---|---|---|---|
| 1 clean cache drop | file + CAS clean pages | recompute/refetch (CAS = likely re-hit) | no |
| 2 compress cold anon | cold anon → zram-equivalent | decompress latency | no |
| 3 swap | colder anon → flash | flash latency + wear | no |
| 4 cooperative trim | app sheds caches (`trim_memory`) | app rebuilds its caches | no |
| 5 park / evict cached jail | a backgrounded app-jail | **restart** (cheap on Atrium, §5) | semi |
| 6 OOM by lifecycle tier | a live jail, lowest tier first | lost work | **yes** |

**Honest asymmetry vs power** (don't paper over it): power throttling (DVFS) is
instantaneous, reversible, graceful; memory reclaim is **asynchronous, lossy, and
cliff-shaped**. So a memory "grant" is a **soft target / high-watermark that drives
reclaim pressure on the member**, not an instantaneous clamp. And the cap is
*harder* than a power cap — physical RAM is physics, not a thermal policy you can
briefly exceed.

## 4. The control loop (pressure-driven)

The input is a **PSI-equivalent pressure signal** — *time stalled on memory*, per
jail and global. This is the load-EWMA analog the Laminar controller already
consumes; it is the missing kernel primitive (§6).

```
pressure P = psi_memory()                      # the control input (slope, not cliff)
if P rising:   descend the cascade by cost-tier across members,
               protecting each member's floor by its weight
if P falling:  let members re-grow elastic (caches) toward their posture target
```

- **The "we know" advantage** (again): Portcullis knows which jails are background.
  That is **predictive reclaim** — shed *before* the cliff, exactly the
  deadline-aware-pre-wake move from `powergate.rs`, here hiding reclaim cost instead
  of gating-exit latency.
- **`water_fill` appears in exactly one place** — splitting the protected **floors**
  when even the floors cannot all fit under a tight cap. That is triage under hard
  scarcity (by lifecycle-tier weight, foreground floor first), *not* a fair byte
  division — consistent with §2.
- **Posture sets the resting point on the cascade + the soft cap.** Powersave: small
  caches, eager compress, a tight soft cap below physical → maximum free RAM.
  Performance: fat caches, lazy reclaim, cap = physical RAM. The **same**
  `power_policy` 0..10 that drives CPU parking / GPU gating / display PSR.

## 5. Membership + weights = the lifecycle

**Members:** app-jails, the Tessera CAS cache, the GPU BO/residency pool, kernel
UMA zones, the page cache.

**Weight = lifecycle tier = priority** — the *same* "weight = priority under
contention" as the energy federation. A **foreground** app has a high weight (floor
protected, reclaimed last); a **cached/parked** jail has weight ≈ 0 (no floor,
reaped first). The Android **lmkd tier is literally the federation weight** —
the two ideas unify.

**Atrium's edge over lmkd**, both from the jail+bundle design:

1. **Informed eviction** — kill by lifecycle + manifest tier, not blind-largest-RSS
   (`vm_pageout_oom`'s heuristic).
2. **Cheap restart** — an Insula bundle is CAS-addressed and self-contained
   ([[project_atrium_bundle_format]]): relaunch = re-exec from CAS, and its pages
   are likely *still in the CAS cache*. Android keeps cached apps because cold start
   is expensive; if Atrium's restart is cheap, the keep-vs-reap calculus shifts —
   Atrium can reap **more** aggressively for less user-visible cost.

## 6. Plumbing

- **Pressure primitive (PSI-equivalent)** — per-jail + global stall accounting. The
  missing kernel *mechanism*. Policy stays in a userspace daemon — the lmkd lesson
  (Android moved its killer out of the kernel) and Atrium's own doctrine
  (portcullisd/lyrad: mechanism in kernel, policy in a daemon).
- **Enforcement = RCTL** — per-jail `memoryuse`/`swapuse` caps + actions are the
  cap/floor actuator that already exists.
- **A reclaim member interface** paralleling `sys/energy_budget.h`:
  `memory_member_register(name, probe_fn, reclaim_fn, weight)` where `probe_fn`
  returns `{min, current, reclaimable[]}` and `reclaim_fn(target, posture)` =
  "descend your cascade to `target`." Same registry/tick pattern as the energy
  federation, different currency.
- **Compressed-RAM tier (zram/zswap analog)** — a build/port item; the cheapest
  non-destructive deep tier, near-mandatory on flash.
- **Cooperative trim** — a Pergola `trim_memory(level)` callback ([[project_pergola_toolkit]])
  + a **cached-jail pool** managed by Portcullis/Choragus. The `onTrimMemory` +
  cached-pool analog.
- **A `memoryd`** (or fold into an existing daemon) running the §4 loop, taking the
  posture broadcast alongside `power_policy`.

## 7. Coherence invariants

1. **Cap hard (physics), posture soft.** Performance under a tight RAM cap still
   reclaims; you cannot exceed physical RAM, ever.
2. **Floors before electives, by tier.** A foreground working set is reclaimed last;
   a cached jail's floor is zero. Triage, not fair-share.
3. **One posture, two currencies.** Power (watts) and memory (bytes) are separate
   federations sharing the single posture; never conflated (§2).
4. **Reclaim is cost-ordered and predictive.** Cheapest non-destructive tier first;
   lifecycle foreknowledge sheds before the cliff; killing is the last tier and is
   informed.
5. **Slack = inert.** At desktop RAM with a lazy posture the cap never binds, no
   reclaim runs, members keep their caches — mirrors the energy federation at
   `cap = 0`.

## 8. Worked cases

- **Desktop slack:** cap not binding, lazy posture → fat caches, zero reclaim.
  Trivial, as it should be.
- **Mobile, foreground app + many cached jails:** pressure rises → cascade drops
  clean cache, compresses cold anon, trims cooperatively, reaps cached jails by tier
  — the foreground floor is never touched.
- **Mobile, foreground app grows past comfort:** its soft cap binds → its *own*
  elastic is reclaimed first; only then are background floors triaged by `water_fill`;
  OOM fires only if even the foreground floor cannot fit.
- **Powersave vs performance at the same RAM:** powersave keeps a leaner footprint
  (smaller resting caches, sooner cold-page reclaim), performance holds more resident
  for snappier response — a *responsiveness-vs-footprint* preference (user intent),
  **not** a power saving (reclaim costs energy; see §2.1). The reclaim *mechanics* are
  energy-driven by the federation, independent of the posture.

## 9. Implementation phases (when approved)

0. **Model tier (deterministic, gpusim-style):** a reclaim-cascade + pressure-
   controller *simulator* — the `powergate.rs` analog for memory. Prove cascade
   ordering, floor protection, predictive-vs-reactive reclaim, posture moves the
   resting point, the cap clamps. No kernel. **DONE** — `gpusim engine/src/pressure.rs`,
   8 tests (271 engine total): slack inert, `some` (cache wait) vs `full` (working-set
   thrash), predictive flattens the spike, floors protected lowest-weight-first,
   hard-cap OOM, posture moves headroom, edge-trigger fires once.
1. **Pressure primitive** — a PSI-equivalent in the kernel.
   - **1a — global signal: DONE + verified in-VM (#171).** `sys/kern/kern_pressure.c`
     + `sys/sys/pressure.h`: the PSI `some` accumulator (wall-time with ≥1 thread
     stalled, by the 0→1/1→0 transitions of a stalled count — not a sum of per-thread
     times); leaf mutex, hot-path-cheap. Bracketed at the real memory-stall sleep
     points in `vm_page.c` (`"vmwait"` in `vm_wait_doms`, `"pfault"` in
     `vm_waitpfault`; pagedaemon's own waits excluded). Exposed at
     `kern.pressure.memory.{some_ns,nstalled}`. Verified: a paging hog left `some_ns`
     at **0** while free RAM fell 11→2 GB (allocation-into-free ≠ stall — why
     free-pages is the wrong signal), then climbing ~1 s-stall/s once paging began
     with `nstalled=1`.
   - **1b — decaying PSI averages: DONE + verified in-VM (#172).** A 1 s callout
     folds the `some_ns` delta into three fixed-point EWMAs (no kernel FPU);
     `kern.pressure.memory.avg{10,60,300}` as fraction ×10000, mirroring Linux
     `/proc/pressure/memory`. Verified: under a paging pulse `avg10` fast-rose and
     saturated ~100% while `avg60`/`avg300` lagged; after it `avg10` decayed fast
     (74→9% in 24 s) while the longer windows held — the PSI multi-window signature.
   - **PSI `full` — attempted, REVERTED (negative finding, 2026-06-22).** Built
     `full` = wall-time with a stall pending and zero non-stalled runnable work
     (`total_load == 0` from the Laminar control loop, no new hot-path hook). In-VM
     it read **0 even while `some` showed 92% stall** — because the **pagedaemon is
     itself runnable throughout thrash** (it is the thread doing the reclaim), so
     `total_load` is never 0. A coarse load proxy cannot tell "CPU running the
     reclaim daemon" from "CPU running productive work." Faithful `full` needs
     **per-task mem-stall accounting in the scheduler** (Linux PSI's design: mark
     tasks in-stall, compare `nr_running` to `nr_memstall`) — a real scheduler
     change, deferred. `some` (which works) stands; `memoryd`'s free-floor gate
     remains the thrash proxy.
   - **PSI `full` — BUILT + verified in-VM (#177).** The confound (the prototype's
     `total_load` proxy counted the running pagedaemon as productive → full stuck at
     0) is fixed by the key classification from the model (`gpusim psi.rs`): a CPU is
     "productive" only if it runs a **USER** thread; the idle thread and any **kernel**
     thread (`P_KPROC`: pagedaemon, laundry, swap-I/O) are reclaim/IO infrastructure,
     not workload progress. Realized **low-risk** — no scheduler hot-path hook, no
     `struct thread` change: `pressure_sample_cpus()` samples each CPU's `pc_curthread`
     from the Laminar control loop (~100 ms), and Riemann-sum-integrates the elapsed
     period into `full_ns` iff (a thread is blocked on memory) AND (no productive
     user thread runs) — so `full_ns` can never exceed wall-clock. `full` =
     `(nstalled > 0 && productive == 0)`. Exposed at
     `kern.pressure.memory.{full_ns,full_avg10,full_avg60,full_avg300}`. Verified
     under a 38 s thrash: `some` 94.6%, `full` 88.7% — `full ≤ some ≤` wall, the gap
     being the hog running between faults. `memoryd` can now key on `full` instead of
     the free-floor proxy. (A per-task `sched_switch`-tracked version would be more
     precise + scalable, but the sampled version is correct and far lower-risk; that
     remains a future refinement.)
   - **1c — per-jail attribution + the kqueue edge-trigger** (with `memoryd`):
     prison-keyed storage via `pr_osd` (no KBI break); a `EVFILT_VM` note `memoryd`
     waits on instead of polling. Deferred until there is a consumer (an event source
     with no sink rots).
2. **Reclaim member interface + RCTL enforcement + the cached-jail pool.**
   - **Controller model tier — DONE.** `gpusim engine/src/controller.rs`
     (`MemController`): watches the pressure signal and under *sustained* thrash
     reaps the lowest-lifecycle-tier member early (before the kernel's largest-RSS
     OOM); posture sets reaping patience; cache-only pressure never reaps. 4 tests
     (275 engine total) — converges by reaping cheapest-first, lifecycle-tier not
     largest-RSS, posture tolerance, no-reap-on-cache-churn. The `memoryd` brain,
     proven before the daemon (it kills processes).
   - Remaining: the kernel reclaim-member interface (parallels `energy_budget.h`),
     RCTL enforcement, the cached-jail pool (Portcullis/Choragus).
3. **Compressed-RAM reclaim tier** (zram/zswap analog).
   - **Model tier — DONE** (`gpusim engine/src/compress.rs`, 7 tests / 282 total).
     The principled fix for the memoryd v3 finding (cooperative trim is unreliable
     under pressure): a kernel, **non-destructive, cooperation-free** reclaim tier.
     `compress(cold)` frees `cold×(1−1/ratio)`; the store holds `logical/ratio`
     physical. Proven: compressed refault (decompress ~1 µs) is ≥25× cheaper than a
     flash read (the PSI-stall reduction over swap); cascade orders
     compress < drop-clean < swap-flash < kill; effective capacity =
     `(ram−pool)+pool×ratio`; falls through to swap/kill once the cold pool is spent
     (a tier, not a panacea); CPU budget bounds the reclaim rate.
   - Remaining: the kernel implementation — a compressing swap pager (a substantial
     focused effort), specified + de-risked by the model.
4. **Pergola `trim_memory` cooperative channel.**
5. **`memoryd` policy loop + posture wiring.** **FIRST CUT DONE + verified in-VM
   (#172).** `memoryd/` (cross-compiled `aarch64-unknown-freebsd`) reads the live
   `kern.pressure.memory.avg10` + free RAM, members register `(pid, tier)` in a file
   (the Portcullis stand-in), and under sustained thrash it reaps the lowest tier
   (posture-paced; `--arm` to SIGKILL, default dry-run). Verified: dry-run logged the
   correct lifecycle pick (`WOULD REAP … tier=0 … sparing foreground t9`); **armed,
   it SIGKILLed the tier-0 hog while the tier-9 foreground survived and free RAM
   recovered — memoryd rescued the VM by killing the right process**, the opposite of
   `vm_pageout_oom`. Thrash = sustained `avg10` gated by a free-memory floor (the
   live signal is PSI `some`; `full` is a later kernel refinement).
   - **Built since the first cut:** a three-tier app-cooperative cascade
     **TRIM(SIGINFO)→EXIT(SIGTERM)→KILL(SIGKILL)** with a reap-in-flight guard and a
     SPARE-on-relief path (finding: cooperative trim is best-effort — a thrashing
     target is starved and can't respond, so the kill is the guarantee); RSS-aware
     tie-break within a tier with a quantified lmkd-vs-OOM decision log; and **posture
     wiring DONE** — with no `--posture` flag, memoryd FOLLOWS `kern.sched.power_policy`
     live (re-read each tick), so the one posture knob spans CPU + GPU + display +
     memory reclaim. Verified in-VM: flipping `power_policy` 5→0→10 moved memoryd's
     tolerance in step.
   - Remaining: per-jail members once 1c lands; consume kernel `full` when it exists.

## 10. Non-goals / deferred

- **Memory-denominated fairness** — the governing principle forbids it (§2).
- **Per-jail NUMA / huge-page policy** — orthogonal; later.
- **Cross-device shared-buffer (dmabuf) accounting** beyond noting the GPU BO pool
  is a member — the GPU residency work already owns that mechanism
  ([[project_gpu_device_model_spec]]).
