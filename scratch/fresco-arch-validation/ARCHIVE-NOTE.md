# Fresco architecture validation — archived POC

**Frozen on 2026-05-04.** Do not modify.

This is a snapshot of `~/src/fresco-poc` taken when the POC's
architectural validation work concluded. The standalone repo at that
path was a 12-commit proof that the Fresco rendering stack design
(SPIR-V bundles, GPU compute scene processing, CAS dedup wire format,
SPIR-V reflection-driven descriptor layouts) actually works on real
Vulkan via MoltenVK on macOS.

## What this validated

- §3 SPIR-V bundle architecture — manifest, AOT pipeline compilation,
  reflection-driven descriptor layouts, two ops in one bundle
- §3.6 GPU per-node compute — atomic-counter readback proves the
  kernel ran; parallel-for shape (NOT tree walking)
- §3.7 CAS dedup wire format — measured **99.8×** on scene-b
  (1× 4 MiB texture upload + 100 instance references)
- §3.4 op-ID registry mechanism — runtime dispatch by op-id
- Cross-platform Vulkan portability — works on MoltenVK; same code
  paths will hit vendor Vulkan on FreeBSD

## Where production work continues

The architecture validated here is being lifted into the
`fresco-scene-server` (renamed from `atrium-compositor`) in this
repository. See `docs/spec/fresco-production-rollout.md` for the M0..M9
milestones.

The POC's specific Rust crates (`fresco-vulkan`, `fresco-bundle`,
`atrium-rpc-display`, `atrium-core` bundle) are ports targets in
M1/M2.

## Files

- `Cargo.toml`, `Cargo.lock` — workspace
- `crates/` — five crates (fresco-server-poc, fresco-vulkan,
  fresco-bundle, atrium-rpc-display, fresco-test-client)
- `bundles/atrium-core/` — SPIR-V bundle (manifest + GLSL sources +
  build.sh). Compiled `.spv` files excluded from the archive — rebuild
  via `bundles/atrium-core/build.sh` if needed.
- `README.md` — original POC overview
- `GIT-HISTORY.txt` — chronological commit log from the original repo,
  captured at archive time (12 commits: `b782ce1..9c3033f`)

## Why archived as a copy, not a subtree

The original repo at `~/src/fresco-poc` is a separate git repository
with its own history. Rather than merging that history into the bsd
repo via `git subtree` (which would rewrite commits), we kept it as a
plain snapshot. The textual `GIT-HISTORY.txt` captures the commit
narrative for reference. If the original repo is ever needed for
deeper archaeology, it remains at its original path, untouched.
