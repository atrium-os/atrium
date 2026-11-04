#!/usr/bin/env python3
"""Choose the exploration bounds from the corpus (legacy-web §5.4.5.6b).

    budget.py <cov.tsv> [<cov.tsv> …] [--grid K1,K2,… N1,N2,…]

Reads the `<doc>  #stats  handlers probes cut rounds micros per_handler` lines
that `PRERENDER_COVERAGE` writes, from a run with the bounds OFF
(`PRERENDER_PROBES=1000000,1000000`), where each handler's probe count is its
candidate count. For each (per-handler K, per-document N) it replays the
round-robin: round r probes every handler with more than r candidates, until
the document has spent N. It reports, over all documents:

  handlers reached   handlers with at least one probe, of all handlers
  docs complete      documents where every handler got its first probe
  probes             total probes, and the largest document's
  est. time          per document, probes x its measured cost per probe;
                     the worst document and the total

The replay is exact for direct candidates. Delegated candidates join when
their discovery probe runs, which is always in round 0 when it fits in N, so
the replay treats them as present from the start; it can only overstate
reach in documents that hit N inside round 0, and it says how many did.
"""
import sys


def load(paths):
    docs = []
    for p in paths:
        for line in open(p, encoding="utf-8", errors="replace"):
            f = line.rstrip("\n").split("\t")
            if len(f) >= 7 and f[1] == "#stats":
                per = [int(x) for x in (f[7] if len(f) > 7 else "").split(",") if x]
                probes, micros = int(f[3]), int(f[6])
                # ★ A table without the per-handler column (an older binary)
                # would replay as zero handlers and report a perfect score.
                if len(per) != int(f[2]):
                    sys.exit(f"{p}: {f[0]}: {f[2]} handlers but {len(per)} per-handler counts "
                             "(a binary older than the per-handler column?)")
                docs.append(dict(doc=f[0], per=per, probes=probes,
                                 us_per=(micros / probes) if probes else 0.0))
    return docs


def replay(per, k, n):
    spent, reached, r = 0, 0, 0
    first_round_cut = False
    while r < k:
        live = [c for c in per if c > r]
        if not live:
            break
        for _ in live:
            if spent >= n:
                return spent, reached, first_round_cut or r == 0
            spent += 1
            if r == 0:
                reached += 1
        r += 1
    return spent, reached, first_round_cut


def main():
    args = sys.argv[1:]
    ks, ns = [1, 2, 4, 8, 16], [64, 128, 256, 512, 1024]
    if "--grid" in args:
        i = args.index("--grid")
        ks = [int(x) for x in args[i + 1].split(",")]
        ns = [int(x) for x in args[i + 2].split(",")]
        del args[i:i + 3]
    docs = load(args)
    H = sum(len(d["per"]) for d in docs)
    C = sum(sum(d["per"]) for d in docs)
    print(f"{len(docs)} documents with exploration; {H} handlers; {C} candidates (unbounded)")
    hs = sorted(len(d["per"]) for d in docs)
    if hs:
        q = lambda p: hs[min(len(hs) - 1, int(p * len(hs)))]
        print(f"handlers per document: median {q(0.5)}, p90 {q(0.9)}, max {hs[-1]}")
        big = sorted(docs, key=lambda d: -len(d["per"]))[:5]
        print("most handlers: " + ", ".join(f"{d['doc'].rsplit('.', 1)[0]} {len(d['per'])}" for d in big))
        cost = sorted(docs, key=lambda d: -d["us_per"])[:5]
        print("costliest probe: " + ", ".join(f"{d['doc'].rsplit('.', 1)[0]} {d['us_per'] / 1000:.1f} ms" for d in cost))
    print(f"\n{'K':>4} {'N':>5}  {'handlers reached':>18} {'docs complete':>14} {'probes':>7} {'max/doc':>7}"
          f" {'worst doc s':>11} {'total s':>8} {'cut in r0':>9}")
    for k in ks:
        for n in ns:
            tot = reach = complete = mx = r0 = 0
            worst = total_s = 0.0
            for d in docs:
                spent, reached, cut0 = replay(d["per"], k, n)
                tot += spent; reach += reached; mx = max(mx, spent)
                complete += reached == len(d["per"])
                r0 += cut0
                s = spent * d["us_per"] / 1e6
                worst = max(worst, s); total_s += s
            print(f"{k:>4} {n:>5}  {reach:>7}/{H:<5} {100 * reach / max(H, 1):4.1f}% {complete:>6}/{len(docs):<6}"
                  f" {tot:>7} {mx:>7} {worst:>11.1f} {total_s:>8.1f} {r0:>9}")


if __name__ == "__main__":
    main()
