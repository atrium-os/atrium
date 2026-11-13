# GPU arbitration and flow control on silicon

**Status:** design rules plus a gap list, 2026-11-13, written during the R9700
bring-up (stage 5f). One rule is agreed (Shankar): a compositor-like queue must
meet its frame deadline while another client hogs the GPU. The rest are
proposals for the stage after 5f. The theory (two-level admission and time
slicing, preemption granularity, the energy-denominated controller) is in
[`atrium-gpu-scheduler.md`](atrium-gpu-scheduler.md) §3-§6. The ABI hooks are
in [`atrium-gpu-abi-v2.md`](atrium-gpu-abi-v2.md): principle 7 (no scheduler in
the kernel), §4.1-§4.2 (contexts, weights, quotas), `SCHED` (§5), §6.4
(mixed-criticality rules) and §10 (watchdog, reset budget). This document says
what applies on the R9700 today, what happens when two heavy clients compete,
and what is still missing.

## 1. Who does what

Arbitration lives in layers the client cannot bypass.

| Layer | Job | Enforces? |
|---|---|---|
| MES firmware | Time-slices queues: each queue is a MES process and gang; preemption; per-queue reset | yes (mechanism) |
| Kernel (`atrium-gpu-amd`) | Limits, never arbitration: per-uid/jail memory caps (`EDQUOT`), in-flight cap per queue (`EBUSY`, §2), hang watchdog, free rule, revocable doorbells | yes (limits) |
| Broker (frescod, `aqueduct-gpu-host`) | Policy: mints one client context per app with a weight; `SCHED` priorities, weights, deadline windows, watchdog, reset budget; admission and frame pacing | yes (policy), through the control fd |
| `atrium-gpu-rs` | Cooperates: on `EBUSY` it waits on the syncobj instead of spinning; passes `deadline_ns` through | no |

`atrium-gpu-rs` cannot enforce anything because it runs inside the calling
process, so a hostile client simply skips it. Apps hold no GPU fd in
production (ABI principle 13), so every submit already passes through a
broker. That makes the broker the admission point.

## 2. Two heavy clients today (R9700, stage 5f)

- **Time.** Both get MES user queues at NORMAL priority with a 10 ms process
  quantum and a 1 ms gang quantum (`mes12.c`, ADD_QUEUE). MES round-robins
  them, and each runs at about half speed. That is fair but blind: it does not
  know which one matters.
- **Preemption.** Compute wave save/restore (CWSR) is not set up, so
  preemption is at best between dispatches. One long dispatch holds the pipe
  until it ends, or until the watchdog stops that queue after `hang_ms`
  (default 5 s). This is inferred from the setup, not measured.
- **Memory.** First come, first served, capped per uid at 75 % of VRAM and
  50 % of RAM (`isolation.vram_limit_pct`, `sys_limit_pct`). There is no
  eviction, so a second big client gets `EDQUOT` or `ENOMEM`; nothing is
  reclaimed from the first.
- **Backpressure.** At most 16 signalled submits in flight per queue
  (`ATRIUM_AMD_MAX_PENDING`); past that, submit returns `EBUSY`. This is the
  gpusim engine queues' limit; whether silicon queue fds apply the same cap
  is not yet checked (gap 8).
- **Policy.** `SCHED` works on gpusim but returns `EOPNOTSUPP` on silicon
  (the `si_client.c` backend has no sched op). The kmod refuses any queue
  priority other than 0. `deadline_ns` is passed through and ignored.
- **The compositor** competes at the same priority as everyone else. This is
  the gap a user would notice first.

## 3. The compositor and the hog

The hard case: the GPU hog is one of the apps the compositor is compositing.
Five rules keep the hog's cost on its own window.

1. **Never wait on a client fence.** At each frame's latch point the
   compositor takes each window's newest buffer whose completion fence has
   already signalled. If the hog has not finished its frame, the compositor
   reuses the hog's previous buffer and composites on time. Explicit sync
   (ABI §5.6) is what allows checking a fence without blocking on it. The
   hog's window drops frames; the cursor, the other windows and the desktop
   don't.
2. **Separate contexts, separate priorities: no priority inversion.** Any GPU
   work done *for* an app runs in that app's own context, at that app's
   priority, charged to that app's quota. That includes frescod rendering the
   app's retained scene graph. frescod's top-priority queue does only the
   final composite: sampling finished surfaces and drawing the desktop. If
   frescod rendered app content on its top-priority queue, the hog would
   inherit top priority.
3. **Pace from the protocol.** An app that is behind gets no new frame
   callback until its last frame lands. A cooperative app slows down before it
   reaches the kernel's `EBUSY`; an uncooperative one runs into `EBUSY` and its
   quota.
4. **The hardware must let the compositor in.** Rules 1-3 handle scheduling,
   but the hog can still occupy every compute unit when the composite is
   ready. Three things clear room:
   - MES priority levels, with the top class (ABI §6.4 rule 4) for the
     compositor's queue;
   - CWSR, so a long hog dispatch can be preempted at a wave boundary
     ([`atrium-gpu-scheduler.md`](atrium-gpu-scheduler.md) §4 explains why the
     floor is otherwise the hog's longest dispatch);
   - optionally, a few compute units reserved for the compositor's queue, as
     guaranteed headroom at a small throughput cost.
5. **Keep the composite pass cheap, and deal with sustained hogs.** frescod
   caps how much composite work any one app's tree can cost (the hostile-profile
   scene-graph rules), so a crafted scene graph cannot turn frescod's own pass
   into the hog. A client that keeps missing deadlines gets a lower weight from
   the broker (Laminar and the power posture can do this automatically, for
   example for unfocused windows). One that never yields is stopped by the
   watchdog and the reset budget (ABI §10), which lose only its queue, never
   frescod's.

## 4. Gaps, in the order to fix them

1. **Priorities.** Map `SET_PRIORITY` onto MES priority levels (in-process and
   gang priority in ADD_QUEUE). Reserve the top class for the control plane's
   compositor queue. This is the main protection for interactivity.
2. **Weights.** Turn `CLIENT_OPEN`'s `sched_weight` and `SET_WEIGHT` into MES
   process quanta, set per jail by Laminar and the power posture.
3. **CWSR.** Per-queue save area in VRAM (charged to the owner's quota), so
   preemption is at wave boundaries. Add a "long-running compute" context flag
   (`SUBMIT_FEATURES` reserves a bit): it exempts a queue from the hang clock
   in exchange for a lower priority, so legitimate long compute is not killed
   at 5 s.
4. **Deadlines.** Act on `deadline_ns` / `DEADLINE_WINDOW` (raise a queue's
   priority as its frame deadline nears, per
   [`atrium-gpu-scheduler.md`](atrium-gpu-scheduler.md) §6).
5. **Memory pressure.** A device-wide budget the broker splits between
   contexts, then eviction through MES SUSPEND/RESUME (the free rule already
   uses it).
6. **GPU-time accounting.** Per-context busy time, from MES or the kernel, so
   the broker and RCTL see who is hogging instead of trusting weights alone.
7. **frescod audit.** Check that frescod renders app content in each app's
   context (rule 2) and never puts an unsignalled client fence on its own
   critical path (rule 1), before its GPU renderer runs on silicon.
8. **In-flight cap on silicon.** Confirm (or add) the per-queue cap of
   signalled submits for MES queue fds, so a client cannot queue unbounded
   completions.

## 5. Acceptance test

A contention test on the R9700, extending `atrium-gpu-rs`'s `mes_contention`
bin:
- a hog client submits back-to-back long dispatches;
- a compositor-like client submits a 1 ms job every 16.7 ms at the top
  priority.

It passes when the compositor job's completion latency stays under its
deadline, which is only possible after gaps 1 and 3, while the hog still makes
progress. Report each client's busy share and the worst-case compositor
latency. Run it before and after CWSR so the improvement is measured, not
assumed.
