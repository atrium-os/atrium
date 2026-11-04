# Atrium — workstream status

What is being worked on right now, where it stands, and where to read more.
This is the index; the specs and the run book hold the detail. How to run
anything named here is in [RUNBOOK.md §13](../RUNBOOK.md#13-cloud-and-remote-workflows-2026-11).

**Last refreshed: 2026-11-04 21:45 UTC** (HEAD `c4bbf2e`). Kept current by the
project's docs thread; if a row is wrong, the commit log wins — fix the row.

Commit hashes are atrium unless prefixed: `fbsd:` = atrium-os/freebsd-src
branch `atrium-os`, `gpusim:` = atrium-os/gpusim.

## At a glance

| Workstream | State | Latest | Next |
|---|---|---|---|
| [Native RDNA4 backend + gpusim](#1-native-rdna4-gpu-stack) | R9700 oracle v19: **54/54 jobs, 82 MATCH, 0 MISMATCH**; bench v5 ours ≤ RADV | spilling stage 3 `4bf3191`; VA reuse `2dac848` | bench v7 staged on the box (atrium16), waiting for an idle GPU; F6 driver half committing |
| [GPU driver isolation (F1/F6)](#2-gpu-driver-isolation) | F1 done; isolation 50/50, driver suite 28/28 on gpusim | `73a94ec`, `2dac848`, re-attest `9ae14a0` | warm-reboot fix (gpusim `bf008f1`), silicon run before production grant |
| [Fresco display pipeline](#3-fresco-display-pipeline) | D1–D3 done; GPU renderer on gpusim; F2, F4a–F4d done | F4d trusted chrome `a9bf150` | tell forum-wm about the 24 px strip; then F4f (secure password input) |
| [Compressed memory](#4-compressed-memory) | zram v2 increments 0–6 done; design A (swap fast path) **kept**, B shelved | spec `3da0acb` | — (closed 2026-11-04) |
| [Navigator security](#5-navigator-threat-model-fixes) | P0/P1 rows verified on FreeBSD; jail limits verified | `c23c266` (§6.5.2e) | P2: total fetched-bytes budget per conversion |
| [Navigator exploration coverage](#6-navigator-exploration-coverage) | baseline 14.6% of 2,045 handlers; re-measure 298 → 324 covered elements | probes `c57631b`, budget `c4bbf2e` | finish write-up, flags unchanged; then P2 byte budget |
| [Navigator document lane](#9-navigator-document-lane-normalizer--renderer) | corpus layout work: grid, flex, tables, RTL, UAX #14 line breaking | grid/box-alignment batch `36a341f`…`d715d05` | keep driving corpus flags down (thread "Fix Navigator corpus render flags") |
| [Navigator backend](#10-navigator-backend) | M3 forward/back passed; M4 jailed save/restore passes in a real jail | `a168de8`, `0841678`, `b9edfe3` | — |
| [x86_64 port](#7-x86_64-port) | A done, C done (amd64 VM, Tessera root, gpusim, navigator e2e) | `801a762`, `6273d0d`, `108098c`, `991db65` | B (Ubuntu userspace), D (driver on R9700 via VFIO), E (IPMI boot) |
| [Cloud dev loop](#8-cloud-dev-loop) | arm64 + amd64 FreeBSD VMs under TCG in cloud sessions | `scripts/cloud-vm.sh` | — |

## 1. Native RDNA4 GPU stack

Our own RDNA4 (GFX12) Vulkan/compute backend, no Mesa. Compiler layering is
frontend IR → GIR (`atrium-gir`) → RDNA4 machine layer (`atrium-rdna4-compiler`).
Command streams in `atrium-rdna4-cmd`, ISA tables in `atrium-rdna4-isa`
(MIT XML), oracle harness in `atrium-rdna4-oracle`.

- **Hardware truth:** every compiler change is checked on the R9700 (Linux,
  amdgpu as transport) and on gpusim. Reports:
  `/mnt/project-files/graphics/rdna4/reports/atrium_r9700_report_vN.txt`.
  - v14: Fresco bin + raster pixel-exact vs the Rust reference.
  - v16: first scratch use (spill stage 1).
  - v17: found that `DISPATCH_DIRECT` dims are exclusive *end* ids when
    `COMPUTE_START` is nonzero → fixed `4c9f6b2`, gpusim `60bb84e`.
  - **v19: 54 jobs, 82 MATCH, no mismatches** (includes LDS-alloc calibration).
- **Bench v5** (quiet box): ours ≤ RADV on every case; 1M-node glyph_run 374 µs vs RADV 526.
- **Spilling:** stages 1–3 done (scratch ST mode; demand-driven spill + remat;
  SGPRs into VGPR lanes, `4bf3191`). Design doc in the artifact linked from
  project memory `rdna4-spill-design`.
- **gpusim** (`~/src/gpusim`, separate repo): compute-only RDNA4 core built
  from public sources only (ISA guide + MIT headers). Draws use a SIM
  fixed-function rasterizer, so graphics-pipeline (M4) work has nothing to run on yet.
- Docs: [spec/atrium-rdna4-vulkan-backend.md](spec/atrium-rdna4-vulkan-backend.md),
  [spec/atrium-gpu-ir.md](spec/atrium-gpu-ir.md),
  [spec/rdna4-vulkan-subset.md](spec/rdna4-vulkan-subset.md),
  [GRAPHICS-STACK.md](GRAPHICS-STACK.md) (map as of 2026-11-03).
- Roadmap after this: graphics pipeline → ray queries → full RT, with Orbis as
  the driving workload.

## 2. GPU driver isolation

`atrium-gpu-amd` against GPU ABI v2.1 ([spec/atrium-gpu-abi-v2.md](spec/atrium-gpu-abi-v2.md),
header `include/atrium/gpu_abi_v2.h` is normative).

- **F1 done** (`73a94ec`): the five isolation breaks from
  [reviews/2026-11-03 §A3](reviews/2026-11-03-gpu-kernel-and-tier2-review.md)
  are closed. Map in [atrium-gpu-amd/ISOLATION.md](../atrium-gpu-amd/ISOLATION.md).
- VA reuse, reject overlapping/misaligned binds (`2dac848`).
- jaild policy lists atrium-gpu-amd as **experimental (passed, not granted)**;
  a silicon run of `test_gpu_isolation` is required before production.
- **F6 (in progress):** driver half = per-queue completion tracking + hang
  watchdog (gpusim: isolation 50/50, suite 28/28). A warm-reboot panic
  (gpusim kept DMA-ing after reset) is fixed in gpusim `bf008f1` and the
  driver now clears Bus Master on shutdown. App half (broker submits validated
  app code, apps hold no device fd) follows with Fresco F4.
- Known open: a parked/faulted ring holds its shared queue until close or ctl
  reset (DoS, not leak) until per-VM MES queues; review §A2.2, §A2.6.

## 3. Fresco display pipeline

Plan D1–D5 in [reviews/2026-11-04-fresco-display-pipeline.md](reviews/2026-11-04-fresco-display-pipeline.md);
compute route chosen; security conditions F1–F6 in
[spec/atrium-navigator-threat-model.md §2.5.4](spec/atrium-navigator-threat-model.md).

| Item | State | Commit |
|---|---|---|
| D1 display ABI v2 on gpusim (`include/atrium/display_abi_v2.h`, fenced commits, vblank pacing) | done | `4205742` |
| D2/D3 `fresco-compute` bin + raster kernels, Tier-2 matches reference; `FRESCOD_RENDERER=compute` | done | `ce1d804`, golden `6ca05f0` |
| GPU renderer: `FRESCOD_RENDERER=gpu` draws via RDNA4 kernels on gpusim | done | `7c1ec1b` |
| Damage tracking (only dirty tiles re-rastered; 15 s → 0.4 s/frame on gpusim) | done | `57035bc` |
| F2 bounds hardening + differential fuzzer | done | `9323062` |
| F4a per-window surfaces + compose kernel | done | `a20492f`, `797a3e5` |
| F4b one jailed renderer per client (`FRESCOD_SPLIT=1`, portcullis jail) | done, off by default | `a46ec90`, `0a864bd` |
| F4c input routed by the composed layer list; window 0 = WM | done | `1c87e2d` |
| F4c′ click-through on alpha < 16 (anti-clickjacking) | done | `2cf9231` |
| F4d trusted chrome: 24 px origin strip only frescod draws, focus ring | done | `a9bf150` |
| forum-wm aware of the strip | next | — |
| F4e one GPU VM per window | waits on F6 | — |
| F4f secure password input to frescod | waits on F5 + Navigator C0 | — |
| F3, F5 | not started | — |
| D4 gpusim toward real DCN4 regs, D5 silicon display | not started (R9700 has no monitor) | — |

Slice detail: [spec/fresco-f4-split.md](spec/fresco-f4-split.md). Screenshots
in `/mnt/project-files/graphics/frescod-*.png`.

## 4. Compressed memory

Decision (2026-11-03): in-kernel compressed memory; GPU-VRAM overflow rejected.
Spec [spec/atrium-zram-v2.md](spec/atrium-zram-v2.md).

- Increments 0–6 done: swap-free notification (fbsd:`da0b30649`), packed pool,
  per-CPU codecs, lazy table, backing store + writeback, LZ4 option (`c964309`).
- Native §6 bench on the R9700 (KVM): swap+GEOM overhead 46.5 % zstd / 68.6 %
  LZ4, over the 25 % trigger.
- **Design A** (swap fast path, fbsd:`482ce93dd`, spec §6.1): zstd 41.0 → 18.3 %
  (passes), LZ4 70.8 → 29.8 % (31.0 % with backing). 0 corrupt pages.
- **Shankar: keep A; B (full compressor pager) shelved.** Results in
  `/mnt/project-files/zram-bench/`.

## 5. Navigator threat model fixes

[spec/atrium-navigator-threat-model.md](spec/atrium-navigator-threat-model.md)
(all recommendations accepted 2026-11-04).

| Row | Fix | Verified on FreeBSD |
|---|---|---|
| P0 converter (runs page JS) unjailed | one-shot jail per conversion `68adf2e`; navigatord refuses unconfined host | yes, `da360ac` — 50 pages byte-identical jailed vs unconfined, no leaks |
| P1 no LAN/loopback filter in fetch | filter after resolution + pin `4a99a56` | yes, `9880f2b` — 23-row M1 gate unjailed and jailed |
| P1 `require_confinement()` unused | `PRERENDER_REQUIRE_CONFINEMENT=1` | yes |
| Jail limits (rctl memory/pcpu, memfed pools) | `f26f8ad`; SetRctl replaces instead of stacks `c23c266` | yes, §6.5.2e |
| P1 production image decoding home | open | — |
| P1 shared-store poisoning (per-user dedup) | open | — |
| P2 total fetched-bytes budget per conversion | next (coverage thread) | — |
| Credentials design (§6: vault + per-origin fetcher, C0) | designed; no logins before C0 | — |

Also settled: islands + DRM approach (legacy-web §8.1); honest UA, never solve
bot challenges (§5.4.1b).

## 6. Navigator exploration coverage

Measurement in legacy-web §5.4.5.6a (`3dd3fb7`); raw tables in
`~/src/navigator-corpus/coverage-2026-11-05/` (Mac, never committed).

- Baseline: exploration covers 14.6 % of 2,045 handlers; server-fed data 1.5 %;
  large state spaces 28.6 % (mostly the 64-probe cap); other 47 % (probe fidelity).
- Done: probe fidelity (real gestures, delegated targets) `c57631b`;
  per-handler budget `37c1de6`, raised to 64 per handler in `c4bbf2e` (8 lost coverage); spinning probe ends
  itself `4e653a6`.
- Re-measure (legacy-web §5.4.5.6b): covered handler elements 298 → 324 at
  64/handler, 256/document; 7,422 probes, 29 s over both corpora.
- Sibling templates (legacy-web §5.4.5.6c, recording format `/4`): covered handler
  elements 324 → 754 (19.4% → 45.2%); case 3 775 → 55; server-fed 39 = 18 data,
  13 remote execution, 8 analytics. No normalize/flag change; raw tables in
  `coverage-2026-11-05-r3/`.
- Next: answer `el.id` at load (24 documents read it; own corpus diff); the 6
  pre-existing replay failures (gov.uk, chinadaily, yahoo.jp, ynet); case 1 on arXiv.

## 7. x86_64 port

[spec/x86_64-port.md](spec/x86_64-port.md) (status §3.5).

- **A done** (`2280670`): Tier-2 on x86_64 via Cranelift (bespoke stays AArch64).
- **C done**: amd64 FreeBSD VM boots the fork kernel (`801a762`) and a Tessera
  root; fork userland cross-built and installed (`108098c`); Navigator jail e2e
  passes on amd64 (`6273d0d`); gpusim on amd64 (`991db65`, `atrium_gpu_test` 27/27).
- **Next:** B (finish Ubuntu userspace), D (atrium-gpu-amd on the R9700 via
  VFIO passthrough; IOMMU group verified, RDNA4 VFIO reset unknown), E (native
  PXE/IPMI boot; never write the box's disk).

## 8. Cloud dev loop

Cloud sessions run FreeBSD 16-CURRENT under QEMU TCG (no KVM, ~50–100× slower
than HVF): fine for jail/functional tests, not for perf numbers. See
[RUNBOOK §13.1](../RUNBOOK.md#131-freebsd-vm-in-a-cloud-session-scriptscloud-vmsh).

## 9. Navigator document lane (normalizer + renderer)

The document lane converts a page into the Document Profile
([spec/atrium-document-profile-v1.md](spec/atrium-document-profile-v1.md)) and
lays it out natively. Measured against two frozen corpora on the Mac (batch 1:
85 recordings; batch 2: 47), flagged by `nsg-flag` (TEXTLOSS, OVERLAP,
OFFCANVAS, SLICED, NARROW, REFUSED).

- 2026-11-03/04 work, roughly in order: inherited custom properties shared,
  GitHub render 177 s → 16 s; grid placement and minified media queries;
  tables (rowspan, baselines, nested tables, RTL tables); UAX #14 line breaking
  for CJK/Thai and spaced scripts; full RTL (`f03f991`…`ef5ef3e`); flexbox
  §9.7 freezing and auto margins, webkit aliases (…`228bc7f`); reversed flex
  done by the normalizer so visual order = reading order (`ac9e827`, profile G5);
  grid track sizing and §8.5 auto-placement; grid shorthands and Box Alignment
  (`36a341f`…`d715d05`).
- Rule: after any renderer change re-run `navigator-normalize` before
  `nsg-flag`, or the flags are stale. Flag runs are numbered
  (`baseline/flag-N.txt`, `batch2/flag-N.txt`) in `~/src/navigator-corpus`.
- Policy: honest UA, bot-check pages are REFUSED-BY-ORIGIN, never solved
  (legacy-web §5.4.1b).

## 10. Navigator backend

[spec/atrium-navigator-backend.md](spec/atrium-navigator-backend.md).

- M3 forward/back through every layer passed (`a168de8`).
- Session save/restore by replay through a content-addressed store (`6b6449d`),
  then through the jailed host (`0841678`).
- M4: jailed save/restore passes in a real FreeBSD jail (`b9edfe3`;
  "1 sessions, 1 navigations, 1 rewinds, 1 restores, 0 failures", TCG timings only).
- Accepts recording format v3 (`08661a8`).

## Finished threads (for the record)

| Thread | Outcome |
|---|---|
| Run FreeBSD VM on cloud machine / Boot FreeBSD and run jail tests | `scripts/cloud-vm.sh` (`e163289`); jail e2e passes in the cloud VM in ~1m48s |
| Test jailed save and restore | Navigator M4 passes in a real jail (`b9edfe3`) |
| Build compressed main memory | zram v2 increments 0–6, design A kept, B shelved |
| Add RDNA4 shader core to gpusim | gpusim compute core from public sources; calibrated against the R9700 (VGPR step 8/4, LDS unit 512 B, TIDIG); scratch modelled (gpusim `ed8c928`) |
| Port Atrium to x86_64 | phases A and C done; amdgpu firmware licence checked: binary redistribution OK, no reverse engineering |
| Set up R9700 GPU access here | access via Remote Control on the Mac; handed over to "Run R9700 jobs from the Mac" |
| Verify Navigator security fixes on FreeBSD | §2.1/§2.2 verified (`9880f2b`, `da360ac`); jail limits verified and rctl stacking bug fixed (`c23c266`); open nits in portcullis.md §6.5.2e′ |

## Where things live

| What | Where |
|---|---|
| Repo (Mac clone) | `~/src/bsd` |
| gpusim | `~/src/gpusim` (Mac), `~/gpusim` in cloud; repo atrium-os/gpusim |
| FreeBSD fork | atrium-os/freebsd-src branch `atrium-os` |
| QEMU fork (gpusim device) | atrium-os/qemu |
| Navigator corpus (third-party content, never commit) | `~/src/navigator-corpus` on the Mac |
| Shared outputs (reports, screenshots, kits) | project files: `graphics/`, `navigator/`, `x86_64/`, `zram-bench/`, `catchup/` |
| R9700 box | reachable only from the Mac (`ssh -l ubuntu orbis-r9700`); see RUNBOOK §13.5 |
