# Venus latency trace — findings (2026-05-08)

End-to-end cross-boundary trace of `frescod-vulkan-smoke` + `atrium-rect-bouncer`
under `--venus` on Apple M4 Max, FreeBSD 16.0-CURRENT guest under HVF.

## Pipeline traced

```
┌──────── guest ────────┐    ┌──────── macOS host ────────┐
frescod-vulkan-smoke
  ↓ vkQueueSubmit
mesa-venus driver
  ↓ ring_submit                  virgl_render_server
  ↓ SUBMIT_3D ioctl              ↑ vkr_ring_thread (cnd_wait)
atrium-virtio-gpu kmod   ──→    QEMU virtio-gpu  ──→  vkr_dispatch_vkQueueSubmit
  ↓ virtqueue_notify(ctrl_vq)                          ↓ vk->QueueSubmit
  ↓                                                    MoltenVK MVKQueue::execute
  ↓ cv_wait(ctrl_done)                                 ↓ [mtlCB commit]
                                                       Metal GPU
  ↑ cv_signal                                          ↓ addCompletedHandler
atrium_vgpu_ctrl_intr     ←──   QEMU virtio-gpu  ←──  per_context_fence_retire
  ↑ virtqueue_dequeue              ↑ HVF inject IRQ
  ↑                                ↑ write_context_fence
mesa-venus driver
  ↑ vkWaitForFences returns
frescod-vulkan-smoke
```

## Run parameters

- `frescod-vulkan-smoke` + `atrium-rect-bouncer` driving 60 frames
- 1280×720 render target, compute + indirect-draw bundle, no PNG (`FRESCOD_SMOKE_NO_PNG=1`)
- Wall time: 3.69 s, **16.2 fps**
- Frame median: **62 ms** (matches the V7 perf characterization in RUNBOOK.md exactly)

## Counts within the 60 frames

| Event                   | Count | Per-frame |
|-------------------------|-------|-----------|
| `kmod.submit_3d_enter`  |   299 |    **4.98** |
| `kmod.vq_notify`        |   299 |    4.98 |
| `kmod.ctrl_intr`        |   300 |    5.00 |
| `venus.ring.notify`     |   262 |    4.37 |
| `venus.ring.dispatch`   |   262 |    4.37 |
| `venus.QueueSubmit`     |    52 |    0.87 |
| `mvk.execute`           |    53 |    0.88 |
| `mvk.metal.commit`      |    53 |    0.88 |
| `mvk.gpu.completed`     |    53 |    0.88 |
| `venus.fence.retire`    |   265 |    4.42 |

## Headlines (single clock domain, trustworthy)

| Phase                                          | p50 (ms) | p99 (ms) |
|------------------------------------------------|---------:|---------:|
| `mvk.execute` (`vkQueueSubmit` total inside MVK) |    0.157 |    2.463 |
| `mvk.execute` → `mvk.metal.commit` (encode)    |    0.144 |    2.415 |
| `mvk.metal.commit` → `mvk.gpu.completed` (Metal GPU exec) | **0.485** |  8.577 |
| `venus.ring.notify` → `venus.ring.dispatch` (host worker wake) | 0.021 |  0.064 |

The actual GPU work and Vulkan API time are **negligible** — under
1 ms total per submit. Metal is fast. MoltenVK is fast. virgl_render_server
worker wake is fast.

## Root cause

**~5 SUBMIT_3D round-trips per frame × ~12 ms each = ~60 ms per frame.**

Each guest→host→guest round-trip costs ~12 ms wall-clock regardless of
whether actual GPU work happens — most of these crossings carry venus
ring metadata (descriptor heap updates, resource ops, etc.), not real
`vkQueueSubmit`. The 12 ms is paravirt overhead:

- HVF VM exit on virtqueue notify
- macOS scheduler wakes the `vkr-ring-N` worker thread
- Host processes the command (often trivial ≪ 1 ms)
- QEMU's virtio-gpu schedules an IRQ on the fence-retire callback
- HVF VM entry to inject the IRQ on the guest vCPU
- Guest scheduler wakes the `cv_wait` thread in the kmod

Each crossing is a cache-miss / scheduler / TLB / GIC event. None of them
individually are slow, but five of them per frame × two-way traversal
adds up to the 60 ms wall.

The ratio of paravirt overhead (~60 ms) to actual host work (~1 ms) is
**60×**. This matches the 10–60× gap to native Linux+Venus+Linux host
that the RUNBOOK.md V7 note already called out, and is consistent with
HVF-on-macOS scheduler-wake latency on an idle system.

## Confirmed *not* the bottleneck

- ❌ Metal command-buffer encoding cost (≤ 0.2 ms p50 inside MVK)
- ❌ Metal GPU exec (0.5 ms p50)
- ❌ MoltenVK per-submit overhead
- ❌ virgl_render_server worker wakeup (0.02 ms p50)
- ❌ Vulkan API / scene-build cost (lavapipe baseline shows < 1 ms)

## Likely fixes, ranked

### A. **Reduce SUBMIT_3D count per frame** (biggest win, 5× speedup ceiling)

Each frame currently triggers ~5 `kmod.submit_3d_enter` events. Cutting
that to 1 would drop frame time from ~62 ms to ~14 ms — **45 fps
ceiling on a strict-serial harness**, and much higher on a pipelined one.

Specific approaches in priority order:

1. **Mesa-venus async ring batching.** Most of the 5 SUBMIT_3D's are venus
   ring metadata commands (descriptor heap, resource ops) that don't need
   a host fence. They're being sent synchronously today. Sending them
   async (no SUBMIT_3D until the next sync point) coalesces them into the
   one real `vkQueueSubmit`'s SUBMIT_3D.
2. **Smoke harness: combine compute + draw into one cmdbuf, one submit.**
   Drops `vkQueueSubmit` count from 2 to 1 per frame. Quick test,
   modest win (~12 ms / frame).
3. **Smoke harness: pipeline two frames.** Submit frame N+1 while waiting
   on frame N's fence. Hides host round-trip latency behind GPU exec.
   Quick test, big apparent fps win even if per-frame latency unchanged.

### B. **Reduce per-round-trip latency** (secondary, complements A)

1. Bump `virgl_render_server` ring-thread QoS to `USER_INTERACTIVE`
   (`pthread_set_qos_class_self_np`). Currently default. Could shave
   1–3 ms off worker-wake latency.
2. Pin the QEMU vCPU and ring-thread to specific P-cores via affinity
   APIs to avoid bouncing across performance/efficiency cores.
3. Investigate whether QEMU's `-accel hvf` has flags or knobs for IRQ
   delivery batching (HVF API exposes `hv_vcpu_run_until` for tighter
   loops).

### C. **Reduce paravirt fundamentals** (long-term)

- Switch from synchronous `cv_wait` in `atrium_vgpu_submit_3d` to true
  async-IRQ-driven completion (already noted as deferred in
  `atrium_virtio_gpu.c`'s `IOC_FENCE_WAIT` comments). With async, multiple
  guest submits can be in flight concurrently, eliminating serialization
  of paravirt round-trips.

## Instrumentation caveats / TODO

- **Cross-host/guest clock skew is not corrected.** Host and guest
  both write `CLOCK_REALTIME` ns, but the two clocks can disagree by
  hundreds of ms under HVF. Cross-domain `B → E` matching (e.g.
  `kmod.vq_notify → venus.ring.notify`) shows nonsensical negatives
  in the merged trace. All conclusions in this doc are derived from
  *single-domain* event pairs.
- **Kmod uses `getnanotime()`** which on FreeBSD has 1/HZ granularity
  (1 ms by default). All in-kmod intervals report as 0.000 ms. Need to
  switch to `nanotime()` (hardware clock read) for sub-µs precision.
  Filed as next-iter improvement; doesn't change the headline since
  the dominant cost is *not* in-kmod.
- **Frame matching to per-event causality is approximate** — pairing
  by "next event after" is wrong when multiple ring threads concurrently
  produce events. Single-domain phase durations are reliable.

## Files in this trace

- `merged.json` — Chrome Trace JSON (5673 events, 4 processes)
- `kmod.dump` — raw kmod ring buffer text dump
- `atrium-trace-guest.json.3010` — frescod-vulkan-smoke (guest userspace)
- `atrium-trace-host.json.81448` — QEMU process (host)
- `atrium-trace-host.json.81630` — virgl_render_server (host fork from QEMU)

Open `merged.json` in https://ui.perfetto.dev for visual flame-trace.
