#!/usr/bin/env python3
"""Exploration coverage, sorted into legacy-web §5.4.5's cases (§5.4.5.6 item 1).

    coverage.py <name>=<cov.tsv> [<name>=<cov.tsv> …] [--docs <out.tsv>] [--examples N]

Each cov.tsv is what `PRERENDER_COVERAGE=<file> PRERENDER_EXPLORE=1 prerender <dir>`
appends: one line per interactive element,

    doc  path  tag  type  events  flags  net_get  net_blocked  beacons
         timers_scheduled  timers_left  intervals_left  effect_targets

(flags: n native, e explorable, p probed, c cut by the probe cap, r recorded,
t truncated), or `doc  -` for a document with none.

An INTERACTIVE ELEMENT is one the page registered a handler on (any event
type), plus every native control (form, input other than hidden, textarea,
select, details, button, contenteditable). Two denominators are reported,
because they answer different questions:

  handlers  elements with a registered handler: what the page's code wired
  all       handlers plus native controls that have none

Each element is classified once, first rule that applies (in this order):

  1 server-fed     its probe tried the page network (refused: exploration
                   closes it), other than beacons — FIRST, even when the probe
                   also recorded a transition, which is then partial
  covered          a probe recorded a transition (truncated ones are noted)
  3 large state    an explorable candidate cut by the probe cap (64)
  2 free-form      a text-entry control, a form, or only typing events
  4 time-driven    its probe left timers or intervals pending past the drain
  native-only      a native control with no handler and no submission
                   (details, select, checkbox, a button…): the document lane
                   renders it natively; nothing to record
  other            the rest, split by why: events exploration never
                   simulates (hover, focus, pointer, scroll, key on a
                   non-text element…), or probed with no observable effect

★ The order is a decision, and it is stated: a search box whose probe fetches
is case 1 (a data binding whose parameter is the field, §5.4.5.2), not case 2.
"""
import collections
import sys

TEXT_TYPES = {"", "text", "search", "email", "url", "tel", "password", "number"}
TYPING = {"input", "keydown", "keyup", "keypress", "paste", "cut", "beforeinput",
          "compositionstart", "compositionend", "compositionupdate"}
CASES = ["covered", "1 server-fed", "2 free-form", "3 large state", "4 time-driven",
         "native-only", "other"]


def parse(path):
    rows, docs = [], set()
    for line in open(path, encoding="utf-8", errors="replace"):
        f = line.rstrip("\n").split("\t")
        docs.add(f[0])
        if len(f) < 13:
            continue
        rows.append(dict(doc=f[0], path=f[1], tag=f[2], type=f[3],
                         events=[e for e in f[4].split(",") if e], flags=f[5],
                         net_get=int(f[6]), net_blocked=int(f[7]), beacons=int(f[8]),
                         tsched=int(f[9]), tleft=int(f[10]), ileft=int(f[11]),
                         targets=[t for t in f[12].split("|") if t]))
    return rows, docs


def text_entry(r):
    return (r["tag"] == "textarea" or r["tag"].endswith("[contenteditable]")
            or (r["tag"] == "input" and r["type"] in TEXT_TYPES))


def classify(r):
    fl = r["flags"]
    # ★ THE NETWORK DECIDES FIRST, even over a recorded transition. A probe
    # that changed the DOM AND asked for data recorded only the part that did
    # not need the data (a spinner, a toggled panel): its transition is
    # partial, and counting it as covered hid every case-1 element in the
    # corpus — 30 of them — behind the word "covered".
    part = " (a partial transition was recorded)" if "r" in fl else ""
    if r["net_get"] > 0:
        return "1 server-fed", "same-origin GET" + part
    if r["net_blocked"] > 0:
        return "1 server-fed", "cross-origin or non-GET (refused)" + part
    if "r" in fl:
        return "covered", "truncated" if "t" in fl else ""
    handlers = bool(r["events"])
    if handlers:
        if "c" in fl:
            return "3 large state", "beyond the probe cap"
        if text_entry(r) or r["tag"] == "form" or set(r["events"]) <= TYPING:
            return "2 free-form", "text entry" if text_entry(r) else ("form" if r["tag"] == "form" else "typing events")
        if r["tleft"] > 0 or r["ileft"] > 0:
            return "4 time-driven", "interval" if r["ileft"] > 0 else "timer past the drain"
        # ★ A DELEGATING ROOT: React/Vue-style roots register dozens of event
        # types on one element and dispatch to components internally, and a
        # document-level click listener does the same for plain pages. A probe
        # on the root itself reaches no component, so every handler it stands
        # for is invisible here — and so are the elements they belong to,
        # which then show up as native-only buttons with "no handler".
        if len(r["events"]) >= 20 or (r["tag"] in ("html", "body") and "click" in r["events"]):
            return "other", "delegating root (handlers dispatched internally)"
        if "e" not in fl:
            kinds = sorted({kind(e) for e in r["events"]})
            return "other", "unsimulated: " + "+".join(kinds)
        return "other", "probed, no observable effect"
    # A native control with no handler at all.
    if r["tag"] == "form":
        return "2 free-form", "form submission (no handler)"
    return "native-only", r["tag"] + (f"[{r['type']}]" if r["type"] else "")


def kind(e):
    if e.startswith(("mouse", "pointer")) or e in ("hover", "contextmenu", "dblclick", "wheel"):
        return "pointer/hover"
    if e.startswith("touch") or e.startswith("drag") or e == "drop":
        return "touch/drag"
    if e in ("focus", "blur", "focusin", "focusout"):
        return "focus"
    if e in TYPING:
        return "keyboard"
    if e in ("scroll", "resize", "load", "error", "transitionend", "animationend", "animationstart"):
        return "lifecycle/scroll"
    return "other:" + e


def interference(rows):
    """Pairs of recorded elements in one document whose transitions write the
    same node — what §5.4.5.3 must explore jointly."""
    by_doc = collections.defaultdict(list)
    for r in rows:
        if "r" in r["flags"]:
            by_doc[r["doc"]].append(set(r["targets"]))
    out = {}
    for d, sets in by_doc.items():
        n = sum(1 for i in range(len(sets)) for j in range(i + 1, len(sets)) if sets[i] & sets[j])
        out[d] = (len(sets), n)
    return out


def report(name, rows, docs, examples):
    print(f"\n## {name}: {len(docs)} documents, {len(rows)} interactive elements")
    for r in rows:
        r["case"], r["why"] = classify(r)
    h = [r for r in rows if r["events"]]
    for label, sub in (("handlers", h), ("all (handlers + native controls)", rows)):
        c = collections.Counter(r["case"] for r in sub)
        tot = len(sub) or 1
        print(f"\n{label}: {len(sub)}")
        for k in CASES:
            if c[k]:
                print(f"  {k:<16} {c[k]:>6}  {100 * c[k] / tot:5.1f}%")
        unc = [r for r in sub if r["case"] not in ("covered", "native-only")]
        if unc:
            print(f"  uncovered (excl. native-only): {len(unc)}  ({100 * len(unc) / tot:.1f}%)")
    print("\nwhy, within each case (handlers + native):")
    for k in CASES:
        c = collections.Counter(r["why"] for r in rows if r["case"] == k)
        if c:
            print(f"  {k}: " + "; ".join(f"{w or '—'} {n}" for w, n in c.most_common(8)))
    print("\ntop documents per case (count of elements):")
    for k in CASES[1:]:
        if k == "native-only":
            continue
        c = collections.Counter(r["doc"] for r in rows if r["case"] == k)
        if c:
            print(f"  {k}: " + ", ".join(f"{d.rsplit('.', 1)[0]} {n}" for d, n in c.most_common(5)))
    print("\nexamples:")
    for k in CASES[1:]:
        if k == "native-only":
            continue
        ex = [r for r in rows if r["case"] == k]
        seen, shown = set(), 0
        for r in ex:
            key = (r["doc"], r["tag"], tuple(r["events"]))
            if key in seen:
                continue
            seen.add(key)
            ev = ",".join(r["events"]) or "(none)"
            print(f"  {k:<14} {r['doc'].rsplit('.', 1)[0]:<34} <{r['tag']}{' type=' + r['type'] if r['type'] else ''}>"
                  f" on {ev}  [{r['why']}; get {r['net_get']} blocked {r['net_blocked']} timers left {r['tleft']}]")
            shown += 1
            if shown >= examples:
                break
    capped = collections.Counter(r["doc"] for r in rows if "c" in r["flags"])
    print(f"\nprobe cap (64 per document): hit in {len(capped)} documents, "
          f"{sum(capped.values())} explorable elements never probed")
    inter = interference(rows)
    rec = sum(a for a, _ in inter.values())
    pairs = sum(b for _, b in inter.values())
    docs_i = [d for d, (_, b) in inter.items() if b]
    print(f"\ninterference (§5.4.5.3): {rec} recorded elements in {len(inter)} documents; "
          f"{pairs} pairs write a common node, in {len(docs_i)} documents")
    for d, (a, b) in sorted(inter.items(), key=lambda kv: -kv[1][1])[:5]:
        if b:
            print(f"  {d.rsplit('.', 1)[0]}: {b} interfering pairs among {a} recorded")
    return rows


def main():
    args = sys.argv[1:]
    docs_out, examples = None, 4
    if "--docs" in args:
        i = args.index("--docs"); docs_out = args[i + 1]; del args[i:i + 2]
    if "--examples" in args:
        i = args.index("--examples"); examples = int(args[i + 1]); del args[i:i + 2]
    allrows, alldocs = [], set()
    per = []
    for a in args:
        name, path = a.split("=", 1)
        rows, docs = parse(path)
        report(name, rows, docs, examples)
        for r in rows:
            r["corpus"] = name
        allrows += rows
        alldocs |= {(name, d) for d in docs}
        per.append((name, rows, docs))
    if len(per) > 1:
        for r in allrows:
            r["doc"] = r["corpus"] + "/" + r["doc"]
        report("overall", allrows, {f"{n}/{d}" for n, d in alldocs}, examples)
    if docs_out:
        with open(docs_out, "w") as o:
            o.write("corpus\tdocument\telements\thandlers\t" + "\t".join(CASES) + "\n")
            for name, rows, docs in per:
                by = collections.defaultdict(list)
                for r in rows:
                    by[r["doc"].split("/", 1)[-1]].append(r)
                for d in sorted(docs):
                    rs = by.get(d, [])
                    c = collections.Counter(r["case"] for r in rs)
                    o.write(f"{name}\t{d}\t{len(rs)}\t{sum(1 for r in rs if r['events'])}\t"
                            + "\t".join(str(c[k]) for k in CASES) + "\n")


if __name__ == "__main__":
    main()
