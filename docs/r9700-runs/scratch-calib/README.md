# Scratch calibration + spilled bench re-run, R9700 under Ubuntu (2026-11-16)

Box: R9700 (GFX1201), Ubuntu kernel 7.0.0-38, Mesa (RADV/ACO) 26.0.8-1ubuntu0.3.
Atrium at main 13d077d6 (scratch calibration cases, bench scratch knobs).
GPU idle (runtime-suspended) before and after.

| File | What |
|---|---|
| `oracle-report.txt` | the whole `bundle-jobs` report at 13d077d6 (35 cases) |
| `scratch_layout.od` | `od -Ad -tx4 -v scratch_layout/scratch.out` in full (the BO is 16 KiB, not 32) |
| `scratch_isolation_and_tierb.txt` | isolation cases: nonzero counts, first 20 nonzero, value and per-wave histograms; tierb_raster_math's differing words |
| `bench-{a,b,c}.out` | bench-script at 13d077d6, three scratch variants (below), RADV compile section only (no `compile-time` binary) |
| `bench-summary.txt` | spilled rows of the three runs plus dmesg fault/timeout/reset counts |
| `dmesg-bench-{a,b,c}.txt` | amdgpu kernel messages over each bench run (b and c: none) |

Op SPIR-V for the desktop shaders: the four files from the v7 script, as in bench-v7.

## Oracle (bundle-jobs at 13d077d6)

39 MATCH, 4 MISMATCH. No amdgpu fault, timeout or reset over the run.

- **scratch_layout: MISMATCH.** One wave32 stored lane t, dword k = `t + (k << 24)`.
  On silicon the values land in the expected order, `(k * 32 + t) * 4`, but
  **4096 bytes into the ring**, not at slot 0: words 1024..1535 hold
  `0x00000000..0x0f00001f` in order (511 nonzero words; word 1024 is lane 0
  dword 0 = 0). Words 0..1023 and 1536..4095 are zero.
- **scratch_isolation_w64: MATCH** (0 nonzero of 262144 words).
- **scratch_isolation_w2048: MISMATCH.** 227440 of 262144 words nonzero.
  Per 32-word group (one wave): 7096 groups all 32 nonzero, 23 groups 16
  nonzero, 1073 clean. First 20 nonzero: words 0..19, all 0x00000060. Most
  common values 0x60 (50096), 0x20 (9568), 0x7e0 (5824), 0x3e0 (4096), 0x40,
  0x120, 0x2c0, 0x220, 0xe0, 0x1e0, 0xfe0. Waves do not keep their own
  slots when TMPRING WAVES = 2048.
- **tierb_raster_math and spill_tierb_raster_math: MISMATCH (new since v19).**
  b0_0 matches; b0_1 differs in 30 words, identical in both arms (same
  sha256 71b1f944…), so not a spill effect. Every difference is the
  float sign bit: silicon negative, expectation positive (e.g. word 60
  0xc0e23a1e vs 0x40e23a1e), all at word index ≡ 12 (mod 16), i.e. field 12
  of a 16-word record. List in `scratch_isolation_and_tierb.txt`.
- Every other case matches, as in v19.

## Spilled benches (GPU timestamps, median of 20, µs)

Variants, set when generating the script:
(a) none (slot 1x, WAVES 2048);
(b) `ATRIUM_BENCH_SCRATCH_X=2` alone tripped "scratch ring of 88 dwords per
lane is too large", so (b) is `ATRIUM_BENCH_SCRATCH_X=2 ATRIUM_BENCH_SCRATCH_WAVES=512`;
(c) `ATRIUM_BENCH_SCRATCH_WAVES=512`.

| Bench | (a) ours | (b) ours | (c) ours | RADV | (a) output | (b) output | (c) output |
|---|---|---|---|---|---|---|---|
| raster @96 | 2131.3 | 3929.0 | 3845.5 | 453-656 | **DIFFER 6684152 B** | DIFFER 31 B | DIFFER 31 B |
| raster @64 | 5238.0 | 7489.8 | 7455.1 | 422-656 | **DIFFER 6666783 B** | DIFFER 31 B | DIFFER 31 B |
| glyph_run 256K @16 | 212.5 (2.03 s wall: resets) | 297.6 | 292.6 | 178.5-179.4 | **counter 394448 vs 394142**, instances differ at byte 14691 | counter 394142 = 394142, instances differ at byte 68 | as (b) |
| path 1M @16 | 209.6 | 191.1 | 194.1 | 161.6-164.4 | **instances differ at byte 7** | instances same | instances same |

With WAVES = 512 (b or c), every spilled bench gives what its unspilled run
gives: unspilled raster 1080p is DIFFER 31 B in all three runs, unspilled
glyph_run 256K "differ at byte 68 (sorted records)" with the same counter,
unspilled path 1M "same". The slot multiplier (b vs c) makes no difference.

dmesg: (a) 30 fault lines (+253 suppressed), 9 ring timeouts
(comp_1.0.1 x5, comp_1.1.0 x4), 9 "reset succeeded", all from glyph_run @16
(TCP, VA 0x22120b000-0x22120e000, past the instances buffer as in bench-v7);
(b) and (c) none.

So the spilled-bench failures follow the scratch ring's WAVES = 2048, the
same setting under which scratch_isolation_w2048 shows waves overwriting
each other's slots; with 512 the spill code is correct on silicon.
