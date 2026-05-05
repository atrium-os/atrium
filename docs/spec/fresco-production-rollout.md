# Fresco — production rollout plan

> **Scope:** the work between today (POC validated, spec frozen) and the
> Fresco rendering stack running on FreeBSD bare metal as a usable
> graphics platform. Aligns with `ROADMAP.md` D1 → D5; supersedes the
> old `D5: Slint backend` line item (we're building Pergola natively
> instead — see `pergola.md`).
>
> **Status:** drafted 2026-05-04; not yet started.

---

## State of play

| Track | Status | Where |
|---|---|---|
| Architectural POC (macOS, Vulkan via MoltenVK, SPIR-V bundles, CAS dedup) | ✅ Done — 12 commits, 99.8× CAS dedup measured, scene-a + scene-b visually verified | `~/src/fresco-poc` |
| FreeBSD native scene server + clients (frescod, atrium-edit-socket, etc.) | 🟡 D1 in progress; tiny-skia rasterizer; not yet on bare metal | `~/src/bsd/frescod`, `fresco-socket-rs`, others |
| Wire protocol (`fresco-protocol`) | 🟡 Stable enough for POC; needs the §3.8 op-family additions before D2 | `~/src/bsd/aqueduct/`, `~/src/fresco-poc/crates/fresco-protocol/` |
| Tessera CAS-FS | 🟡 D1.5 substantial work done; not the critical path for Fresco production | `~/src/bsd/fresco-kmod/` etc. |
| Pergola toolkit | ⚪ Spec drafted (`docs/spec/pergola.md`); no code yet | (new) |
| atrium-mesa fork | ⚪ Decision committed; no code yet (D5 work) | (new) |

---

## Pre-production gates (settled)

All five locked during the planning conversation:

1. **Rename `atrium-compositor` → `frescod`** (the daemon binary) and absorb the `fresco-server` library crate into bsd as `fresco-scene-server`. Retire the tiny-skia path from production; lavapipe is the SW fallback.
2. **Archive `~/src/fresco-poc`.** Move under `bsd/scratch/fresco-arch-validation/` for posterity. Stop committing there.
3. **Wire format: hard cutover at D2.** No coexistence window with the legacy 128-byte format.
4. **Vulkan strategy:**
   - macOS dev fast path: virtio-gpu venus + custom virglrenderer-on-MoltenVK (one-time setup, document in RUNBOOK)
   - SW fallback: lavapipe (in atrium-mesa fork)
   - Real FreeBSD HW: Mesa + drm-kmod (D2-D4), then atrium-mesa + Atrium GPU ABI (D5)
5. **Pergola is a parallel track**, not a D2 critical-path dependency. Bring-up apps continue on raw `fresco-socket-rs` through D5; Pergola adoption begins D6.

---

## Milestones

Each milestone has a concrete **done-when** so we know to stop and check.

### M0 — Repo reconciliation (1–2 days)

**Goal:** clean working tree before any new code lands.

- [x] Archive `~/src/fresco-poc` to `bsd/scratch/fresco-arch-validation/` as a frozen reference (plain copy + GIT-HISTORY.txt; original repo retained at the source path).
- [x] Update `docs/ROADMAP.md`: remove "D5: Slint backend"; replace with "D5: Pergola native + atrium-mesa fork + accessibility". Add D4.5 (declarative animation). Cross-reference `pergola.md` and `LICENSING-POLICY.md`.
- [x] Subtree-merge `~/src/fresco/` (the `fresco-server` library crate) into bsd at `bsd/fresco-scene-server/`. Retire the macOS-only binary (winit + Metal) inside it. Rename crate `fresco-server` → `fresco-scene-server`.
- [x] Rename the bsd binary crate `atrium-compositor` → `frescod` (binary name, crate name, directory). Update all consumer Cargo.toml path deps and Rust `use` statements across `fresco-socket-rs`, `atrium-edit-socket`, `atrium-term-socket`, `atrium-splash`, `atrium-clock-socket`, `atrium-find-socket`, `atrium-test-client`, and `frescod` itself. Rename the default socket path `/tmp/atrium-compositor.sock` → `/tmp/frescod.sock` and env var `ATRIUM_COMPOSITOR_SOCK` → `FRESCOD_SOCK`.
- [ ] Update any remaining inline refs to `atrium-compositor` in spec docs / RUNBOOK / README to use the new naming.
- [ ] Cross-compile workspace build smoke-test (cargo build per consumer; verify nothing's broken).

**Done when:** `git grep atrium-compositor` returns only historical commit messages and intentional documentation of the rename; ROADMAP.md is internally consistent; bsd workspace builds.

---

### M1 — Wire protocol freeze for D2 (3–5 days)

**Goal:** lock the op set we're going to commit to before refactoring frescod (and the `fresco-scene-server` library it links) against it.

- [ ] Port `fresco-protocol` payload schemas from the macOS POC into the bsd `aqueduct/` workspace.
- [ ] Implement the `WINDOW_*` op family per §3.8.1: control ops + async events.
- [ ] Reserve op-id ranges per §3.4 closed registry: confirm `0x1000-0x1FFF` for atrium-core, `0x2000-` for atrium-text (D3), reserve `0x3000-` for animation, `0x4000-` for AX (deferred).
- [ ] Spec-side: extend `docs/spec/aqueduct.md` if needed; ensure `aqueduct-services.md` reflects CLASS_DISPLAY content.
- [ ] Add a wire-format conformance test crate (`aqueduct-conformance`?) that pins envelope encoding bit-for-bit.

**Done when:** aqueduct + fresco-protocol compile clean in the bsd workspace; conformance test passes; spec docs reference each op.

---

### M2 — frescod: SPIR-V backend + bundle loading (2–3 weeks)

**Goal:** frescod replaces tiny-skia with the SPIR-V bundle dispatch path proven in the POC. Most of the work lands inside the `fresco-scene-server` library (where the rendering machinery lives), with frescod as the consuming binary.

- [ ] Port `fresco-vulkan` crate from POC into the bsd repo (new top-level crate).
- [ ] Port `fresco-bundle` crate (manifest + SPIR-V load + reflection).
- [ ] Port `atrium-core` bundle (rect, texture ops + GLSL sources + build.sh).
- [ ] Wire bundle dispatch into `fresco-scene-server`'s render loop, replacing the tiny-skia rasterizer (frescod inherits this transparently).
- [x] ~~Implement per-connection `SceneState` in `fresco-scene-server`~~ — **resolved 2026-05-04 audit**: the existing fresco-scene-server already enforces per-window-with-client-owner isolation at every routable dispatch (see `command/frontend.rs:119`). The macOS POC's shared-SceneState gap was POC-specific, not architectural. M2.6's WINDOW_* dispatchers slot into the existing model.
- [ ] Implement `WINDOW_*` op family in `fresco-scene-server`'s `CommandFrontend` (input routing, focus, close-request flow).
- [ ] Validate against existing demos via frescod: rect-bouncer, slot-demo, edit-socket, textured, window-demo, keyboard.

**M2.7c migration scope discovered 2026-05-04**: of the 8 atrium-test-client sub-binaries + 5 socket apps + splash, only the rect/texture-shaped ones can migrate now:

- ✅ atrium-test-client (magenta rect), atrium-rect-bouncer, atrium-window-demo, atrium-textured, atrium-slot-demo, atrium-text-demo: migrated to fresco-client
- 🚫 atrium-keyboard, atrium-mouse-demo: blocked on input-event op family (M3+ design)
- 🚫 atrium-clock-socket, atrium-edit-socket, atrium-term-socket, atrium-find-socket: blocked on `ATRIUM_CORE_PATH` (rotated rects, glyph cursors, custom shapes). Op-id `0x1002` reserved per spec §3.4 but params struct + bundle compute kernel don't exist yet.
- N/A atrium-splash: writes directly to EFI GOP framebuffer (`/dev/atrium-bootfb0`); not a Fresco client.

**Implication**: M2.7c can complete only the 6 demos shipped above. Full demo migration unblocks at M3 (atrium-text bundle + atrium-core PATH op).

**Done when:** all pre-existing demos run on the new SPIR-V backend in QEMU (lavapipe). atrium-edit-socket is visually identical to its tiny-skia output. Multi-app: launch two demos in parallel, both render correctly without state interference.

**Risks:**
- Per-connection state migration — the POC's SceneState refactor pattern needs careful application
- Wire-format hard cutover — coordinate fresco-socket-rs migration with all consumers in one PR

---

### M3 — Vulkan in QEMU: lavapipe (slow path) (3–5 days)

**Goal:** scene server runs in the FreeBSD QEMU VM with Mesa lavapipe as the Vulkan ICD. CI-friendly; no GPU dependency.

- [ ] Confirm Mesa pkg on FreeBSD-CURRENT includes lavapipe.
- [ ] Document RUNBOOK setup: `MESA_VK_DEVICE_SELECT=*:llvmpipe` or similar, environment for guest-VM tests.
- [ ] CI lane (later): headless render of scene-a, capture to PNG, pixel-diff against expected.

**Done when:** `cargo run --bin frescod` inside QEMU successfully renders scene-a + scene-b through lavapipe. Frame rate is allowed to be terrible.

---

### M4 — Vulkan in QEMU: venus (fast path) (1–2 weeks; risky)

**Goal:** macOS dev environment has GPU-accelerated Vulkan in the FreeBSD guest via venus + virglrenderer + MoltenVK.

- [ ] Validate the combination: existing virglrenderer / venus / MoltenVK builds compatible? May require local QEMU + virglrenderer rebuild with MoltenVK linkage.
- [ ] Document the recipe in RUNBOOK.
- [ ] If the combination doesn't build: file the patches upstream (or maintain locally), or fall back to lavapipe-only and re-evaluate.

**Done when:** the same scene-a + scene-b run in the QEMU FreeBSD guest with measurably-non-toy frame rates. Or: definitive determination that this combo doesn't work today, captured as an issue with next steps.

**Honest risk:** this could take longer than 2 weeks if the existing venus/MoltenVK story needs real plumbing work. M3 (lavapipe) unblocks development independently, so M4 is parallel and not on the critical path.

---

### M5 — D1 final push: bare-metal FreeBSD (4–6 weeks)

**Goal:** the existing ROADMAP.md D1 deliverable — Fresco runs on a real FreeBSD machine, no QEMU, displaying to a real monitor.

This is largely already-existing ROADMAP D1 scope. Calling it out here for sequencing:

- [ ] Replace winit-managed window with direct cdev modesetting on FreeBSD.
- [ ] Native input: usbhid / hkbd → HID-tagged events. No evdev.
- [ ] Vulkan-on-real-HW: drm-kmod + Mesa for the bring-up phase.
- [ ] DPI scaling, multi-monitor coordinates.
- [ ] Power-management hooks (suspend/resume; can stub initially).

**Done when:** boot a real FreeBSD box, see scene-a rendered to its native display. atrium-edit / atrium-term usable on it.

**Critical-path note:** M5 unblocks D2 (Vestibulum), D2.5 (Portcullis launcher integration with Fresco), and is the moment we declare "Fresco is real."

---

### M6 — atrium-text bundle + glyph rendering (D3-aligned) (3–4 weeks)

**Goal:** real text rendering via SPIR-V bundle, not the texture-atlas hack.

- [ ] Build atrium-text bundle (glyph_run op, id 0x2000).
- [ ] Vector-outline rendering on GPU (Pathfinder-style stencil-and-cover, or atlas-based with on-demand regen).
- [ ] Integrate rustybuzz / swash for shaping in the host (per Fresco spec §7).
- [ ] Migrate `fresco-text` crate to feed the new op.

**Done when:** atrium-edit displays text via the new pipeline; multi-script (Latin + Devanagari + Arabic) text shaped + rendered correctly.

---

### M7 — Pergola native: minimum-viable toolkit (parallel track, ~D5 timeframe)

**Goal:** the Pergola crates exist with enough surface to host a foundation app.

This runs in parallel with M5/M6 — bring-up apps stay on `fresco-socket-rs` until Pergola is ready.

- [ ] `pergola-layout`: taffy-based or custom flexbox.
- [ ] `pergola-widgets`: Button, TextField, ScrollView, container layouts.
- [ ] `pergola-events`: input routing, focus, keyboard navigation.
- [ ] `pergola-text`: thin wrapper over `fresco-text`.
- [ ] `pergola`: top-level façade + app entry point.
- [ ] Diff-on-commit pipeline: widget tree → SCENE_NODE_SET deltas.

**Done when:** rewrite atrium-edit on Pergola; visual + behavioural parity with its raw-socket version.

**Defer to later milestones:** `pergola-anim` (M8), `pergola-ax` (M9).

---

### M8 — Declarative animation (D4-aligned)

**Goal:** server-side animations; close the iOS Render Server gap.

- [ ] `ANIMATION_*` op family in CLASS_DISPLAY (id range `0x3000-`).
- [ ] Server-side interpolator in fresco-scene-server.
- [ ] `pergola-anim`: animation surface that targets either client-driven or declarative-to-server backends transparently.

---

### M9 — Atrium GPU ABI + atrium-mesa + accessibility (D5-aligned)

**Goal:** native FreeBSD GPU stack; first-class accessibility.

- [ ] Atrium GPU ABI: native FreeBSD kernel driver replacing drm-kmod.
- [ ] `atrium-mesa` fork: prune to NIR + radv/anv/nvk + nak + vk_common + lavapipe slice. Replace libdrm-coupling with Atrium GPU ABI calls. License audit per LICENSING-POLICY.md.
- [ ] `CLASS_AX` wire protocol per §3.8.3; `pergola-ax` crate; AT integration.

**Done when:** drm-kmod is no longer in the runtime tree; AX tree drives a screen-reader prototype; license inventory is 100% permissive.

---

## Critical path

```
M0 ──► M1 ──► M2 ──► M3 ──► M5 (real HW) ──► M6 ──► M9
            └► M4 (parallel; not blocking)
            └► M7 (parallel; D5/D6 timeframe)
            └► M8 (after M7)
```

**M2 is the heavy lift.** Once frescod runs the SPIR-V bundle dispatch
path with per-connection state and the WM op family, everything after
is incremental.

---

## ROADMAP.md updates needed (M0 task)

- Remove `D5: Slint backend for Fresco`
- Replace with `D5: Pergola native + atrium-mesa + accessibility`
- Add `D4: Declarative animation` if it doesn't already exist (audit)
- Cross-reference `pergola.md` and `LICENSING-POLICY.md`
- Reference the production-rollout doc (this file)

---

## Open items not yet a milestone

- **Multi-bundle composition test** — load atrium-core + atrium-text together in one running scene server. Currently no milestone explicitly exercises bundle interaction.
- **Pixel-diff regression harness** (deferred Step 12 from POC) — depends on M3 (lavapipe) for deterministic SW rendering. Worth folding into M3 as a stretch goal.
- **Performance baseline** — re-run the v2 GO benchmark on real Vulkan (not just Metal POC). Honest measurement of the architectural claim. Could land alongside M5.

---

## Future evolution (post-M9)

### M10 (potential) — GPU-driven scene processor

frescod's per-frame loop today does CPU-side scene-state extraction
(walk per-window state, merge nodes, hand to fresco-vulkan). A future
evolution would push this onto the GPU as a persistent compute kernel:

- Per-window scene state lives in GPU-mapped memory (HOST_VISIBLE |
  DEVICE_LOCAL). CPU envelope-decode writes update entries directly
  into this buffer (memcpy-equivalent; no per-frame extraction loop).
- A persistent GPU kernel (the "scene processor") integrates updates,
  merges across windows, generates draw calls into an indirect-command
  buffer, and dispatches the render passes — all without per-frame
  CPU intervention beyond the page-flip trigger.
- CPU per-frame work shrinks to "envelope decode → write update entry".
  Scene merging, traversal, draw generation, rendering, presentation
  all happen GPU-side in one continuous flow.

This is the in-spirit answer to "could the GPU consume aqueduct
directly?" — the wire-format decode stays CPU-side (sequential
work, microseconds, not worth moving), but everything *after* scene
state lives GPU-side.

Prerequisites:
- Hardware support for efficient persistent compute (vendor-dependent;
  AMD MES-based scheduling, Intel GuC, Apple ASC all support it; older
  GPUs would fall back to CPU-driven path).
- Vulkan extension surface for cooperative-scheduled persistent kernels
  (varies; some hardware exposes it cleanly, some doesn't).
- Recovery path when the persistent kernel wedges (needs a watchdog +
  restart story without full device reset, ideally).
- A scene-state representation that's GPU-friendly (bounded arrays,
  immutable update log + atomic head pointer, no pointer chasing).

This is M10-shaped, not M3-shaped. The current architecture (CPU does
per-frame extraction, GPU does dispatch) is correct for v1; the GPU-
driven version is a refinement once the substrate is mature and the
performance gap of the CPU-side extraction loop is measured to matter.

The Path A / Path B decision is unaffected — Path B (client-rendered
surfaces) bypasses the scene-graph layer entirely, so the GPU-driven
scene processor optimization applies only to Path A content.

### Other future evolutions (placeholder)

- **Bundle pipeline derivative caching** — if many bundles differ only
  in one parameter, share compilation work via Vulkan pipeline
  derivatives. Cap-flagged extension.
- **Persistent / system-wide bundles** — bundles registered once at
  install time, available to all clients without re-registration.
  Adds a trust + permission story; defer until use case appears.
- **Multi-GPU presentation** — render on discrete GPU, present on
  integrated. PRIME-equivalent across atrium-virtio-gpu / atrium-amdgpu /
  atrium-display cdevs. Substrate already supports it via share_fd
  (GPU ABI v2 §6.2); needs WSI-extension work in Mesa.
- **Cross-app GPU work-sharing for bundles** — if two apps register
  byte-identical SPIR-V, frescod could dedupe to one compiled pipeline.
  Adds complexity for memory savings; defer.

---

## Decision log

| Date | Decision |
|---|---|
| 2026-05-04 | Drafted; not started. Pre-production gates settled per planning conversation. |
| 2026-05-05 | M0 → M3 + M2.7e + four migrated socket apps + initial_position landed (eb0c7e1..38e018b, multiple commits). M4 status: virtio-gpu native via atrium-virtio-gpu kmod working in QEMU after probe-priority fix (e210165). M3 lavapipe verified end-to-end (frescod-vulkan-smoke + atrium-test-client renders pixel-identical to host MoltenVK build). |
| 2026-05-05 | Architecture extended: drm-research-findings.md + atrium-gpu-abi-v2.md + fresco-dynamic-bundles.md drafts added. Path A/B decision: Path A (scene-graph dispatch) is the default + preferred; Path B (client-rendered surfaces) added as a documented escape hatch for engines, with explicit architecture-loss caveat (no CAS dedup, no per-node deltas, no GPU traversal benefit for those clients). M10 (GPU-driven scene processor) added as potential future evolution. |
