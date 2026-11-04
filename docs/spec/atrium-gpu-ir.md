# Atrium GPU IR (GIR) — the compiler core shared by GPU backends

**Status:** design, 2026-11-04. Refines §3 of
[`atrium-rdna4-vulkan-backend.md`](atrium-rdna4-vulkan-backend.md). The
user asked for a properly layered compiler core, like LLVM, that can target
more than one GPU architecture, instead of a one-shot lowering from the
frontend IR to RDNA4.

## 0. Decision

The shader compiler has three levels:

| Level | Crate | Shared by | Purpose |
|---|---|---|---|
| 1. Frontend IR | `atrium-spv-ir` / `atrium-spv-frontend` (exists) | CPU and GPU | SPIR-V parsed once; close to SPIR-V |
| 2. **GIR** | `atrium-gir` (new) | every GPU target | SSA with explicit regions, address spaces, uniformity; target-independent optimisation |
| 3. Machine layer | `atrium-rdna4-compiler` (first), `atrium-xe-compiler` (possible later) | one target each | instruction selection, register allocation, waits, hazards, encoding |

This is the shape of LLVM IR → MIR and of Mesa's NIR → backend IRs. We take
the shape, not the code. LLVM stays the documented fallback for the RDNA4
machine layer only (backend design §9).

## 1. Why a new level instead of compiling the frontend IR directly

The frontend IR is a faithful SPIR-V translation tuned for the CPU backends.
Three things are missing for GPUs, and the desktop shaders already hit all
three (measured 2026-11-04 on the four `op_*` compute shaders):

1. **Control flow is not reliably structured.** Header blocks carry `merge`
   ids that need not name a real block (op_rectangle: `IfHeader { merge: 11 }`,
   with no block 11), and op_glyph_run's `for` loop has no `LoopHeader` at all:
   it is a plain back edge 12 → 4. Divergent execution needs to know exactly
   where lanes reconverge, so GIR rebuilds the regions from the CFG itself.
2. **Variables are memory.** op_glyph_run keeps its loop counter in a
   `Function`-storage `LocalAlloc`, loaded and stored each iteration. A GPU has
   no cheap per-lane stack; this must become SSA (mem2reg) before register
   allocation.
3. **No notion of the wave.** Nothing says which values are the same for every
   lane (uniform: scalar registers on AMD, cheaper everywhere), which branches
   are divergent, or how wide a subgroup is.

Doing these once, in a target-independent IR, means a second GPU target only
writes level 3.

## 2. Which targets are realistic

| Vendor | ISA public? | Plan |
|---|---|---|
| AMD RDNA4 (GFX12) | Yes: machine-readable XML (MIT) + ISA guide | First target; R9700 is the oracle |
| AMD RDNA3 / older GCN | Yes, same XML family | Cheap follow-on if ever needed |
| Intel Xe / Xe2 | Yes: Intel PRMs | Possible second target; nothing scheduled |
| NVIDIA, Apple, Arm Mali, Qualcomm Adreno | No | Out of scope; GIR does not assume AMD, but no backend is planned |

GIR must therefore not bake in AMD facts: wave size, the scalar/vector
register split, descriptor formats and wait counters all live in level 3.

## 3. GIR data model

**Functions and regions.** A function body is a tree of regions, as in NIR:

```
Region  = [Node]
Node    = Block([Inst])
        | If   { cond: Value, then: Region, else: Region, results: [Phi] }
        | Loop { body: Region, results: [Phi] }      // exits via Break/Continue
Inst terminators inside regions: Break(depth), Continue(depth), Return
```

Every reconvergence point is explicit: the end of an `If`, the end of a
`Loop`. A divergent branch is lowered by the target from this tree, so no
backend reconstructs it. Values merged at the end of an `If`, and loop-carried
values, are `Phi`s that belong to the region node (loop phis at the head, if
phis at the end).

**Values.** SSA. Types: `bool`, `i32`/`u32`, `f32`, `f16`, `i64`/`u64`,
`f64`, plus `vecN<T>` for memory ops only. Arithmetic is scalarised when GIR
is built: both AMD and Intel run one lane per invocation, so a vector add is
N scalar adds. Loads and stores keep their width (a `vec4` load stays one
128-bit load).

**Per-value facts** filled by passes: `uniform` (same across the subgroup),
known constant, and known alignment for addresses.

**Memory and resources.** Address spaces: `Buffer(binding)`, `Push`,
`Workgroup` (LDS / SLM), `Private` (rare after mem2reg; scratch), `Image`.
Buffer access is `load(Buffer(b), byte_offset, width)`, never a raw pointer;
the target decides how `b` becomes an address or descriptor. Atomics carry the
SPIR-V scope and semantics. Builtins (`global_id`, `local_id`,
`workgroup_id`, `vertex_id`, `frag_coord`, …) are ops that the target maps to
its input registers.

**Subgroup ops** are first-class (`ballot`, `broadcast`, `reduce(add|min|…)`,
`elect`), with the subgroup size a property the target supplies (AMD 32 or
64, Intel 8, 16 or 32).

## 4. Building GIR from the frontend IR

1. **CFG rebuild.** Successors from terminators only; the frontend's header and
   merge annotations are ignored.
2. **Loops.** Back edges from a DFS; natural loop bodies; one loop region per
   header. Irreducible control flow is rejected (SPIR-V forbids it).
3. **Reconvergence.** Each conditional branch's merge point is its immediate
   post-dominator within the enclosing loop. Early `return` inside an `if`
   becomes a `Return` terminator that the target lowers by removing lanes from
   the execution mask (op_rectangle and friends all have one).
4. **Structurisation check.** If the CFG does not fit the region tree, fail
   loudly with the SPIR-V offset. No structuriser rewrites in V1: Slang's
   output is structured.
5. **Scalarise** arithmetic, **map** SSBO pointers to `Buffer(b)` + offset
   expressions, **map** builtins.

## 5. Passes (target-independent)

In order, each one small and testable on its own:

1. `mem2reg`: Function-storage locals with only constant indexing become SSA.
2. Constant folding and algebraic simplification (`x / 4` on `u32` → `x >> 2`;
   the desktop shaders do this sixteen times per invocation).
3. CSE (the shaders recompute the same `AccessChain` base repeatedly).
4. Dead code elimination.
5. **Uniformity analysis.** Divergent sources: invocation ids, atomic results,
   loads whose address is divergent, and values defined inside divergent
   control flow and used after it. Everything else is uniform. Conservative:
   a wrong "uniform" is silent corruption.
6. Address analysis: fold constant offsets into memory ops, record alignment.

Passes run to a fixed order, not a pass manager. A GIR verifier (SSA
dominance, region well-formedness, types) runs after each pass in debug and
test builds.

## 6. The target interface

```rust
pub trait Target {
    /// Subgroup sizes the target can run, preferred first.
    fn subgroup_sizes(&self) -> &[u32];
    /// Rewrite ops the target cannot do natively (e.g. 64-bit divide)
    /// into GIR it can, before code generation.
    fn legalize(&self, f: &mut gir::Function);
    /// Produce the binary and its metadata.
    fn compile(&self, f: &gir::Function, abi: &Abi) -> Result<Binary, Error>;
}
```

`Abi` says how resources and builtins reach the shader (which user registers,
which descriptor layout). `Binary` is the code plus register counts, scratch,
LDS size and the input layout the command builder needs.

## 7. The RDNA4 machine layer (level 3, first target)

`atrium-rdna4-compiler`:

1. **Instruction selection** from GIR into a machine IR of RDNA4 instructions
   on virtual registers of class SGPR, VGPR or lane mask. Uniform GIR values
   take SGPRs and scalar instructions; the rest take VGPRs.
2. **Control flow.** `If`/`Loop`/`Break`/`Return` → EXEC save, narrow and
   restore, with scalar branches to skip regions whose mask is empty; uniform
   conditions use plain scalar branches.
3. **Register allocation.** Linear scan over the linear order of the region
   tree, with liveness extended across loop back edges. Separate SGPR and VGPR
   pools. Written generic over register classes so an Intel backend can reuse
   it.
4. **Wait insertion** from the reviewed counter table (backend design §3.7).
5. **Hazard pass** from the hazard table (§3.8).
6. **Encode** with `atrium-rdna4-isa`.

## 8. Testing

* **GIR interpreter** (in `atrium-gir`): runs a GIR function for a whole
  workgroup with real subgroup semantics (lanes, divergence, reconvergence).
  It gives every oracle case its expected output without a hand-written
  reference, and it checks GIR against Tier-2, which runs the same SPIR-V
  through the frontend IR. A pass bug shows up as interpreter-before ≠
  interpreter-after.
* **R9700 and gpusim** run the RDNA4 binaries as before (backend design §6).
* Every pass has unit tests on small GIR functions built in code; the four
  desktop compute shaders and the 21 Tier-B compute examples are the
  integration corpus.

## 9. Milestones

| | Deliverable | Done when |
|---|---|---|
| G0 | This spec | Agreed |
| G1 | `atrium-gir`: data structures, verifier, builder from the frontend IR, interpreter | The 4 desktop compute shaders build into GIR and the interpreter matches Tier-2 on them |
| G2 | Passes: mem2reg, folding, CSE, DCE, uniformity | Interpreter results unchanged by every pass; op_glyph_run has no locals left |
| G3 | RDNA4 machine layer: isel, control flow, regalloc, waits, encode | The 4 desktop compute shaders match on the R9700 |
| G4 | Tier-B compute widening (push constants, LDS, subgroups, images) | The 21 examples match on the R9700 |

G1–G3 replace milestone M1 of the backend design; M2 onward are unchanged.

## 10. Open questions

* Should Tier-2 (CPU) eventually consume GIR too, running a subgroup as SIMD
  lanes the way lavapipe does? It would share the passes, but Tier-2 works
  today, so this waits until GIR is stable.
* Graphics stages (NGG vertex, fragment) add inputs, outputs and
  interpolation to GIR; designed when M4 starts, not now.
