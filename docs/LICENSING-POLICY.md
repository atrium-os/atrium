# Licensing policy

> **Atrium ships permissive licenses only. No GPL/LGPL/AGPL/SSPL/BUSL anywhere in the runtime stack.**

This document records a one-time architectural decision so contributors know the rule and the reasoning behind it. Re-litigating the question costs more than it saves; that's the only reason this document exists.

## The rule

| Category | Allowed | Forbidden | Case-by-case |
|---|---|---|---|
| **Atrium-authored code** | MIT or Apache-2.0 (project default) | — | — |
| **Vendored or forked deps** | MIT, BSD-2-Clause, BSD-3-Clause, Apache-2.0 (with or without LLVM exception), ISC, Zlib, public-domain, CC0 | GPL (any version), LGPL, AGPL, SSPL, BUSL, CDDL, EPL | MPL-2.0, Anti-996, EUPL — evaluate per-component |
| **Spec / docs** | CC-BY-4.0 or Apache-2.0 | GPL-style copyleft for docs | — |

No new GPL deps land in the dependency chain. Existing GPL components (today: drm-kmod, see below) are tracked as technical debt with a hard removal milestone.

## Why permissive only

1. **App ecosystem.** Atrium's thesis is "anyone ships an app." If our toolkit (Pergola), our display protocol (Fresco client libs), or our system services were GPL, every app linking against them would be GPL too. That's a non-starter for commercial software, proprietary tools, ports of existing apps, and toolkits like Qt/GTK that have their own license stories. **Permissive licensing is a precondition for being a credible app platform.**

2. **Linking model.** Atrium leans on dynamic libraries with stable C ABIs (per LANGUAGE-POLICY.md). LGPL imposes restrictions even for dynamic linking (must be replaceable, must distribute object files); GPL prohibits closed-source linking entirely. Both impose redistribution obligations we can't pass to downstream consumers without compromise.

3. **Kernel inheritance.** FreeBSD base is BSD-licensed. The Atrium project's parallel stack stays consistent with that culture. We're additive to FreeBSD, not a Linux-style culture transplant.

4. **Audit clarity.** A purely-permissive stack means every file in the runtime carries one of a small set of well-understood licenses. Compliance reviews, SBOM generation, and downstream redistribution are tractable.

## Existing GPL exposure: drm-kmod

The current Atrium GPU stack uses `drm-kmod` — Linux's DRM driver code ported to FreeBSD via linuxkpi. drm-kmod is **GPL-2** because the upstream Linux DRM is GPL-2.

- This is the **only** GPL component in our planned production runtime as of 2026-05.
- It is **technical debt to be excised by D5**, when Atrium GPU ABI replaces drm-kmod with a native FreeBSD driver written from scratch.
- The "no linuxkpi or compatibility shims" memory note (project-level) implies this excision; this document makes the licensing rationale explicit.

No additional GPL components may land between now and D5.

## Mesa / atrium-mesa

Mesa userspace (radv, anv, nvk, lavapipe, venus, NIR, gallium) is **MIT-licensed**. There is no GPL contamination concern in the Mesa runtime we'd ship.

The D5 atrium-mesa fork (per the Fresco rendering-stack roadmap) does a per-file license audit anyway, because Mesa is multi-vendor and has accumulated code over decades. The audit's role is to catch any drift, not to remediate a known problem. Expected outcome: 99% MIT, a few Apache-2.0 LLVM-derived files, possibly a public-domain math routine — nothing to actually excise.

The fork ships `atrium-mesa/LICENSES.md` inventorying every distinct license present, by file or directory.

## What this is NOT

- **Not a license-purity crusade.** The rule is pragmatic: keep the runtime permissively licensed so apps can ship. Tools, build scripts, CI rigs, dev-only utilities can be any license that lets us use them — they don't ship in the runtime.
- **Not anti-GPL.** GPL is a fine license for projects whose thesis is "all derivatives are open." Atrium's thesis is different. Linux is a GPL kernel and we run on FreeBSD partly *because* of that licensing difference.
- **Not retroactive.** Atrium-authored code already MIT or Apache-2.0; nothing to change.

## Enforcement

- **License manifest per repo.** Every repo with a `Cargo.toml` workspace ships a generated `LICENSES.md` listing the distinct licenses across all transitive deps. CI fails if a forbidden license appears.
- **Vendoring requires an explicit `LICENSES.md` entry** documenting the license of each vendored crate or directory.
- **Pull-request template** asks "does this introduce any new dependency? If so, what license?" — same way LANGUAGE-POLICY.md is enforced through review discipline.
- **License-scanner** (e.g. `cargo deny check licenses`) runs in CI with the allowlist baked in.

## Practical consequences

- New Rust crate dep → check `cargo deny`'s license output. If MIT/Apache/BSD/ISC/Zlib, fine. Otherwise reject or open a discussion.
- New vendored C library → audit license headers per file. Reject if any GPL/LGPL/AGPL surfaces.
- New tool used in CI but not shipped → fewer constraints; document in `tools/LICENSES.md` if non-trivial.
- New spec / doc → CC-BY-4.0 or Apache-2.0.
- Bug in a GPL upstream we wish we could vendor → write a permissive replacement, find a permissive alternative, or live without the feature.

## Decision log

| Date | Decision | Status |
|---|---|---|
| 2026-05-04 | Policy committed; drm-kmod flagged as technical debt to excise by D5 | Active |

## See also

- [LANGUAGE-POLICY.md](LANGUAGE-POLICY.md) — Rust + C language rule.
- [NAMING.md](NAMING.md) — component vocabulary.
- [ARCHITECTURE.md](ARCHITECTURE.md) — platform thesis.
- [ROADMAP.md](ROADMAP.md) — phase D5 (Atrium GPU ABI replaces drm-kmod).
