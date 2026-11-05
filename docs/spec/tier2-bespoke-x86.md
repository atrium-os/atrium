# Tier-2 bespoke backend for x86_64

Status: phases 1-8 landed 2026-11-05 (phase 5 scalarised only; packed forms move to phase 9); phase 9 run on a cloud Xeon passes the bar (§6); packed vectors landed. Owner: the x86_64 port thread. Companion to
[x86_64-port.md](x86_64-port.md) §1 and the arm64 backend in
`atrium-spv-backend-bespoke/`.

## 1. Why

On arm64 the bespoke backend reaches about 90% of hand-written code, while Cranelift
reaches about 70%. On x86 today Cranelift is the only Tier-2 backend. The gap matters once
frescod renders through Tier-2 on an x86 desktop, so the x86 path should get the same
treatment.

## 2. Decisions

- **Encoder: iced-x86's `code_asm`, not a new pptk crate.** iced-x86 1.21 is already a pptk
  dependency. It is MIT licensed and provides a complete x86-64 assembler: every
  legacy/VEX/EVEX encoding, labels, and branch-size relaxation. Its decoder gives tests a
  free round-trip check. Writing an encoder in pptk would only re-implement it.
- **A new crate, `atrium-spv-backend-bespoke-x86`**, with the same public surface as the
  arm64 crate: `compile_blob(module, target) -> Result<BlobOutput, BackendError>`, and
  `BackendError::Unsupported` meaning "use Cranelift". `atrium-spv-compile` tries it first
  on x86_64 targets, exactly as it tries the arm64 bespoke backend on aarch64.
- **ISA baseline: x86-64-v3** (AVX2, FMA, F16C, BMI1/2, LZCNT). This is the same baseline
  as the Cranelift x86 path, and the R9700 box's Xeon E-2288G has all of it. Scalar float
  ops use VEX forms (`vaddss`, `vfmadd231ss`, `vroundss`, `vcvtps2ph`). The VEX forms avoid
  SSE/AVX transition stalls and take three operands, so no copies are needed to keep a
  source alive.
- **ABI: SysV AMD64**, with the signatures fixed by the loader (`atrium-spv-loader/src/dlopen.rs`:
  VsMain, FsMain, FsSpanMain, CsMain). Integer args go in rdi, rsi, rdx, rcx, r8, r9, then
  the stack. Float args go in xmm0–7. All XMM registers are caller-saved. Callee-saved
  GPRs (rbx, rbp, r12–r15) are pushed only if used.
- **Blob format:** `ARCH_X86_64`. Same flat, relocation-free rule as arm64: runtime helpers
  (image read/write, barrier) are called through the image-table function pointers, and
  constants are materialized inline or kept in a RIP-relative literal pool at the end of
  the function.

## 3. What does not port 1:1 from arm64

| arm64 backend | x86 backend |
|---|---|
| 32 V regs, 31 W/X regs | 16 XMM, 14 usable GPRs. Spilling is the common case, not the exception, so spill mode is the default design and not a retry path. |
| 3-operand integer ops | 2-operand integer ops (`add dst, src`); use `lea` for 3-operand add and shift-add. BMI2 `shlx/shrx/sarx` give 3-operand shifts. |
| `cset`/`csel` on NZCV | `setcc` + `movzx`, `cmovcc`. Float compares (`vucomiss`) need the parity flag for unordered: `FOrd*` and `FUnord*` differ only in how PF combines. |
| `fmov` lane moves, `ins`/`dup` | `vmovd`, `vinsertps`, `vpermilps`, `vbroadcastss`, `vextractps`. |
| `udiv` (no trap) | `div` traps on zero and on INT_MIN/-1. SPIR-V leaves those results undefined, but they must not crash, so guard them the way the Cranelift path does. |
| `rbit` | No single instruction. Use a bit-reverse sequence (`bswap` plus mask swaps), or `Unsupported` until a shader needs it. |
| `swpal`/`ldaddal` atomics | `lock xadd`, `xchg`, `lock cmpxchg` loops for and/or/xor/min/max. |
| AAPCS64 stack args (8-byte slots, Apple packs) | SysV: 8-byte slots above the return address. |

## 4. Phases

Each phase lands with the differential tests for its op set green. Anything outside the
phase returns `Unsupported`, so x86 output is never silently wrong.

1. **Skeleton.** Crate, ABI prologue and epilogue, `Return`, scalar f32 constants and
   arithmetic, `Store` to Output. Wire it into `atrium-spv-compile` and the
   `atrium-spv-differential` runner set. The decoder round-trip runs in unit tests.
2. **Register allocation + spills.** Linear scan over XMM and GPR files with write-once SSA
   spill slots (the arm64 design), sized for 16 XMM registers.
3. **Integer, bool and control flow.** Int ops, compares, `Select`, `Branch`/`BranchCond`/
   `Switch`, `Phi` with critical-edge splitting (reuse the arm64 pass), conversions,
   `Bitcast`.
4. **Memory.** (Also brought the vertex stage forward: its ABI is mostly memory.) `Load`/`Store`/`AccessChain`/`PtrOffsetDynamic`/`LocalAlloc`, uniforms, push
   constants, varyings, storage buffers.
5. **Vectors.** Shuffle/extract/insert/dot/`MatrixTimesVector`, scalarized first, then
   packed XMM where it pays.
6. **Compute.** Builtins, workgroup memory, `Barrier` (via the image-table pointer), atomics.
7. **Images and derivatives.** Calls through the image table, `DPdx`/`DPdy`/`Fwidth`
   (quad-based, as on arm64).
8. **Fragment span thunk.** The batched FS entry, with the SysV stack-argument layout.
9. **Benchmark** against Cranelift-x86 with `bench_fs_codegen` on the R9700 box's Xeon. The
   pass bar is the arm64 one: 90% of hand-written code on the kernels the bench covers.

## 5. Testing

- Every phase adds its op set to `atrium-spv-differential` (interpreter vs bespoke-x86 vs
  Cranelift-x86), plus the 110-case compute differential.
- `ATRIUM_SPV_FORCE_SPILL=1` runs everything in spill mode, as on arm64.
- Encoder bugs are caught twice: by iced's own encoder validation, and by a decoder
  round-trip on every emitted function in debug builds.

## 6. Results

`bench_fs_codegen` (release), 2026-11-05, cloud VM "Intel Xeon Processor @ 2.10GHz", 4 vCPU,
two runs:

| | ns/pixel | % of cc -O3 |
|---|---|---|
| cc -O3 (LLVM, fma) | 499-514 | 100 |
| cc -O3 (LLVM, no fma) | 491-538 | 96-102 |
| bespoke-x86 | 421-424 | 118-122 |
| Cranelift-x86 | 458-481 | 107-109 |

Update, same day: those bespoke numbers used a bare `vminss` for FMin, which differs from
Cranelift (and the arm64 backend) on NaN and ±0 inputs. With Cranelift's exact sequence
(compare, then min / or / add) bespoke-x86 measures 474-478 ns/pixel, 106-112% of cc and level
with Cranelift on this min-heavy kernel. The vector work (packed loads/stores, lane-wise
add/sub/mul/div, matrix × vector) doesn't show here, since this kernel is a scalar chain; a
matrix-times-vector vertex shader shrank from 305 to 108 bytes.

Speed round, 2026-11-05 (after the above): helper-call arguments go straight to their ABI
registers as a parallel move; float constants with one definition become RIP-relative
constant-pool operands (`XBinC`/`VBinC`), so abs/neg masks and broadcast constants no longer
cost `mov`+`vmovd`; FMin/FMax work in the destination register; and neg/abs/sqrt/floor/ceil/
trunc and dot (lane-ordered sum, no FMA) have packed forms. One step of the bench kernel is now
10 instructions on the common path against Cranelift's 12, and the function shrank from 4669
to 4529 bytes (+64 bytes of pool). The bench can't resolve that on the shared cloud VM:
run-to-run spread is about ±10% (bespoke 398-485 ns/pixel, Cranelift 394-485 over five runs).
Note the bench loads shaders through the debug `atrium-spv-compile` binary, so rebuild it
after any backend change or the numbers come from stale code.

Originally: bespoke-x86 clears the 90% bar and is 8-12% faster than Cranelift-x86. Unlike on arm64, cc
is not a true ceiling here: without `-ffast-math`, `fminf` stays a libm call on x86, so the
C reference pays a call per step that the shader backends lower to `vminss` sequences.
Outputs agree across backends. Compile time (debug `atrium-spv-compile`, wall clock per
shader): bespoke 25 ms, Cranelift 78 ms, cc 41 ms. The R9700 box's Xeon run is still to do.

