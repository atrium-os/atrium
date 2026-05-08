#!/usr/bin/env python3
"""Quick analytic dump of a merged atrium-trace JSON.

Usage:
    analyze.py <merged.json>

Prints per-frame breakdown:
  - frame index (by guest.render_to_buffer matched begin/end)
  - duration
  - sub-phase durations / instant timestamps relative to frame begin

Identifies the dominant phase across frames.
"""
import json, sys, statistics, collections

if len(sys.argv) != 2:
    print(__doc__); sys.exit(2)

with open(sys.argv[1]) as f:
    data = json.load(f)

events = sorted(data["traceEvents"], key=lambda e: e.get("ts", 0))

# Group by (pid, name) to reconstruct B/E pairs
def pair_durations(name):
    """Return list of (begin_ts, end_ts) tuples for matched B/E."""
    pairs = []
    stack_by_tid = collections.defaultdict(list)
    for e in events:
        if e.get("name") != name: continue
        ph = e.get("ph")
        tid = (e.get("pid"), e.get("tid"))
        if ph == "B":
            stack_by_tid[tid].append(e["ts"])
        elif ph == "E":
            if stack_by_tid[tid]:
                begin = stack_by_tid[tid].pop()
                pairs.append((begin, e["ts"]))
    return pairs


# Frames defined by guest.render_to_buffer
frames = pair_durations("guest.render_to_buffer")
print(f"Frames captured: {len(frames)}")
if not frames:
    sys.exit(1)

durs_ms = [(end - beg) / 1000.0 for beg, end in frames]
print(f"render_to_buffer (full frame, ms): "
      f"min={min(durs_ms):.2f} median={statistics.median(durs_ms):.2f} "
      f"mean={statistics.mean(durs_ms):.2f} max={max(durs_ms):.2f}")
print(f"  steady-state median × {len(frames)} frames = {statistics.median(durs_ms) * len(frames) / 1000:.2f}s wall")
print()

# All B/E label pairs
labels = sorted({e["name"] for e in events if e.get("ph") in ("B", "E")})
print("Phases (B/E pairs) — median ms across all instances:")
rows = []
for lbl in labels:
    p = pair_durations(lbl)
    if not p: continue
    ds = [(end - beg) / 1000.0 for beg, end in p]
    rows.append((lbl, len(p), min(ds), statistics.median(ds), max(ds), sum(ds)))
rows.sort(key=lambda r: -r[3])  # by median desc
print(f"{'phase':<35} {'count':>6} {'min ms':>8} {'med ms':>8} {'max ms':>8} {'total ms':>10}")
for r in rows:
    print(f"{r[0]:<35} {r[1]:>6} {r[2]:>8.2f} {r[3]:>8.2f} {r[4]:>8.2f} {r[5]:>10.1f}")
print()

# Per-frame breakdown for the first 5 + last 5 frames, splitting host vs guest events
def events_in_window(beg, end):
    return [e for e in events if beg <= e.get("ts", 0) <= end]

print("Per-frame timing relative to render_to_buffer begin (ms; only key labels):")
key_labels = [
    "guest.compute.submit", "guest.compute.wait_fence",
    "guest.draw.submit", "guest.draw.wait_fence",
    "kmod.submit_3d_enter", "kmod.vq_notify", "kmod.submit_3d_woke", "kmod.ctrl_intr",
    "venus.ring.notify", "venus.ring.dispatch", "venus.QueueSubmit", "venus.QueueSubmit2",
    "mvk.execute", "mvk.metal.commit", "mvk.gpu.completed", "venus.fence.retire",
]
for fi, (beg, end) in enumerate(frames[:5] + frames[-5:]):
    if fi == 5: print(f"  --- last 5 ---")
    win = events_in_window(beg, end)
    by_label = collections.defaultdict(list)
    for e in win:
        if e.get("name") in key_labels:
            by_label[e["name"]].append((e["ts"], e.get("ph", "?")))
    print(f"  frame {fi}: dur={ (end - beg) / 1000:.2f}ms")
    for lbl in key_labels:
        if lbl in by_label:
            for (t, ph) in by_label[lbl][:4]:
                rel = (t - beg) / 1000.0
                print(f"    {lbl:<32} ph={ph} +{rel:6.2f} ms")
            if len(by_label[lbl]) > 4:
                print(f"    {lbl:<32} ... +{len(by_label[lbl])-4} more")
print()

# Latency between key host/guest boundary events per frame
def between(window, lbl_a, lbl_b):
    ta = next((e["ts"] for e in window if e.get("name") == lbl_a), None)
    tb = next((e["ts"] for e in window if e.get("name") == lbl_b), None)
    if ta and tb and tb > ta:
        return (tb - ta) / 1000.0
    return None

print("Critical-path gaps per frame (median across all frames, ms):")
gap_defs = [
    ("kmod.vq_notify", "venus.ring.notify",     "guest_kick → host_ring_notify"),
    ("venus.ring.notify", "venus.ring.dispatch", "host_ring_notify → ring_dispatch"),
    ("venus.QueueSubmit", "mvk.execute",         "vkr_QueueSubmit → MVK.execute"),
    ("mvk.metal.commit", "mvk.gpu.completed",    "Metal commit → GPU completed"),
    ("mvk.gpu.completed", "venus.fence.retire",  "GPU completed → fence_retire"),
    ("venus.fence.retire", "kmod.ctrl_intr",     "host_fence_retire → guest IRQ"),
    ("kmod.submit_3d_enter", "kmod.submit_3d_woke", "kmod_submit → kmod_wake (full guest blocking)"),
]
for a, b, desc in gap_defs:
    gaps = []
    for beg, end in frames:
        win = [e for e in events if beg <= e.get("ts", 0) <= end]
        v = between(win, a, b)
        if v is not None:
            gaps.append(v)
    if gaps:
        print(f"  {desc:<55} count={len(gaps):>3}  med={statistics.median(gaps):>6.2f} ms  "
              f"min={min(gaps):.2f}  max={max(gaps):.2f}")
    else:
        print(f"  {desc:<55} (no matched pairs)")
