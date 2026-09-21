# navigator-backend

Consumes `atrium-navigator-recording/1` — the output of `navigator-prerender`.

Specs: `docs/spec/atrium-navigator-backend.md` (§4.0 the format, §4.2 untrusted
input), `docs/spec/atrium-navigator-legacy-web.md` (§5.4.2 tier policy, §5.4.3
the state recorder).

## What exists

**Ingest and validation only.** A recording goes in, a bounded `Recording`
comes out, or a `Reject` explaining why not. It does not build a scene graph
yet: that is a separate answer from *is this a recording and is it within
bounds*, and the two fail for different reasons.

## The stance

A recording is untrusted, **including our own**. It arrives from a
content-addressed store that vouches for the bytes being what someone
published, not for their meaning.

- Size is checked **before** parsing.
- Derived fields (`attribute_only`) are **recomputed, never read** — a derived
  field in untrusted input is a claim, not a fact. Disagreement is reported.
- Unknown effect kinds are **skipped**, not fatal, so the format can grow.
- Too many transitions **truncate** with the discarded count reported, rather
  than refusing a document that is otherwise fine.
- Malformed input refuses; it never panics.

## Testing

`cargo test` covers the rules. To check the interface against the converter's
real output:

```
NAVIGATOR_RECORDINGS=/path/to/emitted cargo test --test roundtrip -- --nocapture
```

Produce that directory with
`PRERENDER_EMIT_DIR=<dir> PRERENDER_EXPLORE=1 prerender <corpus>`.
The test SKIPS loudly when the variable is unset rather than passing quietly.
