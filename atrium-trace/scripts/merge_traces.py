#!/usr/bin/env python3
"""Merge per-process atrium_trace JSON fragments into one Chrome Trace file.

Usage:
    merge_traces.py <out.json> <input1.trace.*> [input2.trace.* ...]
    merge_traces.py <out.json> --kmod-dump <kmod_sysctl_dump.txt> [<input.trace.*> ...]

Each input fragment is a partial JSON file with the shape:
    [
    {"name":"foo","ph":"B","ts":1234,"pid":N,"tid":T},
    ...
    {"name":"_end","ph":"i","ts":...,"pid":N,"tid":0}
    ]

The kmod-dump variant accepts a text dump of the kmod ring buffer
(produced by `sysctl kern.atrium_trace.dump`) with lines of the form:
    <ns_realtime> <cpu> <label>
"""
import json
import sys
import re
import os


def load_userspace_fragment(path):
    with open(path) as f:
        text = f.read().strip()
    if not text:
        return []
    # Strip leading [ and trailing ] / commas; the file format keeps
    # the array open in case the process aborted.
    if text.startswith("["):
        text = text[1:]
    if text.endswith("]"):
        text = text[:-1]
    # Split lines, parse each as a single object (ignore blanks/trailing commas).
    events = []
    for line in text.splitlines():
        line = line.strip().rstrip(",")
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            sys.stderr.write(f"skipping malformed line in {path}: {line[:80]}\n")
    return events


def load_kmod_dump(path, base_pid=99999):
    """Convert a kmod ring-buffer dump into Chrome Trace instant events.

    Each line: '<ns_realtime> <cpu> <label> [<id>]'. Emit as 'i' (instant)
    events under a synthetic pid (so they show up as a separate
    process row in Perfetto).

    The optional <id> trailing field is a correlation id (e.g. fence_id);
    it goes into args.id on the emitted event so the analyzer can match
    cross-boundary by identity rather than by timestamp.
    """
    events = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            # 4-field: ns cpu label id   |   3-field: ns cpu label
            m4 = re.match(r"^(\d+)\s+(\d+)\s+(\S+)\s+(\d+)$", line)
            m3 = re.match(r"^(\d+)\s+(\d+)\s+(.+)$", line)
            if m4:
                ns, cpu, label, eid = int(m4.group(1)), int(m4.group(2)), m4.group(3), int(m4.group(4))
            elif m3:
                ns, cpu, label, eid = int(m3.group(1)), int(m3.group(2)), m3.group(3), 0
            else:
                continue
            ev = {
                "name": label,
                "ph": "i",
                "ts": ns // 1000,  # us
                "pid": base_pid,
                "tid": cpu,
                "s": "g",
            }
            if eid:
                ev["args"] = {"id": eid}
            events.append(ev)
    return events


def main():
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        sys.exit(2)

    out_path = sys.argv[1]
    args = sys.argv[2:]

    all_events = []
    pid_metadata = {}

    i = 0
    while i < len(args):
        if args[i] == "--kmod-dump":
            kmod_path = args[i + 1]
            evs = load_kmod_dump(kmod_path)
            all_events.extend(evs)
            pid_metadata[99999] = "atrium-virtio-gpu (kmod)"
            i += 2
            continue

        path = args[i]
        evs = load_userspace_fragment(path)
        all_events.extend(evs)
        # Tag pid name from filename (e.g. moltenvk.trace.json.12345 -> "moltenvk")
        base = os.path.basename(path)
        for ev in evs:
            if ev["pid"] not in pid_metadata:
                pid_metadata[ev["pid"]] = base.split(".")[0]
        i += 1

    # Emit pid metadata events so Perfetto labels each row.
    meta_events = []
    for pid, name in pid_metadata.items():
        meta_events.append({
            "name": "process_name", "ph": "M", "pid": pid, "tid": 0,
            "args": {"name": name},
        })

    # Sort by ts for sanity (Perfetto doesn't strictly require it).
    all_events.sort(key=lambda e: e.get("ts", 0))

    out = {"traceEvents": meta_events + all_events, "displayTimeUnit": "ms"}
    with open(out_path, "w") as f:
        json.dump(out, f)
    print(f"wrote {out_path} ({len(all_events)} events from {len(pid_metadata)} processes)")


if __name__ == "__main__":
    main()
