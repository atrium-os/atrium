# navigator-normalize — the normalizer (Atrium Document Profile v1 §6)

Accepts real-world HTML and emits a **profile-conformant** document, or fails.
Tolerance lives here, quarantined, so the renderer stays small and strict.

## The design decision

It **resolves the cascade itself** and emits one flat class per distinct
declaration block. It does not rewrite each unadmitted construct into an
admitted one — which is impossible in general for a descendant combinator. It
evaluates the selector, keeps the result, and throws the selector away.

Eleven kinds of unadmitted selector, `!important`, shorthands, `var()`,
`@media`, `@import` and inline `style=` attributes all collapse into that one
move. The corpus named the work: 692 descendant combinators, 1215 inline style
attributes, 201 shorthands across 29 documents.

## The gate

The acceptance test is the profile itself: **the output must render with zero
refusals**. Every test in `tests/gate.rs` renders the output and asserts the
renderer reports nothing, and each carries a control showing the input is
refused before normalization — so a normalizer that stopped working could not
pass quietly.

## No capabilities

Stylesheets and measured image sizes are **inputs** (§6: jailed, offline, zero
capabilities). It never fetches anything. Table columns are pre-measured
through `navigator-render`, with the pinned font set — which is why the font
set version belongs in this function's cache key (§3.13).

## What it drops, and says so

What cannot be represented is dropped and reported by reason and count:
properties outside the profile's 64 rows, pseudo-element content, layered
backgrounds, images whose intrinsic size the input did not declare. Silence
would be the only real failure.
