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

What was missing for mobile, and now built: a **pressure signal** (FreeBSD has no
mature PSI → `kern.pressure.memory` some/full/per-jail), a **compressed-RAM tier**
(no zram/zswap in base → `atrium-zram` `/dev/zram0`, verified as live compressed
swap), a **cooperative trim channel + cached-jail pool** (no `onTrimMemory` analog
→ `memoryd` TRIM/EXIT/KILL cascade), and treating the CAS cache as a member that
*yields* RAM under pressure rather than sitting outside the loop.

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
- **The per-jail HARD CAP = RCTL — the PROACTIVE containment layer (verified #178).**
  FreeBSD RACCT/RCTL already provides per-jail memory accounting (`memoryuse`=RSS,
  `vmemoryuse`, `swapuse`) and enforceable limits with actions (`deny` for
  virtual/swap; `sigkill`/`sigterm` for RSS — you cannot cleanly fail a page fault,
  so RSS is enforced by killing the offending process). So a jail's hard memory
  boundary already exists — and it is *better* than reactive reaping for
  containment: a runaway jail is stopped **at its own cap before it can pressure the
  system**, so the machine never thrashes and `memoryd`/PSI never has to engage.
  Verified in-VM: `jail:x:memoryuse:sigkill=3g` killed a 5 GB hog at the 3 GB
  boundary while system free RAM held at 11.4 GB and PSI `some` stayed **0**; the
  same hog uncapped ran to 5 GB resident. **The layering this fixes:**
  - **RCTL per-jail cap** = proactive hard limit (containment). Present in our kernel
    (needs `kern.racct.enable=1`).
  - **PSI per-jail `some`/`full`** = the signal (which jail suffers). *Complementary,
    not redundant:* RCTL reports per-jail *usage*; PSI reports per-jail *stall*.
  - **`memoryd` cascade** = the RESIDUAL reactive layer — for **uncapped** jails,
    genuine **over-commit** (Σ caps > RAM), and **cross-jail lifecycle-tier**
    prioritization that per-jail RCTL rules don't see.
  - **The federation's real job = setting those RCTL caps DYNAMICALLY — BUILT +
    verified (`memfed`, #178).** `water_fill` the shared RAM by weight (floor +
    weighted elastic share, Σ ≤ budget) and push each jail's `memoryuse` cap via the
    rctl API; the cap is never set below current RSS (would kill), so an over-budget
    jail is frozen-not-killed. Verified: budgeted 8 GB over web(w3)+cache(w1) → caps
    6176/2016 MB; the SAME 4 GB hog was killed in cache but survived in web, PSI
    stayed 0. *Coordinated, not coupled* — `memfed` runs periodically to track
    demand/jail churn. (Not static admin rules.)
  Caveat: RCTL is cruder than cgroup-v2 `memory.max` (deny/sig, not
  reclaim-then-kill-within-the-cgroup).
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
6. **Tiers couple through the signal, not through wiring.** The compress tier (zram)
   and the destructive tier (memoryd) have no direct dependency: PSI-`full` measures
   stall *after* all reclaim, so a tier that absorbs pressure lowers `full` and the
   next tier stays dormant — coordination without coupling. Verified in-VM (#178): a
   12 GB compressible overcommit with zram swap on absorbed ~1.3 GB into ~7 MB
   (~185×) and both registry members survived. `full` *did* spike to ~63% during the
   fill — but the spike was transient (`thrash_run` reached 5 of the 6 s window, then
   `nstalled→0` reset it) and the `nstalled` gate suppressed the decaying-average
   tail (`full=62.9%` with `nstalled=0` is *not* thrash). zram's role in the coupling
   is to keep the workload alive (no OOM) and the stall *brief* (RAM-speed compressed
   I/O), so the residual `full` stays under the sustained-thrash window that slow
   flash swap would blow past.

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
   - **1c — per-jail attribution: `some` DONE + verified in-VM (#178).** Jails are
     the federation-member unit, so pressure is attributed per jail. Realized
     low-risk via a fixed 16-entry table (the `energy_members` pattern — no `pr_osd`
     lifecycle, no malloc in the stall path): each `vm_wait` stall is charged to the
     stalling thread's prison (`cr_prison->pr_id`), exposed at
     `kern.pressure.memory.jails` as `jail <jid> some_ns=<ns>`. Verified: a hog run
     via `jexec` in jail 2 attributed ~31.8 s to "jail 2" while the host (jid 0) did
     not appear. Limits (noted): bounded to 16; slots not reclaimed on jail destroy
     (a production version sweeps).
   - **1c — per-jail `full`: DONE + verified in-VM (#181).** A jail is fully stalled
     when it has a thread blocked on memory AND none of *its own* threads ran this
     sample — so a jail can be locally thrashing while the system overall progresses
     via a different jail (global `full` ⊆ every jail's `full`). Folded into the
     `pressure_sample_cpus()` walk: each running user thread marks its jail
     productive; a stalled jail with no productive thread of its own integrates the
     sample period into its `full_ns`. `full` is a subset of `some` but is sampled
     (vs `some` measured at the stall transitions), so it is clamped `≤ some` at the
     sysctl. Verified: a 12 GB hog in jail `ptest` accrued ~5.9 s `some` and `full`
     (equal — fully stalled whenever stalled), while the host running a busy loop
     showed `some=0.67 s` but `full=0` (degraded but never stalled). Textbook
     `some`-vs-`full`, per member.
   - **1c — kqueue edge-trigger: DONE + verified in-VM (#179).** The BSD-native
     realization of PSI's poll/trigger (Linux: `poll()` on `/proc/pressure/memory`
     with a written threshold). A `/dev/pressure` cdev with a `d_kqfilter` lets a
     controller register an `EVFILT_READ` knote whose `data` carries a `full`
     threshold in basis points (the sysctl unit — 40% = 4000); the existing 1 s
     aggregation callout `KNOTE`s the list, so a knote is active while
     `full_avg10 ≥` its threshold. The controller sleeps in `kevent()` with **zero
     wakeups** until the kernel pushes a pressure edge — no 1 Hz poll, no idle CPU.
     Low-risk: no scheduler hot-path hook, own leaf mutex, `KNOTE_UNLOCKED` after
     `pressure_mtx` drops (no lock-order coupling). memoryd uses it when the kernel
     exposes `full`, falling back to the poll otherwise. Verified: a standalone
     kevent test timed out idle (threshold 90%, zero spurious wakeups) and woke at
     `full=11.3%` under a hog (threshold 10%); memoryd stayed silent 5 s idle (2 log
     lines vs ~5 for a poller) then woke on the edge the instant `full` crossed 30%
     and ran the reap cascade, sparing the foreground. Remaining: per-jail `full`.
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
   - **DONE — built end to end + verified live as compressed swap in-VM (#178).**
     `atrium-zram/` (`atrium_zram.ko`), four increments, each verified before the next:
     - **Codec core** — `zram_*_page` over the kernel's in-tree zstd with an
       incompressible-page fallback. Resolved the hardest unknown: a leaf module *can*
       link the symmetric codec — the running kernel exports `ZSTD_compress`/
       `ZSTD_decompress` as global symbols (ZSTDIO compiled in), so no kernel rebuild,
       no zfs dependency. `kern.zram.selftest` → `OK: 4096 -> 55 bytes`.
     - **Pre-allocated `ZSTD_CCtx`/`DCtx`** (`ZSTD_compressCCtx`) — the one-shot
       `ZSTD_compress` mallocs per call, fatal on the reclaim path; contexts are
       allocated at module load and reused under a mutex.
     - **Compressed page store** — per-slot state machine SAME/COMP/RAW with
       zero/same-filled-page detection (a big fraction of the win, codec-free) and a
       RAW verbatim fallback when a page won't shrink below a page.
     - **Block device + swapon** — `disk(9)` `d_strategy` maps each page of a BIO to a
       store slot; verified first as a plain block device (`dd` round-trip of 200
       random pages MATCHED, unwritten blocks read zeros), *then* `swapon /dev/zram0`.
       Under a 12 GB hog with zram the only swap: **~1.42 GB of swapped pages held in
       ~8 MB physical** (~175×, the hog's pages mostly zero/same-filled), the hog
       round-tripped without crashing, and free RAM *recovered* as pages compressed —
       zram gave the system effective extra memory. No panic; clean `swapoff`.
     - Refinements (own efforts): per-CPU contexts (vs the single mutex'd one);
       dynamic device sizing; writeback of incompressible pages to real swap; wire the
       tier into the federation (memoryd/memfed) so reclaim *prefers* compress.
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
   - **Jail-aware tie-break (per-jail `full` consumer) — DONE + verified in-VM (#181).**
     memoryd reads `kern.pressure.memory.jails`, tracks each jail's `full_ns` delta,
     and names the *culprit* jail (the one whose `full` climbs fastest — locally
     stalled, not merely RAM-hungry). Members carry an optional jid. Tier still
     dominates, but within the lowest tier the tie-break prefers a member of the
     culprit jail over the largest-RSS member of a healthy jail — shedding the cause,
     not an innocent. Verified: two equal-tier-0 members (host vs jailed), a hog
     driving the jail → `culprit=jail 2`, memoryd shed the jail's member and spared
     the host's. Falls back to largest-RSS when no member maps to the culprit.
   - Remaining: the proactive dual — `memfed` consuming per-jail `full` to tighten a
     thrashing jail's RCTL cap (close the loop on the budgeter side too).

## 10. Non-goals / deferred

- **Memory-denominated fairness** — the governing principle forbids it (§2).
- **Per-jail NUMA / huge-page policy** — orthogonal; later.
- **Cross-device shared-buffer (dmabuf) accounting** beyond noting the GPU BO pool
  is a member — the GPU residency work already owns that mechanism
  ([[project_gpu_device_model_spec]]).
