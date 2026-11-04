# Atrium RDNA4 Vulkan backend (native Tier-3, no Mesa)

**Status:** design, 2026-11-03. Decision recorded in `atrium-gpu-abi-v2.md` §13p.
**Scope:** the first real-GPU Vulkan path for Atrium. It targets RDNA4 (GFX12,
the same family as `atrium-gpu-amd`, gpusim and the project's R9700 cloud GPU)
through GPU ABI v2.1, with no Mesa, libdrm or radv code.

## 0. Decision in one paragraph

Atrium already has most of a Vulkan driver: `atrium-vk-icd` turns Vulkan calls
into aqueduct-gpu `FrameOp`s, `aqueduct-gpu-host` runs them on a `Backend`
(Tier-1 tiny-skia, Tier-2 the atrium-spv CPU compiler, Tier-3 MoltenVK on the
Mac), and `RoutingBackend` already moves frames between Tier-2 and Tier-3 and can
certify them by rendering on both and comparing. The native backend is one more
`Backend`, a Tier-3 that drives an RDNA4 GPU through `atrium-gpu-rs` and the
v2.1 ABI, with a shader compiler that reuses the atrium-spv front end and IR.
That keeps one SPIR-V front end for CPU and GPU, makes Tier-2 the semantic
oracle for every GPU result, and avoids carrying a Linux-shaped C/C++ stack. The
radv winsys port remains the fallback if the compiler stalls (§9).

## 1. Goals and non-goals

**Goals**

- Run the Vulkan subset Atrium itself uses on RDNA4: fresco-vulkan's
  `HeadlessRenderer` and its SPIR-V bundles, Navigator's rendering, and the
  `atrium-vk-icd` example corpus (`loader_graphics_*` and the compute examples).
- Correctness first, proven three ways (§6): against Tier-2, against the R9700,
  and against gpusim.
- Fit GPU ABI v2.1 natively: fd objects, `VM_BIND`, per-queue fences, sealed
  shared buffers for content-addressed assets, user-mode queues where granted.
- Run inside the GPU broker. In production apps never hold a GPU fd (ABI §4.1),
  so the backend lives in the broker's `aqueduct-gpu-host`, one GPU VM per app.

**Non-goals (V1)**

- Passing the full Vulkan CTS. The bar is the subset's own tests plus Tier-2
  cross-checks; CTS groups are adopted as the subset grows.
- Any GPU family other than RDNA4. Other generations are separate backends.
- OpenGL, OpenCL, video decode, ray tracing, mesh shaders, sparse resources.
- Render-target compression (off in ABI 2.1 §5.3).

## 2. Where it sits

```
app ──Vulkan──▶ atrium-vk-icd ──FrameOps (aqueduct-gpu)──▶ broker: aqueduct-gpu-host
                                                             RoutingBackend
                                                  ┌──────────────┴──────────────┐
                                             Tier-2 (CPU)                 Tier-3 native (this doc)
                                        atrium-spv → aarch64              atrium-spv → RDNA4 ISA
                                                                          + PM4 command blobs
                                                                                 │
                                                                    atrium-gpu-rs (ABI v2.1)
                                                                                 │
                                                          /dev/atrium/gpu  (atrium-gpu-amd | gpusim)
```

New crates:

| Crate | Job |
|---|---|
| `atrium-rdna4-isa` | Instruction encodings and operand classes for GFX12, **generated** from AMD's machine-readable RDNA4 ISA (XML) at build time. Assembler and disassembler. No hand-written encoding tables |
| `atrium-rdna4-compiler` | atrium-spv IR → RDNA4 machine code (§3) |
| `atrium-rdna4-cmd` | Command-blob builder: PM4 packets, register state, resource descriptors, barriers (§4) |
| `aqueduct-gpu-host::tier3_rdna4` | The `Backend` impl: resources to BOs, `FrameOp`s to blobs, submit, fences, present (§5) |
| `atrium-rdna4-oracle` | Test harness that runs the same compiled shaders and blobs on the R9700, on gpusim and on Tier-2, and compares (§6) |

`atrium-spv-frontend` and `atrium-spv-ir` are shared with Tier-2 unchanged,
except for additions both backends need (for example subgroup ops).

## 3. Shader compiler

The compiler is about 70% of the work. The plan is to own it, built on the
atrium-spv IR (structured CFG, SSA, per-invocation semantics), and to write the
parts that are easy to get subtly wrong against the generated ISA tables from
day one.

Pipeline:

1. **Front end (shared).** SPIR-V → atrium-spv IR via `atrium-spv-frontend`.
2. **GPU lowering.** Resource access becomes descriptor loads plus buffer and
   image instructions; builtins (invocation ids, frag coord) become their RDNA4
   input registers; push constants become user SGPRs.
3. **Uniformity analysis.** Decide which values are the same across the wave.
   Uniform values live in scalar registers (SGPRs) and use the scalar unit;
   the rest live in vector registers (VGPRs). Wrong answers here are silent
   corruption, so the analysis is conservative and checked by the oracle.
4. **Control flow.** The IR's structured CFG maps onto the execution mask:
   divergent `if`/loops save, narrow and restore `EXEC`; uniform branches use
   scalar branches. Wave32 is the default (GFX12's native width for graphics).
5. **Instruction selection** into a machine IR typed by register class.
6. **Register allocation.** Linear scan on SSA, separate SGPR and VGPR classes,
   spilling to scratch. Occupancy (waves per SIMD) is reported to the command
   builder.
7. **Wait insertion.** Memory and scalar-load results arrive asynchronously and
   need explicit waits before use. GFX12 splits these into separate counters
   (loads, stores, samples, BVH, scalar/constant loads, LDS). AMD's XML gives
   every instruction's functional group (`VMEM`/`BUFFER`/`SAMPLE`/`SMEM`/
   `DATA_SHARE`/...) but **not** its counter effects, so the counter model is a
   small, reviewed table keyed by functional group and memory direction, written
   from the RDNA4 ISA guide and checked instruction class by instruction class
   with R9700 microtests (§6). The pass computes minimal waits from that table,
   never from rules scattered through the compiler.
8. **Hazard pass.** Required NOPs and ordering from the ISA guide's hazard
   section, kept as one table next to the counter model (the XML does not carry
   hazards either).
9. **Encode** with `atrium-rdna4-isa`, emitting a shader binary plus metadata
   (register counts, scratch, LDS, user-SGPR layout).

**Host architecture.** The compiler and command builder are pure Rust and run on any host, so the oracle harness runs on the x86_64 box. Tier-2 runs there too: on x86_64 hosts it compiles through the Cranelift backend (`Target::X86_64Linux` / `X86_64FreeBSD`, x86-64-v3 baseline), while the bespoke backend stays aarch64-only. On x86_64 the Tier-2 differential suites compare the SPIR-V interpreter against Cranelift and keep their pinned values; the bespoke-vs-Cranelift comparison runs on arm64 hosts (the Mac).

**Graphics stages.** RDNA4 has no legacy vertex-shader hardware path: all
geometry runs as NGG primitive shaders, which export positions and primitives
themselves. The compiler therefore emits the vertex stage as an NGG shader
(vertex-only, no geometry amplification in V1). This is the largest graphics
item after the compiler core and is scheduled after compute (§7).

**Fallback for the core.** If register allocation or scheduling quality blocks
progress, LLVM's AMDGPU backend can be fed from the same IR (license:
Apache-2.0 with LLVM exception, accepted by `LANGUAGE-POLICY.md` for Slang). It
is a C++ dependency we prefer to avoid, not a forbidden one.

## 4. Command generation

`atrium-rdna4-cmd` builds the opaque blob that ABI §5.7 executes by reference:

- **PM4 packets** for state, dispatch (`DISPATCH_DIRECT`/`INDIRECT`), draw,
  cache flushes and releases, and timestamp writes.
- **Register state** from AMD's MIT-licensed GFX12 register headers, wrapped in
  typed setters. Context state is built per pipeline once and re-emitted only
  when it changes.
- **Resource descriptors** (buffer, image, sampler) built in a descriptor BO
  bound into the app's GPU VM; shaders load them with scalar loads.
- **Images** use GFX12 swizzle modes chosen per usage; no compression metadata
  (ABI §5.3).
- **Barriers** map Vulkan pipeline barriers to the minimal cache flush and
  invalidate set. Unknown or unhandled barrier kinds are errors, not no-ops
  (the Tier-2 review found silent `_ => {}` arms; this backend must not repeat
  that).

The blob never contains a CPU pointer or a kernel-owned address; everything is a
GPU VA in the app's VM, which the kernel validates (ABI §5.12).

## 5. The Backend impl

`tier3_rdna4` implements `aqueduct-gpu-host::Backend`:

- One GPU VM and queue set per aqueduct session (app), created on the broker's
  client context; quotas come from the app's jail (ABI §4.2).
- Buffers and images are BOs; uploads go through a staging BO and a copy, or a
  CPU mapping where the heap allows.
- Shader bundles that are public content (Fresco's, engine shaders) are compiled
  once, stored in sealed BOs keyed by content hash and shared read-only across
  VMs (ABI §5.13), subject to the proven-possession rule.
- `submit_frame` builds one blob per frame and submits it; completion is a
  syncobj point folded into the broker's kqueue loop.
- `present` hands the target BO to the display by fd (ABI §7).
- `measured_gpu_time_s` comes from blob timestamps calibrated with
  `CLOCK_CALIBRATE` (ABI §5.15), feeding the router's cost model.
- On device lost (ABI §10) the backend reports it; `RoutingBackend` falls back
  to Tier-2 for the affected sessions and rebuilds GPU state when the device
  returns.

## 6. Testing: three oracles

Every GPU test is a **case**: SPIR-V (or a frame of `FrameOp`s), input data, and
expected output. Each case runs on up to three targets:

| Oracle | Proves | When |
|---|---|---|
| **Tier-2** (CPU) | The GPU result is *semantically* right. Same front end, independent backend | Every run |
| **R9700** (silicon) | Our ISA and command blobs do what we think on real hardware | Whenever the shared card is free |
| **gpusim** | Regression, fast and deterministic, available all the time | Every run |

The loop:

1. **Golden capture on the R9700.** When the card is free, the harness runs the
   whole case corpus on it and records, per case: the compiled binary hash, the
   blob hash, output bytes, and timestamps. These goldens are committed to the
   repo (`tests/rdna4/golden/`).
2. **gpusim must match the goldens.** The same binaries and blobs run on gpusim.
   Any mismatch is filed as a gpusim bug (or a capture bug) and fixed in gpusim,
   so the model converges on the silicon.
3. **When the card is busy,** CI runs everything on gpusim and Tier-2 against the
   committed goldens. A compiler change that alters a binary hash re-runs the
   case on gpusim; it is re-captured on the R9700 at the next free slot.
4. **ISA microtests.** Generated from the ISA tables: each instruction class is
   exercised with edge inputs on the R9700, and the recorded results become
   gpusim's instruction-semantics tests. This is the main mechanism for making
   gpusim execute RDNA4 encodings faithfully rather than approximately.

**Transport to the R9700.** The box runs Ubuntu on x86_64, and the provider offers IPMI, so Atrium can later be booted on it natively. Two phases:

- **Now — Ubuntu with the stock amdgpu driver:** the harness uses the amdgpu kernel
  interface only as a way to run our blobs and read back results. It tests the
  compiler and command generation, not our kernel driver. This is test
  tooling and never ships.
- **Later — Atrium booted natively over IPMI, with `atrium-gpu-amd`:** once Atrium runs on x86_64 (it has only been tested on arm64 so far), the same harness runs through ABI v2.1 and
  also tests the kernel driver on silicon (firmware loaded locally; ABI §13m
  only blocks *redistribution*).

## 7. Milestones

| # | Milestone | Exit criterion |
|---|---|---|
| M0 | **Ground work.** Inventory the Vulkan subset ([done: `rdna4-vulkan-subset.md`](rdna4-vulkan-subset.md)); ISA tables generated from AMD's XML ([done: `atrium-rdna4-isa`](../../atrium-rdna4-isa), encoder/decoder round-trip over 6,128 base forms) from fresco-vulkan, Navigator and the ICD examples; generate `atrium-rdna4-isa` from the ISA XML with an assembler/disassembler round-trip test; oracle harness skeleton on gpusim and Tier-2; R9700 transport working | Subset list committed; a hand-assembled shader runs on gpusim and the R9700 with matching output. **R9700 half met 2026-11-04:** `write_42` (hand-assembled, our own PM4) ran on the R9700 and matched; GC base 0x1260 confirmed (`atrium-rdna4-oracle/golden/`). gpusim half waits on its RDNA4 shader core |
| M1 | **First compute.** Storage-buffer compute shaders (fill, saxpy, reductions) compiled from SPIR-V and dispatched through `tier3_rdna4` | Matches Tier-2 and R9700 goldens |
| M2 | **Compiler breadth.** Divergent control flow, loops, LDS, atomics, subgroup ops, spilling; the `atrium-spv-tests` corpus as cases | Corpus green on all three oracles |
| M3 | **Images and samplers** in compute | Texture fetch and sample cases green |
| M4 | **Graphics.** NGG vertex stage, fragment shaders, rasteriser state, blending, depth | fresco-vulkan `HeadlessRenderer` renders a frame matching Tier-2 |
| M5 | **In the broker.** (Native runs on the R9700 need Atrium's x86_64 port.) `RoutingBackend` uses `tier3_rdna4` as Tier-3 on Atrium; present by fd; device-lost fallback to Tier-2 | frescod composites a desktop on gpusim and (with FreeBSD on the box) on the R9700 |
| M6 | **Performance.** Scheduling, occupancy tuning, pipeline caching in sealed BOs | Measured against the router's cost model |

Dependencies: M0–M3 need only `atrium-gpu-rs` on the v2.1 header and gpusim.
M5 needs the `atrium-gpu-amd` isolation fixes (ABI §17 steps 2–3).

## 8. Risks

1. **Compiler correctness.** Uniformity, `EXEC` handling and wait insertion fail
   silently. Mitigated by the three oracles and by deriving waits and hazards
   from the generated ISA tables.
2. **gpusim fidelity.** If gpusim interprets RDNA4 loosely, compiler bugs and
   model bugs look alike. Mitigated by the ISA microtests and golden capture
   (§6); until gpusim is calibrated, R9700 results win every disagreement.
3. **R9700 availability.** It is shared. Goldens make the card a periodic
   calibrator rather than a CI dependency.
4. **NGG complexity.** Mitigated by doing compute first and limiting V1
   geometry to vertex-only NGG.
5. **Scope creep toward full Vulkan.** Mitigated by the M0 subset list; anything
   outside it needs an explicit decision.

## 9. Fallback

If the compiler core stalls at M2, two exits, in order of preference:

1. Keep our compiler front half and use LLVM AMDGPU for instruction selection,
   register allocation and encoding (§3).
2. Port radv onto `atrium-gpu-rs` with a winsys shim (ABI §11). The ABI is
   designed so this remains possible.

## 10. Open questions

1. *Resolved:* the R9700 box runs Ubuntu x86_64 with IPMI. Linux transport now;
   native Atrium boot once the x86_64 port is mature (§6, §7 M5).
2. **gpusim's current ISA coverage.** gpusim lives in a separate repo; its
   shader-execution fidelity needs an audit before M1 relies on it.
3. **Pipeline cache format.** Compiled binaries keyed by (SPIR-V hash, pipeline
   state, compiler version) in Tessera; same key on both tiers?
4. **Wave32 vs wave64** per stage, if a workload benefits from wave64.
