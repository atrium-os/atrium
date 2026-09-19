# navigator-prerender — the tier-2 conversion instrument

Runs a page's inline scripts **once, headlessly**, and snapshots the resulting
DOM. Its job is to produce a **measurement**, not a rendering:
[atrium-navigator-legacy-web.md](../docs/spec/atrium-navigator-legacy-web.md)
§5.4 asks what fraction of real content tiers 1–3 can serve, and §11.4 says
build the cheap instrument that answers it before porting Servo to find out.

    cargo run --release --bin prerender -- <file-or-dir>...

Three design points carried from the spec:

- **The engine sits behind a seam** (`engine.rs`). Boa is one implementation;
  the minimal DOM is the bulk of the work and is engine-agnostic. Swapping in
  another engine means implementing one trait, which is what lets this double
  as the evaluation harness for §11.8's trigger 1 — measuring a candidate
  against *our* corpus rather than a published conformance score.
- **The DOM is an arena addressed by integer handles**, so script holds a `u32`
  and every mutation crosses one host boundary where it can be counted and
  bounded. A stale handle is a range check, not a dangling reference.
- **Tolerance lives here and only here.** The document profile parses strictly
  and fails loudly; this is the converter, whose output is checkable.

**The error list is the missing-API report.** A script reaching for an
unimplemented binding fails by name, and the driver buckets those failures —
so the tool says what to build next instead of being guessed at. `window` was
added because the first corpus run named it.
