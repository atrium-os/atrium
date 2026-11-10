#!/usr/bin/env python3
"""Static kernel-stack depth of an arm64 kernel module (kmod audit T2).

Reads each function's frame from its prologue (`stp x29, x30, [sp, #-N]!`
plus `sub sp|x9.., sp, #M`, the KASAN form included), takes call edges from
direct `bl`/`b` to symbols in the module, and prints the deepest chain from
each root. Indirect calls and calls into the kernel are not followed, so this
is the module's own share; add the syscall/VFS path above it and an interrupt
frame below it.

  python3 scripts/kmod-stack-depth.py tessera_fs.ko
  python3 scripts/kmod-stack-depth.py tessera_fs.ko --roots '^tessera_vop_' -n 5 \\
      --cut tessera_fs_flush_sync_or_defer:tessera_fs_flush --cold b3_hash_on_stack

--cut A:B drops one edge (a call the code only makes when the stack allows,
like mark_dirty's inline flush); --cold F drops every call to F (a fallback
that the normal path never takes). Recursion is reported, not followed.
"""
import argparse
import re
import subprocess
import sys

ap = argparse.ArgumentParser()
ap.add_argument("ko")
ap.add_argument("--roots", default=r"^tessera_vop_|^tessera_fs_flush$")
ap.add_argument("-n", type=int, default=10)
ap.add_argument("--cut", action="append", default=[])
ap.add_argument("--cold", action="append", default=[])
ap.add_argument("--objdump", default="llvm-objdump")
ap.add_argument("--frames", type=int, default=10, help="also list the N largest frames")
a = ap.parse_args()

cut = {tuple(c.split(":", 1)) for c in a.cut}
cold = set(a.cold)
dis = subprocess.run([a.objdump, "-d", "--no-show-raw-insn", a.ko],
                     capture_output=True, text=True, check=True).stdout

frame, calls, cur = {}, {}, None
for line in dis.splitlines():
    m = re.match(r"^[0-9a-f]+ <(.+)>:$", line)
    if m:
        cur = m.group(1)
        frame[cur], calls[cur] = 0, set()
        continue
    if cur is None:
        continue
    f = line.split("\t")
    if len(f) < 3:
        continue
    op, args = f[1].strip(), f[2]
    if op == "stp" and "[sp, #-" in args and "]!" in args:
        frame[cur] += int(re.search(r"#-(0x[0-9a-f]+|\d+)", args).group(1), 0)
    elif op == "sub" and re.match(r"(sp|x9|x1[0-7]), sp, #", args):
        v = re.search(r"#(0x[0-9a-f]+|\d+)(, lsl #12)?", args)
        frame[cur] += int(v.group(1), 0) << (12 if v.group(2) else 0)
    elif op in ("bl", "b") and "<" in args:
        t = re.search(r"<([^>+]+)", args).group(1)
        if t != cur and t not in cold and (cur, t) not in cut:
            calls[cur].add(t)

sys.setrecursionlimit(20000)
memo = {}


def deepest(fn, stack=()):
    if fn in stack:
        return 0, [fn + " (recursion)"]
    if fn in memo:
        return memo[fn]
    best = (0, [])
    for c in calls.get(fn, ()):
        if c in frame:
            d = deepest(c, stack + (fn,))
            if d[0] > best[0]:
                best = d
    r = (frame[fn] + best[0], [f"{fn}:{frame[fn]}"] + best[1])
    memo[fn] = r
    return r


roots = [fn for fn in frame if re.search(a.roots, fn)]
for (depth, path), _ in sorted(((deepest(fn), fn) for fn in roots), reverse=True)[:a.n]:
    print(f"{depth:6d}  " + " -> ".join(path))
if a.frames:
    print("\nlargest frames:")
    for fn, n in sorted(frame.items(), key=lambda x: -x[1])[:a.frames]:
        print(f"{n:6d}  {fn}")
