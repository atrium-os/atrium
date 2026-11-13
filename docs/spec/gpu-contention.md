# GPU contention: flow control, arbitration, and the compositor under a hog

> **Status:** write-up, 2026-11-13, after stage 5c-2 (MES user queues) passed on the
> R9700. It answers two questions. First, what happens when two heavy apps compete
> for the GPU? Second, what happens when one of the apps the compositor is
> compositing is itself the hog? It collects what is already specified in
> [GPU ABI v2.2](atrium-gpu-abi-v2.md) (principle 7, §4.2, §5.7, §10),
> [`atrium-gpu-scheduler.md`](atrium-gpu-scheduler.md) (§3–§6: admission, preemption
> granularity, deadlines), [`fresco-surfaces.md`](fresco-surfaces.md) (§3 frame pacing)
> and [`atrium-scenegraph-hardening.md`](atrium-scenegraph-hardening.md) (H6 cost bound),
> checks it against the driver as it stands on `main` (`8ab8f21f`), and names the gaps.
> Companion to [`gpu-security-model.md`](gpu-security-model.md). Where it says
> "required", that is a design rule this note proposes; it is not yet enforced.

## 1. The short version

- **Arbitration does not live in `atrium-gpu-rs`.** That library runs inside the
  caller's process, and anything a library enforces, a hostile or buggy client skips.
  Each job sits in a layer the client cannot bypass:
  firmware (time), kernel (limits), broker (policy).
  The library's only job is to honour backpressure.
- **Two heavy apps today:** each gets about half the GPU, round-robin. Neither starves,
  but nothing knows which one matters more. Memory goes to whoever allocates first,
  up to the quota, and nothing is reclaimed.
- **The compositor under a hog:** only frescod's own composite runs at top priority.
  The hog's work, including work frescod does *for* the hog, stays at the hog's
  priority, in the hog's context, charged to the hog's quota. The compositor
  composites only finished frames and never blocks on a client. A hog makes its own
  window stutter, not the desktop.
- **The missing pieces** are wiring and hardware: MES priority levels (the kmod
  accepts only priority 0 today), weights reaching the firmware, and compute wave
  save/restore (CWSR) so a long dispatch can be preempted.

## 2. Where each job lives

| Layer | Job | Mechanism | State on `main` |
|---|---|---|---|
| **MES firmware** | Splits GPU *time* between the queues that are mapped | Process quantum 10 ms, gang quantum 1 ms (`mes12.c` ADD_QUEUE); priority level per queue | Built; every client at NORMAL (`si_client.c:769`) |
| **Kernel** (`atrium-gpu-amd`) | Enforces *limits*; does not schedule (ABI principle 7) | Quotas → `EDQUOT` (§4.2); per-uid caps `vram_limit_pct` 75 / `sys_limit_pct` 50; 16 signalled submits in flight per queue (`ATRIUM_AMD_MAX_PENDING`) → `EBUSY`; hang watchdog `hang_ms` 5000 → wave kill, HQD reset, REMOVE_QUEUE (§10); revocable queue mapping (scheduler §3) | Built except priority mapping (`rs/uqueue.rs:167` refuses `priority != 0`) |
| **Broker** (frescod / `aqueduct-gpu-host`) | Sets *policy* | Holds the control fd; mints each client context with a `sched_weight`; `SCHED` ioctl: SET_WEIGHT, SET_PRIORITY (top class reserved for the control plane), DEADLINE_WINDOW, WATCHDOG, RESET_BUDGET; admission (it can queue, throttle or refuse work before it reaches the hardware) | ABI specified; ioctl not wired to MES yet |
| **Laminar / power posture** | Feeds the broker's weights | Per-jail weights; energy cap via the federation (`atrium-scheduler-federation.md`) | Design |
| **`atrium-gpu-rs`** (in the caller) | *Cooperates* | On `EBUSY`, waits on the syncobj kevent instead of spinning; passes the `deadline_ns` hint (§5.7) | Library behaviour; not a guarantee |

In production no app holds a GPU fd (ABI principle 13). Every app's GPU work reaches
the hardware through a broker, so the broker is a real choke point for admission.
The kernel and firmware limits still apply underneath it.

## 3. Two heavy apps today

- **Time.** Both get queues within their quotas. Each VM is its own MES process
  context at the same (NORMAL) priority, so MES round-robins them on the 10 ms process
  quantum. Neither starves; each runs at about half speed. That is fair but blind:
  the firmware does not know which app the user is looking at.
- **Preemption granularity.** No CWSR is wired yet, so switches happen between
  packets. A single long dispatch holds its pipe until it finishes. If it makes no
  progress for 5 s, the watchdog kills it. That bounds a runaway, but it also kills
  a *legitimate* compute job that dispatches a single 6 s kernel.
- **Memory.** First come, first served, up to the jail quota (`EDQUOT`) or the device
  (`ENOMEM`). No eviction in V1 (§4.2, §13h): the second app is refused, and nothing
  is taken back from the first.
- **Submission.** A queue with 16 completions outstanding gets `EBUSY`. That is
  per-queue backpressure: it caps how far one queue can run ahead, not how much GPU
  it gets.

## 4. The compositor when the hog is one of its clients

A compositor-class queue must make its frame deadline. Suppose one of the apps it
composites is the hog. The rules below keep the deadline without handing the hog
the compositor's priority.

### 4.1 Separate contexts, separate priorities (required)

frescod mints a GPU context for each client app, at that app's weight and priority.
**Every piece of GPU work done on an app's behalf runs in that app's context and is
charged to that app's quota.** That includes work frescod itself issues, such as
rasterising the app's retained scene graph or running its F6 code. frescod's
top-priority queue does only the final composite: it samples finished surfaces and
draws the desktop, cursor and decorations. That is a small, bounded job.

The failure this rule prevents is priority inversion through the broker. If frescod
rendered app content on its own top-priority queue, every app, including the hog,
would inherit the compositor's priority.

> **Audit needed.** I have not checked whether frescod today renders client scene
> graphs in a per-client GPU context or in its own. Rule 4.1 is not true until that
> is confirmed or changed.

### 4.2 Composite only what is finished; never block on a client fence (required)

At each composite tick, frescod takes, for each surface, the newest frame whose
completion point (the surface's timeline semaphore / syncobj value) has *already*
signalled. If the hog's latest frame has not finished, frescod reuses the hog's
previous frame and composites on time anyway.

Explicit sync makes this possible: frescod polls a fence state (or reacts to its
kevent); it never puts a client's fence in its own GPU queue's wait path.

> **Spec clarification needed.** `fresco-surfaces.md` §2.3 says the server "waits for
> that value before sampling". §3.1 already says a missed deadline reuses the previous
> frame. The wording should become: *the server samples a frame only once its value
> has signalled; it never waits on it, on the CPU or in a GPU queue.*

### 4.3 Pace the hog from the compositor (flow control at the protocol)

In pull mode (`fresco-surfaces.md` §3.1) frescod asks for each frame with
`SURFACE_FRAME_REQ`. An app whose previous frame has not signalled gets no new
request. In push mode only the newest finished frame is used, and older ones are
skipped. Either way, a cooperative app slows down to what it can finish. A
non-cooperative app that keeps submitting runs into the kernel limits beneath it:
`EBUSY` at 16 in flight, its quota, and its weight.

### 4.4 The hardware must let the composite in

Rules 4.1–4.3 fix *scheduling*. But when the composite is ready to run, the hog may
already occupy every compute unit. Something has to make room:

1. **MES priority levels.** frescod's composite queue goes on the top level, which
   the ABI reserves for the control plane (SET_PRIORITY). Today the kmod refuses any
   priority other than 0.
2. **Preemption inside a dispatch (CWSR).** Without it, the composite can be delayed
   by up to one of the hog's dispatches. `atrium-gpu-scheduler.md` §4 sets out the
   trade-off: CWSR costs a per-queue VRAM save area, charged to the quota, and memory
   traffic for each switch. §6 says how the deadline lowers the controller's
   reluctance to pay it.
3. **Optional: reserved compute units.** A small set of compute units held for the
   compositor's queue (CU masking). This gives guaranteed headroom without
   preemption, at a small cost in throughput. It is worth measuring on the R9700
   before deciding.

### 4.5 A hog that keeps it up

- **Lower its weight.** The broker lowers it, or Laminar and the power posture lower
  it automatically, for example for unfocused windows.
- **Kill only its queue.** If it never yields, the watchdog and RESET_BUDGET (§10)
  take down its queue, never frescod's.
- **Report it.** Missed frames per surface are telemetry the broker can show and act
  on.

### 4.6 The composite itself must stay cheap

The one real dependency: frescod's own top-priority pass must be cheap and
predictable. Under the hostile profile, a crafted scene graph could otherwise make
the *composite* the hog. `atrium-scenegraph-hardening.md` H6 already requires a cost
model and a per-client budget, with refusal in preference to unbounded work. For
work that runs on the compositor's queue, that budget is a deadline guarantee, not
just a fairness measure. Anything over budget is rendered in the client's own
context (rule 4.1), not in the composite.

## 5. Gaps, in the order I'd close them

1. **Priority mapping.** SET_PRIORITY and QUEUE_CREATE's `priority` mapped onto MES
   priority levels, with the top level reserved to the control fd. This lifts the
   `priority != 0` refusal in `rs/uqueue.rs`. It is the main protection for
   interactivity.
2. **Rule 4.1 audit and enforcement.** Confirm that frescod renders client content in
   per-client contexts; fix it if not.
3. **Weights to firmware.** `sched_weight` (CLIENT_OPEN) and SET_WEIGHT reach the MES
   process quantum and priority. Laminar and the power posture supply them per jail.
4. **CWSR.** Wave-granular preemption with a quota-charged save area for interactive
   and compositor queues. Batch queues stay packet-granular (scheduler §4).
5. **A long-running compute flag.** The bit is already reserved in `SUBMIT_FEATURES`.
   It exempts a queue from the 5 s hang clock in exchange for a lower priority and
   mandatory preemptibility.
6. **Memory pressure.** A device-wide budget that the broker splits. Later, eviction
   through MES SUSPEND/RESUME, which is the same free rule as §5.11.
7. **GPU-time accounting.** Report each context's busy time (from MES or the kernel)
   to RCTL and the broker, so that hogging is *measured*, not inferred from weights.
8. **`fresco-surfaces.md` wording** (§4.2 above).

## 6. What this note does not change

No ABI change is proposed here. Everything above uses fields and ioctls already in
ABI v2.2 (`sched_weight`, `priority`, `SCHED`, `deadline_ns`, the reserved
long-running bit). The work is wiring them to the firmware, plus two
protocol-discipline rules for frescod (4.1, 4.2).
