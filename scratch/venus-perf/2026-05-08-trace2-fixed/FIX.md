# Venus latency fix — 187× speedup (2026-05-08)

## Result

| Metric | Before | After |
|---|---:|---:|
| Frame time (steady-state) | 62 ms | **0.33 ms** |
| Frame rate (cap) | 16 fps | **3000+ fps** |
| Per-`SUBMIT_3D` round-trip | 12.6 ms | <0.1 ms |

Verified via `frescod-vulkan-smoke` + `atrium-rect-bouncer`,
Apple M4 Max, FreeBSD 16.0-CURRENT guest, MoltenVK + venus + virglrenderer.

Pre-fix log:
```
frescod-vulkan-smoke: frame 0 ... total=109.293584ms (9.1 fps cap)
frescod-vulkan-smoke: frame 30 ... total=60ms+ (16 fps cap)
```

Post-fix log:
```
frescod-vulkan-smoke: frame 30 setup=333ns render=333.875µs total=334.292µs (2991.4 fps cap)
frescod-vulkan-smoke: frame 60 setup=375ns render=295.833µs total=296.25µs (3375.5 fps cap)
frescod-vulkan-smoke: frame 90 setup=250ns render=331.125µs total=331.458µs (3017.0 fps cap)
```

## Root cause

QEMU's venus path has two fence-retire mechanisms:

1. **Polling timer** (`virtio_gpu_fence_poll`, 10 ms interval). Default fallback.
2. **Async eventfd callback** (`virgl_write_async_context_fence` + bottom-half). Faster — no polling.

The async path requires `VIRGL_RENDERER_ASYNC_FENCE_CB` to be set in the
`virgl_renderer_init` flags. In `qemu-build/hw/display/virtio-gpu-virgl.c`,
this flag was gated on `if (qemu_egl_display)`:

```c
#if VIRGL_RENDERER_CALLBACKS_VERSION >= 4
    if (qemu_egl_display) {
        virtio_gpu_3d_cbs.version = 4;
        virtio_gpu_3d_cbs.get_egl_display = virgl_get_egl_display;
#if VIRGL_CHECK_VERSION(1, 1, 2)
        virtio_gpu_3d_cbs.write_fence         = virgl_write_async_fence;
        virtio_gpu_3d_cbs.write_context_fence = virgl_write_async_context_fence;
        flags |= VIRGL_RENDERER_ASYNC_FENCE_CB;
        flags |= VIRGL_RENDERER_THREAD_SYNC;
#endif
    }
#endif
```

On macOS hosts, `qemu_egl_display` is always `NULL` (no EGL on Darwin —
QEMU uses Cocoa for display). So the async path was never enabled, and
every venus command's fence-retire waited up to 10 ms for the next
poll-timer fire.

The EGL gate is a holdover from when async fences were entangled with
the GL/vrend code path. For venus-only operation, only `write_context_fence`
matters and it works fine without EGL — the fence callback fires in
QEMU's main thread via `qemu_bh_schedule`, no display state required.

## Fix

In `virtio_gpu_virgl_init` (`qemu-build/hw/display/virtio-gpu-virgl.c`),
add a venus-specific path that enables async fence callbacks even
without EGL:

```c
#if VIRGL_CHECK_VERSION(1, 1, 2)
    /* Atrium patch: enable async fence callback for venus even without
     * EGL. Without it, QEMU falls back to a 10ms polling timer
     * (virtio_gpu_fence_poll) which adds ~10ms to every venus command's
     * round-trip latency. EGL is only required for vrend's GL fence
     * path; venus needs only write_context_fence which works fine
     * without EGL. */
    if (!qemu_egl_display && virtio_gpu_venus_enabled(g->parent_obj.conf)) {
        virtio_gpu_3d_cbs.version = 4;
        virtio_gpu_3d_cbs.write_context_fence = virgl_write_async_context_fence;
        flags |= VIRGL_RENDERER_ASYNC_FENCE_CB;
        flags |= VIRGL_RENDERER_THREAD_SYNC;
    }
#endif
```

Diff is ~10 lines. No virglrenderer changes required (we used the
existing `virgl_write_async_context_fence` callback, which already
handles the venus per-context fence path via `qemu_bh_schedule`).

## How we found it

Cross-boundary tracing infrastructure (`atrium-trace`) — Chrome Trace
Format JSON probes across all 4 layers (kmod, QEMU, virgl_render_server,
guest userspace), correlated by `fence_id` to bypass cross-host/guest
clock skew.

Single round-trip timeline (pre-fix):

```
+0.000 ms  QEMU process_cmd                        (cmd dispatched)
+0.012 ms  proxy.submit_cmd_send E                 (sent to worker, 9 µs)
+0.020 ms  worker.submit_cmd_recv                  (worker received, 5 µs cross-proc)
+0.032 ms  worker.update_timeline                  (fence ready)
+0.048 ms  worker.ring.dispatch E                  (worker done — 48 µs!)
+1.073 ms  worker.ring.idle_wait B                 (worker back to sleep)
+10.254 ms QEMU venus.fence.retire                 ← 10 ms WAIT for poll
```

The worker completed in 48 µs. The remaining 10.2 ms was QEMU's
poll-timer interval. Once we saw this gap with **no** scheduler activity
on either side, it was clearly a timer, not a wake-up issue.

## Hypotheses we ruled out

| Suspect | Evidence | Verdict |
|---|---|---|
| HVF VM-exit/entry latency | D+E = 82 µs total | ❌ negligible |
| macOS scheduler wake of worker process | venus.ring.notify→dispatch = 5 µs | ❌ negligible |
| `render-server-worker=process` IPC overhead | Switching to `thread` mode → no change | ❌ ruled out |
| QEMU's `virgl_render_server` thread QoS | `pthread_set_qos_class_self_np(USER_INTERACTIVE)` on 4 threads → no change | ❌ ruled out |
| MoltenVK per-submit encode cost | 0.16 ms p50 inside MVK | ❌ < 1% of budget |
| Metal GPU exec | 0.48 ms p50 | ❌ < 1% of budget |

## Files

- `merged.json` — Chrome Trace JSON of the post-fix run (572 events; mostly
  init since steady-state is so fast there's barely anything to record in 3s)
- `kmod.dump` — minimal post-fix kmod ring (4 ctrl_intr events; sub-frame
  command rate is now below the bouncer's 30 Hz send rate)

The pre-fix decomposed trace lives at `../2026-05-08-trace1-decomp/`
with all 4 layers fully populated.

## Open questions / follow-ups

- **Upstream the patch.** This affects every venus user on macOS hosts
  (and any non-EGL host). Worth a QEMU PR.
- **Test on Linux** (with EGL) — should be a no-op there since the gate
  is `!qemu_egl_display`.
- **Remove the polling timer entirely?** Once async fence is the
  default, the 10 ms timer just adds a (rare) safety-net wakeup for
  workloads where the worker's fence-eventfd write somehow doesn't
  reach QEMU. Probably leave it as a 100 ms backstop rather than
  remove.
- **Why does the kmod show only 4 ctrl_intr in the post-fix run?**
  Suspect: with async fences, mesa-venus sends most commands via the
  shared-memory ring directly (no kmod hop), and only does
  IOC_SUBMIT_3D for fence-bearing commands. Worth a follow-up trace
  with longer wallclock to confirm. Doesn't affect the headline win.
- **Pipelined frame submission** would push beyond the 3000 fps
  display-driven cap into "what is venus actually capable of" territory.
  Out of scope for this fix.
