# Tier-2 shader codegen — correctness constraints

> **Status.** Phase-0 deliverable for `docs/spec/tier2-renderer.md`.
> Living document — add new constraints when new bug classes are
> discovered.
>
> **Provenance.** Distilled from PPTK's
> `crates/pptk-codegen-arm64/CONSTRAINTS.md` (the upstream
> document for the bespoke ARM64 backend's lessons learned
> across 100+ debug sessions), filtered for what applies to
> SPIR-V → ARM64/x86_64 shader codegen, plus new constraints
> specific to the three-tier architecture (bespoke + Cranelift
> + interpreter oracle) that tier-2 uses.

**Every new lowering or IR rewrite MUST satisfy ALL of these
or it will reproduce a known bug.**

Constraints are grouped by where the rule applies:
- §A — frontend (SPIR-V → atrium-spv-ir)
- §B — IR-level invariants
- §C — bespoke ARM64 backend
- §D — bespoke x86_64 backend
- §E — Cranelift adapter
- §F — cross-backend parity and testing
- §G — methodology and operational

---

## §A — Frontend (SPIR-V → atrium-spv-ir)

### A1 — Reject unstructured control flow at parse time

SPIR-V emitted by `glslc` / `slangc` / `dxc` is always
structured (every `OpBranchConditional` / `OpSwitch` is
preceded by `OpSelectionMerge` or `OpLoopMerge`). Arbitrary
SPIR-V from a custom tool may have unstructured CFGs we
can't represent.

**Rule.** If the frontend encounters a branch without a
preceding merge instruction, return
`VK_ERROR_INVALID_SHADER_NV` immediately. Do not attempt
recovery, do not synthesize a merge. Apps with unsupported
SPIR-V get a clear rejection at `vkCreateShaderModule`
rather than a misrendering at draw time.

**Bug class avoided.** Silent miscompilation of irreducible
control flow into structured form, producing wrong output
that's nearly impossible to debug from pixels alone.

### A2 — Preserve SPIR-V source offsets in IR provenance

Every `atrium-spv-ir::Inst` carries a `source_spirv_offset:
u32`. The frontend populates this for every instruction it
emits; downstream IR passes preserve it through rewrites
(when an op is replaced, the replacement inherits the
original's offset).

**Rule.** The bespoke backend uses this to populate the
`.pcmap` sidecar (§10 of the renderer spec); the diff
harness uses it to localise where pixel disagreements
came from. Dropping source offsets makes both useless.

**Bug class avoided.** PPTK's runbook explicitly cites
retrofitting the `PC_MAP_OUT` sidecar as their biggest
hindsight regret. We're not retrofitting; we build it in.

### A3 — Validate SPIR-V capabilities before parse

`glslc` defaults to `--target-env=vulkan1.3` which enables
many capabilities (Int8, Float16, GroupNonUniform, etc.)
that tier-2 doesn't honor. Accepting a shader using these
ops and then producing wrong code is worse than rejecting
it.

**Rule.** Before parse, walk `OpCapability` declarations.
If any capability is outside our supported set (see
`atrium-spv-frontend::SUPPORTED_CAPABILITIES`), reject the
shader. Apps that genuinely need an unsupported capability
get told so instead of silently degrading.

### A4 — Structured-CFG recovery is one-shot, not iterative

The frontend builds `BlockKind::{If, Loop, Switch, Linear}`
from `OpSelectionMerge` / `OpLoopMerge` markers in a single
pass over the SPIR-V function. No fix-up passes, no merging
of synthesised blocks.

**Rule.** If the recovery pass produces a malformed
structure (e.g. an `If` block whose merge target doesn't
post-dominate both arms), fail loudly with the SPIR-V
offset. Don't try to patch it.

---

## §B — IR-level invariants

### B1 — SSA dominance must hold at all times

Every use of a value V must be dominated by V's definition.
The IR validator enforces this on construction and after
every rewrite pass.

**Rule.** Backends assume valid SSA; they will produce wrong
code if given a non-SSA IR. The validator runs in debug
builds at every IR-to-IR boundary and in CI on every test
shader.

### B2 — Vector types stay first-class through to instruction selection

`Type::Vec4(f32)` is not lowered to four `f32` SSA values
in the frontend or middle. Only the final ISel step decides
whether to emit NEON ops (preserves vec) or scalar lanes
(forced fallback).

**Rule.** Don't write IR passes that walk vector values
lane-by-lane. They're opaque atoms until the backend.

**Bug class avoided.** Early lowering loses vectorisation
information; we'd need an autovectoriser to recover, which
is a deep rabbit hole. Late lowering keeps the option open.

### B3 — vec3 occupies vec4 storage; ignore the high lane

ARM64 NEON has no native vec3 ops. We map `Vec3(f32)` to
a 4-lane register with the high lane carrying undefined
data. Loads/stores of vec3 read/write only 3 lanes.

**Rule.** Code generators must not assume the high lane of
a vec3 is zero (or any specific value). Any IR op that
produces a vec3 must explicitly write all 3 lanes; ops
that consume a vec3 must mask their use to 3 lanes.

**Bug class avoided.** Reading the high lane of a vec3 as
if it were "the alpha" produces nondeterministic output
depending on what was in the register from a previous op.

### B4 — Bool is i32 with 0/1 convention

SPIR-V's `Bool` becomes `Type::I32` carrying 0 (false) or
1 (true). Comparison ops produce 0/1 ints; logical ops
operate on 0/1 ints; `OpSelect` reads as `cmp ne 0`.

**Rule.** Do not use packed-bool / bitmask representations
in the IR. The backend pattern-matches `i32 cmp ne 0`
followed by `b.cond` as a fused compare-branch — using
i32 explicitly makes the pattern obvious.

### B5 — No general heap; all pointers are typed slots

Pointers in atrium-spv-ir are typed offsets into one of
three flat blocks: uniforms, push-constants, or per-vertex
attributes. Frontend resolves all pointer arithmetic to
constant offsets at translate time using the descriptor-
set layout.

**Rule.** Backends never see runtime-computed pointer
arithmetic. If the frontend can't fold a pointer to a
constant offset, the shader uses a SPIR-V feature we
don't support (dynamically-indexed descriptor arrays
beyond their bounds, etc.) — reject at the frontend.

---

## §C — Bespoke ARM64 backend

### C1 — Flag-setting forms are explicit, not implicit

ARM64 has separate ADD / ADDS variants; only ADDS updates
NZCV. If subsequent code reads flags (e.g. via `B.cond`),
the prior arithmetic *must* be the `-s` form.

**Rule.** The instruction selector tracks which IR values
feed into branches and emits flag-setting forms for those
producers. The encoder (`pptk-codegen-arm64::asm.rs`)
asserts on flag-set/flag-read mismatches in debug builds.

**Bug class avoided (PPTK).** Lifting `and r15b,1; jne` to
non-flag-setting `and w17` then reading stale NZCV from a
previous unrelated op. This appeared "everywhere" in 7-Zip
and was the highest-volume bug class in PPTK.

### C2 — Narrow-register flag updates need the `-s` form too

`AND w17, w0, #1` doesn't update flags. `ANDS w17, w0, #1`
does. Same for `SUB`/`SUBS`, `ADD`/`ADDS`, `BIC`/`BICS`,
etc.

**Rule.** When the selector knows the result feeds a
branch, use the `-s` form even for narrow (W-register) ops.
PPTK had three separate fixes for this pattern (narrow-ALU,
narrow-NEG, narrow-CMP); we encode the rule once and apply
it uniformly.

### C3 — Logical immediates must be representable bit-masks

ARM64 logical immediates (used by AND / ORR / EOR / TST)
must be one of 5334 representable "bit-mask" patterns.
Arbitrary constants don't fit.

**Rule.** Before emitting an AND-immediate, check whether
the constant fits via `pptk-codegen-arm64::asm::is_logical_imm`.
If not, materialise the constant via `MOV` to a scratch
register first and use the register form. The encoder
asserts on invalid immediates rather than silently
truncating.

### C4 — Arithmetic immediates are 12 bits, optionally shifted

`ADD`/`SUB`/`CMP` accept a 12-bit unsigned immediate,
optionally left-shifted by 12. Constants outside this
range need materialisation.

**Rule.** Same shape as C3 — check fit before emit; fall
through to register form on miss.

### C5 — Load/store offsets are unsigned, scaled by element size

Unsigned 12-bit offset, scaled by access size (1 for byte,
2 for halfword, 4 for word, 8 for dword, 16 for qword).
Larger offsets need address materialisation via `ADD` to
a scratch register.

**Rule.** Address materialisation costs one extra
instruction but is unconditional when the offset doesn't
fit. Don't try to coalesce multiple OOB offsets into a
shared base; that's an optimisation for later (see G3).

### C6 — Atomic ops use LSE instructions, no LL/SC loops

SPIR-V's `OpAtomicIAdd`, `OpAtomicCompareExchange`,
`OpAtomicExchange` etc. lower to ARMv8.1 LSE atomics:
- `OpAtomicIAdd` → `LDADDAL`
- `OpAtomicCompareExchange` → `CASAL`
- `OpAtomicExchange` → `SWPAL`
- `OpAtomicAnd` / `Or` / `Xor` → `LDCLRAL` / `LDSETAL` / `LDEORAL`

**Rule.** Tier-2 targets ARMv8.1+ (FreeBSD/aarch64 on
M-series and modern Cortex). LSE atomics are mandatory.
Do not emit LDXR/STXR loops as a fallback; if a target
lacks LSE, reject at daemon initialisation.

**Bug class avoided (PPTK).** 7-Zip's parallel ZIP encoder
deadlocked because pptk-lift originally treated
`lock cmpxchg` as a non-atomic load+compare+store.
SPIR-V atomics have the same semantic weight; emitting
them as non-atomic sequences will deadlock under
multi-threaded rasterisation (phase 8+).

### C7 — Branch reach: B/BL is ±128 MB, B.cond is ±1 MB

ARM64 direct-branch immediate ranges are limited.
Per-shader `.so` sizes are tiny (<1 MB typically) so
both fit comfortably within a single shader. Cross-`.so`
calls (e.g. into `atrium-spv-runtime` for texture sample)
go through the GOT and have unbounded reach.

**Rule.** Selector emits direct `B`/`BL` for intra-`.so`
control flow and indirect `BLR` via GOT for runtime calls.
If a future shader grows beyond 128 MB of code, we have
bigger problems than branch reach.

### C8 — NEON encoder gaps must be filled before SIMD ISel

PPTK's `asm.rs` covers the NEON subset 7-Zip used (mostly
move/extract). Shaders need substantially more:

- Shuffle/permute (`TBL`, `TBX`, `ZIP1`/`ZIP2`, `UZP1`/`UZP2`, `EXT`)
- Dup-from-scalar (`DUP V, W`) — for splatting uniforms
- Float conversions (`SCVTF`, `FCVTZS`)
- Reciprocal estimates (`FRECPE`, `FRSQRTE`)
- Min/max/abs (`FMIN`, `FMAX`, `FABS`)
- Dot product (`FDOTP` on ARMv8.6+; fallback via FMUL+FADDP for older)

**Rule.** Before SIMD ISel lands (phase 8 in the renderer
spec), extend `pptk-codegen-arm64::asm.rs` with these ops.
Adding encoder entries is mechanical and well-tested in
the PPTK pattern; ~1-2 weeks of additive work.

### C9 — Floating-point: default rounding, no denormals, NaN-quiet

Shaders default to round-to-nearest-even, flush-denormals-
to-zero, and quiet NaNs propagate (never signal). ARM64
FPCR can be configured for any combination; we set it once
at shader entry.

**Rule.** Each `atrium_*s_main` entry point's prologue
sets FPCR to the canonical shader state. Don't assume the
caller (rasterizer) set it for us — different shaders
might have different expectations and we can't guarantee
caller state.

**Bug class avoided.** Inheriting host FPCR (e.g. with
denormals-as-zero off when the shader expects FTZ) makes
shader output non-deterministic between rasterizer
invocations — a debugging nightmare.

### C10 — No callee-saved registers across the shader entry boundary

The shader `atrium_*_main` entry functions follow the
native AArch64 ABI for the *call*, but inside the shader
we treat all general-purpose registers as caller-saved
and use them freely. The rasterizer's per-pixel call site
spills whatever it needs before calling the shader.

**Rule.** No register allocator pressure from preserving
x19-x28; the regalloc treats them as scratch. This
trades some rasterizer-side spill cost for simpler shader
codegen and is the right call given shaders are leaf
functions in the hot loop.

---

## §D — Bespoke x86_64 backend

(Phase 4; deferred for FreeBSD-ARM64-only v1.)

### D1 — Use the System V AMD64 ABI for shader entry calls

Same shape as ARM64 §C10 — shader entry follows the host
ABI; inside the shader we treat all GPRs (rax, rcx, rdx,
rsi, rdi, r8-r11) as scratch.

### D2 — Prefer AVX2 over SSE2 for vector ops

x86_64 has SSE2 baseline + AVX2 widely supported. AVX2
gives 256-bit ops (vec8<f32>) which lets one fragment
shader call process 8 pixels at a time vs 4 with SSE2.
Tier-2's target hardware (modern Intel/AMD) has AVX2;
we don't bother with SSE-only fallback.

### D3 — x86_64 floating-point determinism mirrors ARM64 §C9

Set MXCSR at entry: round-to-nearest, flush-to-zero,
mask all exceptions. Inheriting host MXCSR is the same
non-determinism trap as on ARM64.

### D4 — x86 flag semantics: SF/ZF/CF/OF mapped 1:1 from IR

x86 has direct ALU-sets-flags semantics matching what our
IR's compare-then-branch pattern needs. Less polarity-
flipping than ARM64's NZCV/x86-flag translation that PPTK
had to do — our IR is the source of truth, not someone
else's x86 binary.

---

## §E — Cranelift adapter

### E1 — Cranelift IR types must match atrium-spv-ir types exactly

The adapter walks atrium-spv-ir and emits Cranelift IR.
Every IR type has a 1:1 Cranelift IR type mapping:
- `I32` → `I32`
- `F32` → `F32`
- `Vec4(F32)` → `F32X4` (Cranelift's 128-bit vector type)
- `Vec3(F32)` → `F32X4` (same as vec4; high lane is
  garbage per §B3)
- `Vec2(F32)` → `F32X2`

**Rule.** No truncation or extension at the type boundary.
If atrium-spv-ir grows a type Cranelift can't represent,
the adapter returns `Unsupported` and the bespoke backend
must handle that op (or both backends fail, exposing the
shader to the user as unsupported).

### E2 — Cranelift output goes through the same ABI as bespoke

The adapter emits a Cranelift function with the
`atrium_*_main` signature exactly as bespoke does. The
.so loader code on the daemon side cannot tell which
backend produced the binary — only the `backend_kind`
field in the metadata blob distinguishes them.

### E3 — Cranelift fallback is not a "second-class citizen"

A shader compiled via Cranelift must be functionally
indistinguishable from one compiled via bespoke. Different
pixel output between the two is a backend bug (in
whichever backend disagrees with the interpreter — see
§F1).

**Rule.** Don't introduce "Cranelift quirks." If
Cranelift's lowering of an op differs subtly from
bespoke's, write a custom Cranelift IR sequence that
matches bespoke's semantics, even at perf cost.

---

## §F — Cross-backend parity and testing

### F1 — Differential testing is the canonical correctness check

Every test shader is compiled three ways (bespoke,
Cranelift, interpreter); outputs are compared pixel-for-
pixel with `assert_shader_agrees`. Bug-localisation by
disagreement pattern:

| Bespoke | Cranelift | Interpreter | Diagnosis |
|---|---|---|---|
| A | A | A | shader works ✓ |
| A | A | B | frontend bug (both prod paths inherit) |
| A | B | B | bespoke bug |
| B | A | B | Cranelift adapter bug |
| A | B | C | multiple bugs; investigate carefully |

**Rule.** Every reported bug must be reproducible as a
failing `assert_shader_agrees` test. "I see wrong pixels
in app X" is not actionable; "this fragment shader
disagrees by 23 in the red channel at pixel (12, 7)" is.

**Bug class avoided (PPTK).** PPTK's biggest hindsight
regret is leaning harder on Cranelift-as-oracle from day 1.
We bake it into phase 0.

### F2 — Interpreter agrees with hardware on bit-exact semantics

The interpreter is the "ground truth" — but only if its
own semantics match what the SPIR-V spec demands. We
follow the spec rigorously: IEEE 754 binary32 for `f32`,
NaN propagation rules per the spec's "preferred"
behaviour, deterministic rounding.

**Rule.** When the SPIR-V spec leaves something
implementation-defined (e.g. corner-case NaN
propagation), pick *one* behaviour and document it in
the interpreter source. Both production backends then
match that documented choice. The interpreter is the
authority; the spec is the interpreter's authority.

### F3 — Shader-ABI version bump invalidates the cache

When `TIER2_SHADER_ABI_VERSION` increments, the cache
directory path bumps to `v{N+1}/`. Old caches become
unreachable; first launch post-bump recompiles
transparently.

**Rule.** Don't try to migrate cached `.so`s across ABI
versions. Recompile is cheap (50-200 ms per shader);
binary migration is expensive engineering with high bug
surface.

---

## §G — Methodology and operational

### G1 — Test against real shaders, not synthetic

Synthetic tests catch encoder bugs. They DON'T catch:
- Subtle interactions with shader runtime semantics
  (perspective interpolation, derivative computation)
- Cross-pixel flag/state propagation in tile-parallel
  rasterisation (phase 8+)
- Memory ordering issues with multi-threaded compute
  (phase 9+)

**Rule.** Every meaningful change must be validated
against the full Khronos sample matrix (gears, triangle,
texturedcube, deferred, etc.). Pure unit tests are
necessary but not sufficient.

**Bug class avoided (PPTK, C17).** The exact same lesson.

### G2 — Don't optimise prematurely

The pptk-lift lessons show that EVERY "smart" optimisation
introduced before correctness landed had subtle bugs that
took weeks to find.

1. Get correctness first via straightforward 1:1 lowering
   from atrium-spv-ir to instructions.
2. Use the differential harness to validate.
3. Measure where the time goes via the rasterizer's
   per-shader profiler.
4. Optimise the hot mnemonics with full test coverage.

**Rule.** Do not introduce constant folding, dead-store
elimination, or value propagation in the codegen *before*
the bespoke backend can compile a meaningful shader
end-to-end. Premature opts are the highest-risk way to
spend engineering time.

**Bug class avoided (PPTK, C18).** Same lesson, slightly
different wording.

### G3 — Coexistence with Cranelift fallback (the `can_handle` predicate)

The bespoke backend exposes
`bespoke::can_handle(&Function) -> bool`. It returns true
only when bespoke knows it can produce correct output for
every op in the function. Edge cases default to false →
Cranelift fallback.

**Rule.** Don't widen `can_handle` to include ops the
bespoke backend hasn't been tested on. Adding op coverage
is: (1) implement the op in bespoke, (2) add tests via
differential harness, (3) widen `can_handle`. In that
order, never out of order.

**Bug class avoided (PPTK, C16).** Same pattern, applied
to our two-backend topology instead of PPTK's old
single-backend + Cranelift fallback.

### G4 — Don't trust glslc/dxc/slangc to produce identical SPIR-V

Different SPIR-V compilers emit functionally equivalent
but textually different SPIR-V for the same GLSL/HLSL
source. Two apps shipping the "same" shader may produce
two distinct cache entries.

**Rule.** Don't try to canonicalise SPIR-V before
hashing. The cost (translator-side complexity) outweighs
the benefit (cache size). Tolerable until the cache
grows to hundreds of redundant copies; defer the
canonicaliser until then.

### G5 — Build tracing/instrumentation infrastructure FIRST

PPTK's runbook explicitly cites retrofitting the
`PPTK_PC_MAP_OUT` sidecar (after Cranelift had it natively
but bespoke didn't) as their biggest mistake.

**Rule.** Phase 0 of tier-2 includes the PC-map sidecar,
the differential test harness, and per-shader runtime
metrics *before* any real codegen ships. Don't retrofit.

### G6 — Every IR-pass invariant is encoded as a `debug_assert`

The IR validator runs at every pass boundary in debug
builds. Release builds skip it (the validator is too slow
for production cache-miss compiles).

**Rule.** Every constraint in §B has a corresponding
`debug_assert!` in the validator. New constraints get a
new assertion. When a bug is debugged to "I expected
invariant X but got Y," the fix is twofold: (1) fix the
pass that violated X, (2) add a `debug_assert!` to the
validator so the next violation is caught earlier.

### G7 — Document the gap to "production"

Tier-2 v1 doesn't have to be perfect; it has to be
honest about what it doesn't do yet. The bespoke
backend's op coverage is a moving target, and the
"Cranelift fallback" path may have suboptimal perf.

**Rule.** The `atrium-spv-compile` binary exits with
a structured JSON line on its stderr per shader:

```json
{"shader_hash": "abc...", "backend": "bespoke", "ops": 142,
 "compile_ms": 45, "size_bytes": 2150}
```

The daemon's metrics aggregate these. Operators
(`atrium-pkg admin shader-cache stats`) get visibility
into where the perf is.

---

## Constraint coverage status

| Constraint | Encoded in IR validator? | Encoded in CI tests? |
|---|---|---|
| A1 — Unstructured CFG rejection | n/a (frontend) | yes |
| A2 — Source offsets in provenance | yes | yes |
| A3 — Capability validation | n/a (frontend) | yes |
| A4 — One-shot CFG recovery | n/a (frontend) | yes |
| B1 — SSA dominance | yes | yes |
| B2 — Vector types first-class | yes | yes |
| B3 — vec3 high-lane discipline | yes | yes |
| B4 — Bool is i32 | yes | yes |
| B5 — Pointers are constant offsets | yes | yes |
| C1 — Flag-setting forms explicit | yes (selector) | yes (diff harness) |
| C2 — Narrow-reg flag updates | yes (selector) | yes (diff harness) |
| C3 — Logical immediates check | yes (encoder asserts) | yes |
| C4 — Arithmetic immediates check | yes (encoder asserts) | yes |
| C5 — Load/store offset check | yes (encoder asserts) | yes |
| C6 — LSE atomics required | yes (selector) | yes (diff harness) |
| C7 — Branch reach | n/a (size-bounded) | n/a |
| C8 — NEON encoder coverage | n/a (additive work) | yes |
| C9 — FPCR canonical state | yes (prologue emit) | yes (diff harness) |
| C10 — Caller-saved discipline | n/a (regalloc handles) | n/a |
| D1-D4 — x86_64 mirror of ARM64 | (phase 4) | (phase 4) |
| E1 — Cranelift type parity | yes (adapter) | yes (diff harness) |
| E2 — Same ABI as bespoke | yes (adapter) | yes |
| E3 — Functional indistinguishability | n/a | yes (diff harness) |
| F1 — Differential testing | n/a | yes (the canonical test) |
| F2 — Interpreter as authority | n/a | yes |
| F3 — ABI bump invalidates cache | yes (cache path) | yes |
| G1-G7 — Methodology | n/a | n/a |

---

## Provenance and changes from PPTK CONSTRAINTS.md

| PPTK | Status here | Notes |
|---|---|---|
| C1 PE image mirror | dropped | No PE; we have an IR |
| C2 XMM pinned | dropped | We have regalloc |
| C3 Exhaustive operand kinds | C1+C2 here | Generalised to "explicit flag-setting forms" |
| C4 Atomic memory ops | C6 here | Same rule; SPIR-V atomics instead of x86 lock prefix |
| C5 32/16/8-bit destination | partial → B4 | SPIR-V is explicit about width; less of an issue |
| C6 RIP-relative lea/mov | dropped | No RIP; SSA references |
| C7 Native NZCV + liveness | C1+C2 here | Same lesson |
| C8 RSP pinned to own reg | dropped | Use host SP |
| C9 pop rsp semantics | dropped | No stack manipulation |
| C10 Calls clobber caller-saved | C10 here | Regalloc handles |
| C11 Indirect calls via shim | partial → C8 | Texture-sample dispatch is the case |
| C12 IAT calls | dropped | No IAT |
| C13 FS/GS segments | dropped | No segments |
| C14 String ops | dropped | No string ops in shaders |
| C15 xor as zero idiom | dropped | SPIR-V uses OpConstantNull cleanly |
| C16 Cranelift fallback | G3 here | Same shape, different topology |
| C17 Real-binary testing | G1 here | Same lesson, different binaries |
| C18 Don't optimise prematurely | G2 here | Same lesson |
| C19 iced-x86 mnemonic groupings | dropped | rspirv is cleaner |
| C20 Honor flag liveness | C1 + G2 here | Optimisation deferred per G2 |

New constraints with no PPTK predecessor:
- A1-A4 (frontend invariants)
- B2, B3 (vector type discipline)
- B5 (typed pointer slots)
- C9 (FPCR canonicalisation — PPTK ran x86 code under any FPCR)
- E1-E3 (Cranelift adapter — PPTK didn't have a parallel backend)
- F1, F2 (differential testing — PPTK had this informally; we make it canonical)
- F3 (ABI bump policy)
- G4-G7 (methodology specific to three-tier architecture)
