#!/usr/bin/env python3
"""Decompose the SUBMIT_3D round-trip latency by fence_id correlation.

Reads merged.json, groups events by args.id (fence_id), and computes
per-fence phase timings:

    kmod.submit_3d_enter (ns_g)              [guest clock]
        ↓ (kmod prep)
    kmod.vq_notify (ns_g)
        ↓ (HVF VM-exit + QEMU dispatch)         [cross-domain]
    qemu.handle_ctrl + qemu.handle_ctrl.cmd_popped(fence_id) (us_h)  [host clock]
        ↓ (QEMU enqueues to cmdq, virglrenderer worker wake)
    venus.ring.notify / venus.ring.dispatch B (us_h)
        ↓ (host work)
    venus.fence.retire(fence_id) (us_h)
    qemu.fence_resp(fence_id) (us_h)
    qemu.fence_resp.notified(fence_id) (us_h)
        ↓ (HVF VM-entry + IRQ delivery)         [cross-domain]
    kmod.ctrl_intr(fence_id) (ns_g)         [guest clock]
        ↓ (cv_signal → cv_wait return)
    kmod.submit_3d_woke(fence_id) (ns_g)

Within-domain phases (no clock skew):
    A. kmod prep  = vq_notify - submit_3d_enter
    B. host total = qemu.fence_resp.notified - qemu.handle_ctrl.cmd_popped
    C. guest IRQ-to-userspace = submit_3d_woke - ctrl_intr

Cross-domain phases (rely on identity, not absolute time):
    D. guest_kick → host_pop = (host clock at handle_ctrl.cmd_popped) - (guest clock at vq_notify)
    E. host_notify → guest_irq = (guest clock at ctrl_intr) - (host clock at fence_resp.notified)

The total round-trip is A + D + B + E + C.

Even with clock skew between host and guest, D + E together equal
"total round-trip - (A + B + C)" which we can compute exactly because
A, B, C are each within-domain. So the cross-domain split sums correctly
even if individual D, E are skewed; the SKEW will appear as -X / +X in D vs E.

Usage: decompose_roundtrip.py <merged.json>
"""
import json, sys, statistics, collections

if len(sys.argv) != 2:
    print(__doc__); sys.exit(2)

with open(sys.argv[1]) as f:
    data = json.load(f)

events = sorted(data["traceEvents"], key=lambda e: e.get("ts", 0))

# Group by fence_id
by_id = collections.defaultdict(list)
for e in events:
    args = e.get("args") or {}
    fid = args.get("id", 0)
    if fid: by_id[fid].append(e)

# For each fence_id, get the relevant timestamps
def get_ts(events_for_id, name):
    """Return ts of first event matching name."""
    for e in events_for_id:
        if e.get("name") == name:
            return e["ts"]
    return None

records = []
for fid, evs in by_id.items():
    rec = {"id": fid}
    for label in ["kmod.submit_3d_enter", "kmod.vq_notify",
                  "qemu.handle_ctrl.cmd_popped",
                  "venus.fence.retire",
                  "qemu.fence_resp", "qemu.fence_resp.notified",
                  "kmod.ctrl_intr", "kmod.submit_3d_woke"]:
        rec[label] = get_ts(evs, label)
    records.append(rec)

# Filter to those that have at least submit_3d_enter + ctrl_intr + woke
complete = [r for r in records if r["kmod.submit_3d_enter"] and
                                  r["kmod.ctrl_intr"] and
                                  r["kmod.submit_3d_woke"]]

print(f"Total fence_ids seen: {len(records)}")
print(f"With complete kmod path: {len(complete)}")

if not complete:
    sys.exit(1)

def phase(rs, a, b, name, n_warn=10):
    ds = []
    for r in rs:
        if r.get(a) is not None and r.get(b) is not None:
            ds.append(r[b] - r[a])  # us
    if not ds:
        return
    ds_sorted = sorted(ds)
    p50 = ds_sorted[len(ds)//2]
    p90 = ds_sorted[int(len(ds)*0.9)]
    p99 = ds_sorted[int(len(ds)*0.99)]
    flag = "" if len(ds) >= n_warn else " (LOW SAMPLE)"
    print(f"  {name:<55} n={len(ds):>4}  p50={p50/1000:>9.3f}  p90={p90/1000:>9.3f}  "
          f"p99={p99/1000:>9.3f}  ms{flag}")

print("\n=== WITHIN-DOMAIN phases (clean, no skew) ===")
print("\nGuest-only (kmod, single domain):")
phase(complete, "kmod.submit_3d_enter", "kmod.vq_notify",  "A. kmod prep (enter→vq_notify)")
phase(complete, "kmod.submit_3d_enter", "kmod.submit_3d_woke", "TOTAL kmod blocking (enter→woke)")
phase(complete, "kmod.vq_notify",       "kmod.ctrl_intr",  "kmod gap: vq_notify→ctrl_intr (paravirt round-trip — guest view)")
phase(complete, "kmod.ctrl_intr",       "kmod.submit_3d_woke", "C. guest IRQ→userspace return (ctrl_intr→woke)")

print("\nHost-only (single domain):")
phase(complete, "qemu.handle_ctrl.cmd_popped", "venus.fence.retire", "B0. QEMU pop→virglrenderer fence (host work, virgl-side)")
phase(complete, "qemu.handle_ctrl.cmd_popped", "qemu.fence_resp",    "B1. QEMU pop→fence_resp (host work, qemu-side)")
phase(complete, "qemu.handle_ctrl.cmd_popped", "qemu.fence_resp.notified", "B. QEMU pop→fence_resp.notified (HOST TOTAL)")
phase(complete, "venus.fence.retire",         "qemu.fence_resp",    "venus retire→qemu fence_resp (callback delay)")
phase(complete, "qemu.fence_resp",            "qemu.fence_resp.notified", "qemu fence_resp→notified (virtio_notify cost)")

print("\n=== CROSS-DOMAIN phases (timestamps from different clocks; identity-paired) ===")
phase(complete, "kmod.vq_notify",            "qemu.handle_ctrl.cmd_popped", "D. guest kick→QEMU pop (HVF VM-exit + QEMU dispatch)")
phase(complete, "qemu.fence_resp.notified",  "kmod.ctrl_intr",              "E. QEMU notify→guest IRQ (HVF VM-entry + ARM GIC)")

# Compute total round-trip and check additivity
print("\n=== ROUND-TRIP DECOMPOSITION (sums) ===")
totals = []
for r in complete:
    if all(r.get(k) is not None for k in ["kmod.submit_3d_enter", "kmod.vq_notify",
                                            "qemu.handle_ctrl.cmd_popped",
                                            "qemu.fence_resp.notified",
                                            "kmod.ctrl_intr", "kmod.submit_3d_woke"]):
        # within-domain
        A = r["kmod.vq_notify"] - r["kmod.submit_3d_enter"]
        B = r["qemu.fence_resp.notified"] - r["qemu.handle_ctrl.cmd_popped"]
        C = r["kmod.submit_3d_woke"] - r["kmod.ctrl_intr"]
        # cross-domain (skewed)
        D = r["qemu.handle_ctrl.cmd_popped"] - r["kmod.vq_notify"]
        E = r["kmod.ctrl_intr"] - r["qemu.fence_resp.notified"]
        # ground truth — kmod-only
        T = r["kmod.submit_3d_woke"] - r["kmod.submit_3d_enter"]
        # cross-domain budget = T - A - B - C
        cross = T - A - B - C
        totals.append({"A":A,"B":B,"C":C,"D":D,"E":E,"T":T,"cross":cross})

if totals:
    print(f"  Phase medians (us → ms):")
    for k in "ABCDE":
        ds = sorted([t[k] for t in totals])
        p50 = ds[len(ds)//2]
        sign = "" if p50 >= 0 else ""
        print(f"    {k} = {p50/1000:>9.3f} ms (p50, n={len(ds)})")
    ds = sorted([t["T"] for t in totals])
    print(f"    T (TOTAL kmod blocking) = {ds[len(ds)//2]/1000:>9.3f} ms")
    ds = sorted([t["cross"] for t in totals])
    print(f"    T - (A+B+C) = {ds[len(ds)//2]/1000:>9.3f} ms (cross-domain budget; should = D+E once skew cancels)")
    print()
    print(f"  Skew-corrected cross-domain split (D+E should approximate this):")
    print(f"    median (T - A - B - C) = {ds[len(ds)//2]/1000:.3f} ms")
    print(f"    This is the TOTAL HVF crossing cost (in→out + out→in)")
    print(f"    Individual D and E above are skewed; their SUM minus skew equals the above.")
