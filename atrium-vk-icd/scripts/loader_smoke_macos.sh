#!/bin/sh
# atrium-vk-icd loader smoke test (macOS host)
#
# Drives atrium-vk-icd through the Khronos Vulkan loader on the
# macOS host, the way a real Vulkan app would.  Validates the
# entire ICD ABI surface (negotiate / GetInstanceProcAddr / dispatch)
# end-to-end, including the aqueduct-gpu-host handshake.
#
# Three rungs:
#
#   1. ABI rung    -- no daemon.  Loader loads our dylib, calls
#                     vkCreateInstance + vkEnumeratePhysicalDevices.
#                     We expect 0 devices and the loader's synthesized
#                     VK_ERROR_INITIALIZATION_FAILED.  Confirms the
#                     ICD entry-point surface is wired correctly.
#
#   2. Daemon rung -- with aqueduct-gpu-host (--backend software).
#                     vkEnumeratePhysicalDevices returns 1 device
#                     reported as "atrium-vk-icd (software:0)".
#                     Confirms the wire-protocol handshake works
#                     through the loader.
#
#   3. vulkaninfo  -- full vulkaninfo readout against the daemon
#                     run.  Exercises GetPhysicalDeviceFeatures /
#                     Properties / FormatProperties / QueueFamily
#                     Properties through to completion.
#
#   4. Tier-2 rung -- daemon with --tier2 enabled + atrium-spv-
#                     compile reachable.  Runs the loader_smoke
#                     example which uploads a trivial compute
#                     SPIR-V via vkCreateShaderModule.  The daemon's
#                     Tier2Registry routes it through atrium-spv-
#                     compile and we verify an `.afblob` + `.pcmap`
#                     land in the cache directory.
#
#   5. tier2 backend -- same example, daemon with `--backend tier2`.
#                     Confirms the dispatch-ready backend selection
#                     works (compile + cache + dispatch-side state
#                     all happy).
#
#   6. compute round-trip -- the loader_compute_roundtrip example,
#                     daemon with `--backend tier2 --tier2`.  Full
#                     end-to-end: SPIR-V compute writes 42 to an
#                     SSBO, dispatch runs on the daemon's tier-2
#                     runtime, vkInvalidateMappedMemoryRanges pulls
#                     the result back through OP_GPU_BUFFER_READ,
#                     the client asserts ssbo[0] == 42.  The
#                     complete chain proven from a real Vulkan app
#                     with a hand-built rspirv module.
#
#   7. slang round-trip -- same example, but the SPIR-V comes from
#                     slangc compiling write_42.slang instead of
#                     being hand-built via rspirv.  Atrium's
#                     canonical shader compiler.  Proves the SPV
#                     path works for real third-party SPIR-V
#                     producers, not just our synthetic tests.
#
#   8+. Extra slang shaders against the same daemon -- each
#                     rung compiles a different shader and runs
#                     it with a tailored (seed, expect) pair.
#                     Driven by the EXTRA_SHADERS list below.
#                     Current entries:
#                       rmw         -- data[0] = data[0] + 1
#                                      (OpLoad + OpIAdd + OpStore)
#                       atomic_add  -- InterlockedAdd(data[0], 1)
#                                      (OpAtomicIAdd; bespoke
#                                      backend lowers to ARMv8.1
#                                      LSE ldaddal)
#                       bit_chain   -- ((x<<4)^0xFF)|0x100
#                                      (OpShiftLeftLogical +
#                                      OpBitwiseXor + OpBitwiseOr
#                                      chained; tests regalloc
#                                      across intermediate values)
#                       rotate      -- (x>>4) | (x<<28)
#                                      (rotate-right-by-4; tests
#                                      OpShiftRightLogical which
#                                      bit_chain doesn't reach,
#                                      paired with the left shift
#                                      + OpBitwiseOr)
#                       loop_mul    -- for(i in 0..5) v *= 2
#                                      (OpLoopMerge + OpPhi for
#                                      the loop-carried `v`;
#                                      depends on the daemon
#                                      running spirv-opt --ssa-
#                                      rewrite on slangc's
#                                      OpVariable Function output
#                                      first, Arc 144)
#                       branch      -- if (x>100) v=x-100; else v=0
#                                      (OpPhi at selection-merge,
#                                      distinct from loop_mul's
#                                      OpPhi-at-loop-header path)
#                       sum_loop    -- s=0; for(i=1..=n) s+=i
#                                      (two loop-carried function-
#                                      locals, both promoted by
#                                      spirv-opt to OpPhi)
#                       max_const   -- max(x, 100)
#                                      (GLSL.std.450 UMax;
#                                      first loader-mediated
#                                      ext-inst rung)
#                       float_sqrt  -- uint(sqrt(float(x)))
#                                      (int <-> float cast +
#                                      GLSL.std.450 Sqrt; first
#                                      rung that exercises the
#                                      bespoke backend's FP
#                                      register class through
#                                      the loader)
#                       ternary     -- (x & 1) ? 1 : 0
#                                      (Slang lowers C ternary
#                                      to branch + OpVariable
#                                      Function -- same merge
#                                      shape as branch.slang
#                                      but a different idiom
#                                      worth exercising for
#                                      coverage)
#                       bit_count   -- countbits(x)
#                                      (OpBitCount core opcode,
#                                      distinct from the
#                                      GLSL.std.450 ext-inst
#                                      family that max_const +
#                                      float_sqrt cover)
#                       subgroup_sum -- WaveActiveSum(x) + 1
#                                      (OpGroupNonUniformIAdd
#                                      at Atrium's subgroupSize
#                                      = 1; lowers trivially
#                                      since the "subgroup" is
#                                      one lane.  Distinct from
#                                      atomic_add's OpAtomicIAdd
#                                      despite the surface
#                                      similarity)
#
# Also tried: groupshared_var.slang -- Slang's optimizer folds
# the `groupshared` cache away when used by a single invocation
# (the [numthreads(1,1,1)] case), so the shader is observably
# equivalent to a plain SSBO RMW.  Real workgroup-storage
# coverage needs a multi-invocation dispatch -- separate arc.
#
# EXTRA_SHADERS entries optionally carry two extra fields for
# multi-slot / multi-workgroup tests:
#   "name:seed:expect:buf_u32s:dispatch_x"
# `per_thread` uses 0:56:8:8 -- seed every slot with 0, dispatch
# 8 workgroups, expect the wrapping sum of all 8 slots to be 56
# (0+2+4+6+8+10+12+14 for the shader `data[tid.x] = tid.x * 2`).
#
# Pre-reqs (one-time):
#   * brew install vulkan-headers vulkan-loader vulkan-tools
#   * cargo build -p atrium-vk-icd          (produces .dylib + examples)
#   * cargo build -p atrium-vk-icd --example loader_smoke
#   * cargo build -p atrium-vk-icd --example loader_compute_roundtrip
#   * cargo build -p aqueduct-gpu-host      (produces daemon)
#   * cargo build -p atrium-spv-compile     (produces compile binary)
#
# Usage:
#   ./scripts/loader_smoke_macos.sh

set -eu
# pipefail so `if ! cmd | tail; then ... fi` sees `cmd`'s exit
# code and not `tail`'s.  Without this a failing roundtrip
# example whose tail-truncated output still flushes cleanly
# would slip past as success (Arc 140 had exactly this bug:
# Rung 11's slang loop shader failed validation in the daemon,
# the example exited 1, but `tail -2` exited 0 and the script
# reported the whole ladder OK).
set -o pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DYLIB="$REPO_ROOT/atrium-vk-icd/target/debug/libatrium_vk_icd.dylib"
EXAMPLE="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_smoke"
ROUNDTRIP="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_compute_roundtrip"
GRAPHICS_RT="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_roundtrip"
GRAPHICS_INTERP="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_interp"
GRAPHICS_PUSHC="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_pushc"
GRAPHICS_INDEXED="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_indexed"
GRAPHICS_UNORM="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_unorm"
GRAPHICS_DEPTH="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_depth"
GRAPHICS_VS_PUSHC="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_vs_pushc"
GRAPHICS_MULTI_VBUF="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_multi_vbuf"
GRAPHICS_TEXTURE="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_texture"
GRAPHICS_MULTI_FS_IN="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_multi_fs_in"
GRAPHICS_UBO="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_ubo"
GRAPHICS_VIEWPORT="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_viewport"
GRAPHICS_SCISSOR="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_scissor"
GRAPHICS_DEPTH_RANGE="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_depth_range"
GRAPHICS_CULL="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_cull"
GRAPHICS_CULL_DYN="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_cull_dynamic"
GRAPHICS_DEPTH_DYN="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_depth_dynamic"
GRAPHICS_DEPTH_CMP="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_depth_cmp"
GRAPHICS_STRIP="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_strip"
GRAPHICS_DISCARD="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_discard"
GRAPHICS_DEPTH_BOUNDS="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_depth_bounds"
GRAPHICS_STENCIL="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_stencil"
GRAPHICS_STENCIL_DYN="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_stencil_dynamic"
GRAPHICS_DEPTH_BIAS="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_depth_bias"
GRAPHICS_RESTART="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_restart"
GRAPHICS_MIPMAP="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_mipmap"
GRAPHICS_LOD="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_lod"
GRAPHICS_MSAA="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_msaa"
GRAPHICS_HALF="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_half"
GRAPHICS_RGB10A2="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_rgb10a2"
GRAPHICS_DERIV="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_deriv"
GRAPHICS_INSTANCED="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_instanced"
GRAPHICS_INSTANCE_RATE="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_instance_rate"
GRAPHICS_FRONTFACE="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_frontface"
GRAPHICS_POINTS="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_points"
GRAPHICS_LINES="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_lines"
GRAPHICS_LINESTRIP="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_linestrip"
GRAPHICS_TRIFAN="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_trifan"
GRAPHICS_BGRA="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_bgra"
GRAPHICS_SRGB="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_srgb"
GRAPHICS_PRIMID="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_primid"
GRAPHICS_R8="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_r8"
GRAPHICS_RG8="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_rg8"
GRAPHICS_FRAGDEPTH="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_fragdepth"
GRAPHICS_DAMAGE="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_damage"
GRAPHICS_MULTIDRAW="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_multidraw"
GRAPHICS_ARRAY="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_array"
GRAPHICS_CUBE="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_cube"
GRAPHICS_SHADOW="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_shadow"
GRAPHICS_SHADOW_CMP="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_shadow_cmp"
GRAPHICS_PCF="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_pcf"
GRAPHICS_MRT="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_mrt"
GRAPHICS_MRT_BLEND="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_mrt_blend"
GRAPHICS_CLEAR="$REPO_ROOT/atrium-vk-icd/target/debug/examples/loader_graphics_clear"
DAEMON="$REPO_ROOT/aqueduct-gpu-host/target/debug/aqueduct-gpu-host"
COMPILE="$REPO_ROOT/atrium-spv-compile/target/debug/atrium-spv-compile"
SLANGC="$REPO_ROOT/external/slang-bin/bin/slangc"
# spirv-opt is part of the Khronos vulkan-tools install (brew install vulkan-tools
# on macOS, pkg install spirv-tools on FreeBSD).  When present, the daemon runs
# `--ssa-rewrite --eliminate-dead-code-aggressive` on every shader upload so
# slangc's OpVariable Function lands as proper SSA before the atrium-spv-
# frontend sees it.  When absent, slang shaders with loops / branches that
# carry state fail compile (the parked loop_mul rung).
SPIRV_OPT="${SPIRV_OPT:-/opt/homebrew/bin/spirv-opt}"
SHADER_DIR="$REPO_ROOT/atrium-vk-icd/examples/shaders"
SLANG_SRC="$SHADER_DIR/write_42.slang"
SLANG_SPV="$SHADER_DIR/write_42.comp.spv"
# Extra slang shaders (Arc 140+).  Each gets a smoke rung
# under the shared Rung-7 daemon: shader-name, seed, expect.
# All exercise the SSBO ssbo[0] path the existing
# loader_compute_roundtrip example is built around, but each
# stresses a distinct SPIR-V opcode family in the frontend +
# bespoke backend.
EXTRA_SHADERS="
rmw:100:101
atomic_add:200:201
bit_chain:1:495
rotate:0x12345678:0x81234567
loop_mul:1:32
branch:200:100
sum_loop:5:15
max_const:50:100
float_sqrt:100:10
ternary:7:1
bit_count:0x12345678:13
subgroup_sum:42:43
per_thread:0:56:8:8
per_thread_triangle:0:84:8:8
groupshared_xor:0:24:8:1
push_scale:6:42:1:1:7
multi_binding:10:42:1:1::32
spec_scale:6:42:1:1:::7
storage_image_write:0:42:1:1::::1
"
# loop_mul un-parked Arc 144: spirv-opt --ssa-rewrite (run by
# the daemon when --spirv-opt-binary is set) promotes Slang's
# OpVariable Function + Op{Load,Store} into OpPhi-form SSA
# before atrium-spv-compile sees it.  The atrium-spv-frontend
# already handles OpPhi (the differential_compute suite covers
# it), so the loop is fine end-to-end as long as spirv-opt is
# in the pipeline.
SOCKET="/tmp/atrium-vk-icd-loader-smoke.sock"
CACHE_ROOT="/tmp/atrium-vk-icd-loader-cache"
MANIFEST="$(mktemp -t atrium_icd.XXXXXX).json"

if [ ! -f "$DYLIB" ]; then
    echo "missing $DYLIB -- cargo build -p atrium-vk-icd first" >&2
    exit 1
fi
if [ ! -x "$DAEMON" ]; then
    echo "missing $DAEMON -- cargo build -p aqueduct-gpu-host first" >&2
    exit 1
fi
if ! command -v vulkaninfo >/dev/null 2>&1; then
    echo "missing vulkaninfo -- brew install vulkan-tools" >&2
    exit 1
fi
if [ ! -f "/opt/homebrew/lib/libvulkan.dylib" ]; then
    echo "missing /opt/homebrew/lib/libvulkan.dylib -- brew install vulkan-loader" >&2
    exit 1
fi

cat > "$MANIFEST" <<EOF
{
    "_comment": "atrium-vk-icd loader smoke-test manifest (generated by scripts/loader_smoke_macos.sh).",
    "file_format_version": "1.0.1",
    "ICD": {
        "library_path": "$DYLIB",
        "api_version": "1.3.0"
    }
}
EOF
echo "==> manifest: $MANIFEST"

cleanup() {
    if [ -n "${DAEMON_PID:-}" ]; then
        # SIGTERM first; if the daemon is wedged in a blocking
        # syscall that doesn't honour it, follow up with SIGKILL
        # after 250 ms.  Without this fallback an unresponsive
        # daemon would hang the trap forever.
        kill "$DAEMON_PID" 2>/dev/null || true
        ( sleep 0.25 && kill -KILL "$DAEMON_PID" 2>/dev/null ) &
        # Bounded wait: bash `wait` blocks until the PID exits,
        # but the SIGKILL fallback above guarantees that happens
        # within ~250 ms.
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -f "$SOCKET" "$MANIFEST"
}
trap cleanup EXIT INT TERM

# Same-shape kill used between rungs: SIGTERM, SIGKILL fallback,
# bounded wait on the specific PID (not all children).
kill_daemon() {
    local pid="$1"
    [ -z "$pid" ] && return 0
    kill "$pid" 2>/dev/null || true
    ( sleep 0.25 && kill -KILL "$pid" 2>/dev/null ) &
    wait "$pid" 2>/dev/null || true
}

# Wipe the tier-2 cache exactly once, at script start.  Earlier
# revisions wiped it between every rung "for hermetic isolation",
# which forced atrium-spv-compile to re-compile each shader from
# scratch (~1 s per shader) on every tier-2 rung.  Total cost was
# ~5 s for an essentially no-op repeat compile.  Now: first rung
# to use a given shader does the compile, subsequent rungs hit
# the warm cache.  Lifetime: one script run.
rm -rf "$CACHE_ROOT"

# Wait for the daemon's listen socket to appear (or its process
# to die).  Polls every 25 ms up to ~2 s.  Replaces the original
# blanket `sleep 1` per rung, which added 5 s of fixed wait
# across the script.
wait_for_daemon() {
    local pid="$1"
    local sock="$2"
    local n=0
    while [ "$n" -lt 80 ]; do
        if [ -S "$sock" ]; then return 0; fi
        if ! kill -0 "$pid" 2>/dev/null; then
            return 1
        fi
        sleep 0.025
        n=$((n + 1))
    done
    return 1
}

# ── Rung 1: ABI smoke (no daemon) ────────────────────────────────
echo
echo "=== Rung 1: no daemon -- vulkaninfo must report 0 devices ==="
# The rung's pass condition is "vulkaninfo prints
# `vkEnumeratePhysicalDevices failed with
# ERROR_INITIALIZATION_FAILED` on stderr".  That message
# coming through is the PROOF the loader correctly rejected
# our dylib's 0-device handshake -- it is NOT a real error
# in the test pipeline.  We capture the output, check the
# proof line is present, and print a one-line summary
# instead of dumping the scary vulkaninfo stderr verbatim.
set +e
rung1_out=$(DYLD_LIBRARY_PATH=/opt/homebrew/lib \
    VK_DRIVER_FILES="$MANIFEST" \
    vulkaninfo --summary 2>&1)
rung1_rc=$?
set -e
if printf '%s\n' "$rung1_out" | grep -q "ERROR_INITIALIZATION_FAILED"; then
    echo "PASS: vulkaninfo got ERROR_INITIALIZATION_FAILED (expected with no daemon)"
else
    echo "FAIL: Rung 1 didn't see the expected ERROR_INITIALIZATION_FAILED" >&2
    echo "vulkaninfo exit=$rung1_rc output:" >&2
    printf '%s\n' "$rung1_out" >&2
    exit 1
fi

# ── Rung 2 + 3: with daemon ──────────────────────────────────────
rm -f "$SOCKET"
"$DAEMON" --socket "$SOCKET" --backend software >/tmp/aqueduct-loader-smoke.log 2>&1 &
DAEMON_PID=$!
if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
    echo "daemon failed to start; log:" >&2
    cat /tmp/aqueduct-loader-smoke.log >&2
    exit 1
fi
echo
echo "=== Rung 2: daemon up -- vulkaninfo must see 1 device ==="
rung2_out=$(DYLD_LIBRARY_PATH=/opt/homebrew/lib \
    VK_DRIVER_FILES="$MANIFEST" \
    ATRIUM_VK_ICD_SOCKET="$SOCKET" \
    vulkaninfo --summary 2>&1)
if printf '%s\n' "$rung2_out" | grep -q "atrium-vk-icd (software:"; then
    devline=$(printf '%s\n' "$rung2_out" | grep "deviceName" | head -1 | tr -s ' ')
    echo "PASS: vulkaninfo reports our device ($devline)"
else
    echo "FAIL: Rung 2 didn't find atrium-vk-icd in vulkaninfo output" >&2
    printf '%s\n' "$rung2_out" >&2
    exit 1
fi

echo
echo "=== Rung 3: full vulkaninfo readout exit code ==="
if DYLD_LIBRARY_PATH=/opt/homebrew/lib \
    VK_DRIVER_FILES="$MANIFEST" \
    ATRIUM_VK_ICD_SOCKET="$SOCKET" \
    vulkaninfo > /tmp/atrium-vulkaninfo.out 2>&1
then
    nlines=$(wc -l </tmp/atrium-vulkaninfo.out | tr -d ' ')
    echo "PASS: full vulkaninfo readout completed ($nlines lines in /tmp/atrium-vulkaninfo.out)"
else
    echo "FAIL: vulkaninfo full readout returned non-zero" >&2
    exit 1
fi

kill_daemon "$DAEMON_PID"
DAEMON_PID=""

# ── Rung 4: tier-2 (shader upload) ───────────────────────────────
if [ ! -x "$EXAMPLE" ] || [ ! -x "$COMPILE" ]; then
    echo
    echo "SKIP Rung 4: tier-2 (need 'cargo build -p atrium-vk-icd --example loader_smoke' + 'cargo build -p atrium-spv-compile')"
else
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend software \
        --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (tier-2 mode); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung 4: tier-2 daemon, vkCreateShaderModule upload ==="
    DYLD_LIBRARY_PATH=/opt/homebrew/lib \
      VK_DRIVER_FILES="$MANIFEST" \
      ATRIUM_VK_ICD_SOCKET="$SOCKET" \
      "$EXAMPLE" > /tmp/atrium-loader-smoke.out 2>&1
    rung4_rc=$?
    n_afblob=$(find "$CACHE_ROOT" -name '*.afblob' 2>/dev/null | wc -l | tr -d ' ')
    n_pcmap=$(find "$CACHE_ROOT"  -name '*.pcmap'  2>/dev/null | wc -l | tr -d ' ')
    if [ "$rung4_rc" -ne 0 ]; then
        echo "FAIL: loader_smoke returned $rung4_rc" >&2
        cat /tmp/atrium-loader-smoke.out >&2
        exit 1
    fi
    if [ "$n_afblob" != "1" ] || [ "$n_pcmap" != "1" ]; then
        echo "FAIL: tier-2 did not produce the expected cache artifacts \
              (.afblob=$n_afblob, .pcmap=$n_pcmap)" >&2
        exit 1
    fi
    echo "PASS: shader upload landed an .afblob + .pcmap in the cache"
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""

    # ── Rung 5: --backend tier2 (Tier2Backend dispatch path) ──
    # Same observable surface as Rung 4 from the example's POV
    # (we don't submit a draw yet), but exercises the
    # Tier2Backend's startup path: registry shared between
    # listener and backend, with the dispatch table swapped from
    # SoftwareBackend (which would reject tier2_id-bearing draws)
    # to Tier2Backend (which executes them through atrium-spv-
    # runtime).
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 \
        --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (--backend tier2); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung 5: --backend tier2, dispatch-ready ==="
    DYLD_LIBRARY_PATH=/opt/homebrew/lib \
      VK_DRIVER_FILES="$MANIFEST" \
      ATRIUM_VK_ICD_SOCKET="$SOCKET" \
      "$EXAMPLE" > /tmp/atrium-loader-smoke.out 2>&1
    rung5_rc=$?
    if [ "$rung5_rc" -ne 0 ]; then
        echo "FAIL: loader_smoke (tier2 backend) returned $rung5_rc" >&2
        cat /tmp/atrium-loader-smoke.out >&2
        exit 1
    fi
    grep -E "tier-2 backend selected" /tmp/aqueduct-loader-smoke.log >/dev/null \
        || { echo "FAIL: --backend tier2 did not log selection" >&2; exit 1; }
    echo "PASS: --backend tier2 dispatch path active"
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""

    # ── Rung 6: full compute round-trip (dispatch + readback) ──
    # The complete end-to-end: a real Vulkan app builds a compute
    # SPIR-V that writes 42 to an SSBO, dispatches it, and reads
    # the result back through vkInvalidateMappedMemoryRanges.
    # Exercises every link in the chain:
    #   loader -> ICD -> daemon -> Tier2Registry -> atrium-spv-
    #   compile -> Tier2Backend dispatch -> OP_GPU_BUFFER_READ.
    if [ -x "$ROUNDTRIP" ]; then
        rm -f "$SOCKET"
        "$DAEMON" --socket "$SOCKET" \
            --backend tier2 --tier2 \
            --cache-root "$CACHE_ROOT" \
            --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
            > /tmp/aqueduct-loader-smoke.log 2>&1 &
        DAEMON_PID=$!
        if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
            echo "daemon failed to start (Rung 6); log:" >&2
            cat /tmp/aqueduct-loader-smoke.log >&2
            exit 1
        fi
        echo
        echo "=== Rung 6: compute round-trip (SSBO write + readback) ==="
        if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
            VK_DRIVER_FILES="$MANIFEST" \
            ATRIUM_VK_ICD_SOCKET="$SOCKET" \
            "$ROUNDTRIP" 2>&1 | tail -3; then
            echo "FAIL: compute round-trip did not return 0" >&2
            exit 1
        fi
    else
        echo
        echo "SKIP Rung 6: need 'cargo build -p atrium-vk-icd --example loader_compute_roundtrip'"
    fi
    # Reap the Rung 6 daemon before Rung 7 reassigns DAEMON_PID.
    # Without this it would leak as an orphan (init reparents it)
    # and the cleanup trap, which only kills the final DAEMON_PID,
    # wouldn't catch it.
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
fi

if [ -x "$ROUNDTRIP" ] && [ -x "$SLANGC" ] && [ -f "$SLANG_SRC" ]; then
    # ── Rung 7: same round-trip with slang-built SPIR-V ──────
    # The "real third-party shader compiler" rung.  Compiles
    # write_42.slang via slangc (per docs/LANGUAGE-POLICY.md:
    # no `-profile` flag, which would force the legacy
    # BufferBlock + Uniform shape) and feeds the resulting
    # .spv through the same loader_compute_roundtrip example
    # via ATRIUM_VK_SMOKE_SHADER=slang.  Proves the SPV path
    # works for canonical slangc output, not just our hand-
    # built rspirv modules.
    "$SLANGC" "$SLANG_SRC" -target spirv -entry main -stage compute \
        -o "$SLANG_SPV" 2>/tmp/slangc.log \
        || { echo "FAIL: slangc compile failed"; cat /tmp/slangc.log; exit 1; }
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (Rung 7); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung 7: slang-built SPIR-V round-trip ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        ATRIUM_VK_SMOKE_SHADER=slang \
        "$ROUNDTRIP" 2>&1 | tail -4; then
        echo "FAIL: slang round-trip did not return 0" >&2
        exit 1
    fi

    # ── Rungs 8+: extra slang shaders against the same Rung-7
    # daemon.  Safe now: the prior cross-session BufferRecord
    # leak was fixed by Session::cleanup walking ResourceTable
    # and calling backend.buffer_destroyed on disconnect (Arc
    # 142).  EXTRA_SHADERS entry: "name:seed:expect[:buf_u32s:dispatch_x]".
    # The last two are optional and default to "1:1" -- single-
    # slot SSBO, single-workgroup dispatch.  Per-thread rungs
    # set them both to N to exercise dispatch(N,1,1) writes into
    # an N-slot buffer.
    rung_n=8
    for entry in $EXTRA_SHADERS; do
        name=$(echo "$entry" | cut -d: -f1)
        seed=$(echo "$entry" | cut -d: -f2)
        expect=$(echo "$entry" | cut -d: -f3)
        buf_u32s=$(echo "$entry" | cut -d: -f4)
        dispatch_x=$(echo "$entry" | cut -d: -f5)
        # Optional 6th field: push-constant u32 value.  When
        # present, the example creates a pipeline layout
        # with a 4-byte push range and calls vkCmdPushConstants
        # before dispatch.  Empty = no push.
        push_u32=$(echo "$entry" | cut -d: -f6)
        # Optional 7th field: second SSBO seed value.  When
        # present, the example creates a second VkBuffer at
        # binding 1 (same BUFFER_U32S slots) seeded with this
        # u32.  Empty = single-binding (every existing rung).
        second_seed=$(echo "$entry" | cut -d: -f7)
        # Optional 8th field: specialization-constant u32 value.
        # When present, the example passes a VkSpecializationInfo
        # with (constantID=0, offset=0, size=4) carrying this
        # u32.  Slang's [[vk::constant_id(0)]] const baked in.
        spec_u32=$(echo "$entry" | cut -d: -f8)
        # Optional 9th field: storage-image flag.  When set
        # to "1", the example creates a 1x1 R32_UINT VkImage
        # at binding 1 (STORAGE_IMAGE descriptor) instead of
        # a second SSBO.  Layout transition UNDEFINED ->
        # GENERAL happens at cmdbuf start.  Exercises the
        # Tier2Backend storage-image wire path; the image
        # write itself is not read back.
        use_image=$(echo "$entry" | cut -d: -f9)
        [ -z "$buf_u32s" ]   && buf_u32s=1
        [ -z "$dispatch_x" ] && dispatch_x=1
        src="$SHADER_DIR/$name.slang"
        spv="$SHADER_DIR/$name.comp.spv"
        [ -f "$src" ] || { echo "missing $src" >&2; exit 1; }
        "$SLANGC" "$src" -target spirv -entry main -stage compute \
            -o "$spv" 2>/tmp/slangc.log \
            || { echo "FAIL: slangc $name"; cat /tmp/slangc.log; exit 1; }
        echo
        echo "=== Rung $rung_n: $name.slang (seed=$seed, expect=$expect, slots=$buf_u32s, gx=$dispatch_x) ==="
        if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
            VK_DRIVER_FILES="$MANIFEST" \
            ATRIUM_VK_ICD_SOCKET="$SOCKET" \
            ATRIUM_VK_SMOKE_SHADER=slang \
            ATRIUM_VK_SMOKE_SHADER_PATH="$spv" \
            ATRIUM_VK_SMOKE_SEED="$seed" \
            ATRIUM_VK_SMOKE_EXPECT="$expect" \
            ATRIUM_VK_SMOKE_BUFFER_U32S="$buf_u32s" \
            ATRIUM_VK_SMOKE_DISPATCH_X="$dispatch_x" \
            ATRIUM_VK_SMOKE_PUSH_U32="${push_u32:-}" \
            ATRIUM_VK_SMOKE_SECOND_SEED="${second_seed:-}" \
            ATRIUM_VK_SMOKE_SPEC_U32="${spec_u32:-}" \
            ATRIUM_VK_SMOKE_USE_IMAGE="${use_image:-}" \
            "$ROUNDTRIP" 2>&1 | tail -2; then
            echo "FAIL: $name round-trip" >&2
            exit 1
        fi
        rung_n=$((rung_n + 1))
    done

    # Reap the Rung-7-onwards daemon.
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
fi

# ── Rung N+1: graphics round-trip (vertex + fragment + readback) ──
# loader_graphics_roundtrip is the graphics-path companion to
# loader_compute_roundtrip: a real Vulkan app builds a VS + FS
# inline, renders a triangle into a DEVICE_LOCAL color-attachment
# image, then vkCmdCopyImageToBuffer + vkInvalidateMappedMemoryRanges
# pull the rasterized pixels back into the client's mapped pointer.
# Exercises every link in the chain: loader -> ICD -> daemon ->
# Tier2 graphics pipeline -> CopyImgToBuf wire -> OP_GPU_BUFFER_READ.
# Asserts the triangle interior pixel (3,3) matches the FS colour
# (255, 51, 51, 255).
if [ -x "$GRAPHICS_RT" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (graphics round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung G: graphics round-trip (triangle render + copy + readback) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_RT" 2>&1 | tail -3; then
        echo "FAIL: graphics round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung G: need 'cargo build -p atrium-vk-icd --example loader_graphics_roundtrip'"
fi

# ── Rung H: graphics round-trip with per-vertex colour interp ──
# loader_graphics_interp extends Rung G with a per-vertex
# vec3 colour attribute that the VS passes through as a
# Location=0 varying to the FS.  Verifies the rasterizer's
# barycentric interpolation reaches the client end-to-end
# through the same wire path (loader -> ICD -> daemon ->
# fill_image_triangle -> CopyImgToBuf -> OP_GPU_BUFFER_READ).
# Asserts pixel(4,4) has all three RGB channels non-zero
# AND none saturated -- the proof that all three vertex
# colours contributed.
if [ -x "$GRAPHICS_INTERP" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (interp round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung H: per-vertex colour interpolation through the rasterizer ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_INTERP" 2>&1 | tail -2; then
        echo "FAIL: per-vertex interpolation round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung H: need 'cargo build -p atrium-vk-icd --example loader_graphics_interp'"
fi

# ── Rung I: graphics + FS push constants ─────────────────
# loader_graphics_pushc: same render shape as Rung G but
# the FS reads its output colour from a 16-byte
# PushConstantBlock instead of an inline OpConstantComposite.
# Verifies vkCmdPushConstants -> FrameOp::PushConstants ->
# daemon `pc_ptr` -> FS OpLoad PushConstant survives the
# full loader-mediated path.  Daemon-side proven by the
# tier2_render_pixels test; this rung closes the loader
# side.  Pushes (0.0, 0.5, 0.75, 1.0) and asserts
# pixel(3,3) == (0, 128, 191, 255).
if [ -x "$GRAPHICS_PUSHC" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (FS pushc round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung I: FS push constants drive triangle colour ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_PUSHC" 2>&1 | tail -2; then
        echo "FAIL: FS push-constant round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung I: need 'cargo build -p atrium-vk-icd --example loader_graphics_pushc'"
fi

# ── Rung J: graphics + vkCmdDrawIndexed ──────────────────
# loader_graphics_indexed: 4-vert vertex buffer + 6-index
# index buffer driving two triangles (a quad) via
# vkCmdBindIndexBuffer + vkCmdDrawIndexed.  Verifies the
# whole indexed-draw wire works end-to-end (the daemon-side
# dispatch_draw_indexed had no pixel-readback test before
# this).  Pixel(3,3) interior of first indexed triangle,
# pixel(5,5) interior of second -- both must hit the FS
# colour or one of the two indexed primitives didn't
# dispatch.
if [ -x "$GRAPHICS_INDEXED" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (indexed round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung J: vkCmdDrawIndexed two-triangle quad ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_INDEXED" 2>&1 | tail -3; then
        echo "FAIL: indexed-draw round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung J: need 'cargo build -p atrium-vk-icd --example loader_graphics_indexed'"
fi

# ── Rung K: VK_FORMAT_R8G8B8A8_UNORM vertex attribute ───
# loader_graphics_unorm: same triangle as Rung H but the
# per-vertex colour attribute is R8G8B8A8_UNORM (4 bytes
# per vertex in the buffer) instead of R32G32B32_SFLOAT
# (12 bytes).  The daemon's assemble_vertices must expand
# each u8 lane to f32 via `byte / 255.0` before handing
# the stream to the VS.  Asserts pixel(4,4) has all RGB
# channels non-zero -- same proof as Rung H, now through
# the UNORM decode path.
if [ -x "$GRAPHICS_UNORM" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (UNORM round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung K: R8G8B8A8_UNORM vertex colour attribute ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_UNORM" 2>&1 | tail -2; then
        echo "FAIL: UNORM round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung K: need 'cargo build -p atrium-vk-icd --example loader_graphics_unorm'"
fi

# ── Rung L: depth test through the loader ────────────────
# loader_graphics_depth: two triangles at z=0.1 (green) and
# z=0.5 (red) over the same screen-space area, with the
# pipeline's depth-stencil state enabled (test + write,
# compare op = LESS).  Front triangle drawn first; back
# triangle drawn second.  With depth test working the back
# triangle's fragments are rejected at overlapping pixels
# -- pixel(3,3) ends up green, not red.  Drawing order is
# deliberate: front-first proves the depth test is actually
# rejecting fragments (not just happening to draw the right
# colour last).
if [ -x "$GRAPHICS_DEPTH" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (depth round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung L: depth-test rejects the back triangle ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DEPTH" 2>&1 | tail -2; then
        echo "FAIL: depth round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung L: need 'cargo build -p atrium-vk-icd --example loader_graphics_depth'"
fi

# ── Rung M: VS push constants shift triangle position ───
# loader_graphics_vs_pushc: same triangle as Rung G but
# the VS reads a vec4 offset from a push constant and adds
# (offset.xy, 0) to every NDC vertex.  Cmd buffer pushes
# (+0.5, 0, 0, 0) -- the triangle slides right by 2 pixels.
# Asserts pixel(3,3) (was inside the original triangle) is
# now CLEAR + pixel(5,3) (was outside) is the FS colour.
# Both halves must hold: if the push didn't reach the VS,
# pc_ptr would deref as zeros and the triangle wouldn't
# move; if the push bytes were wrong (e.g. byte-swapped),
# the triangle would shift to the wrong place.
if [ -x "$GRAPHICS_VS_PUSHC" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (VS pushc round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung M: VS push constants shift the triangle ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_VS_PUSHC" 2>&1 | tail -3; then
        echo "FAIL: VS push-constant round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung M: need 'cargo build -p atrium-vk-icd --example loader_graphics_vs_pushc'"
fi

# ── Rung N: multi-binding vertex buffers ─────────────────
# loader_graphics_multi_vbuf: same triangle as Rung H, but
# positions and colours live in SEPARATE VkBuffers bound to
# distinct binding slots (positions @ slot 0, colours @
# slot 1).  Verifies the daemon's per-binding
# `vertex_buffers` HashMap stores both buffers + the
# `assemble_vertices_by_index` cross-binding gather pulls
# the right attribute from the right buffer per vertex.
# Same expected pixel(4,4) as Rung H -- if the gather
# broke (e.g. both attrs from slot 0), pixel values would
# diverge.
if [ -x "$GRAPHICS_MULTI_VBUF" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (multi-vbuf round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung N: multi-binding vertex buffers ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_MULTI_VBUF" 2>&1 | tail -2; then
        echo "FAIL: multi-vbuf round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung N: need 'cargo build -p atrium-vk-icd --example loader_graphics_multi_vbuf'"
fi

# ── Rung O: texture sampling through the Khronos loader ──
# loader_graphics_texture: 2x2 RGBA8 texture (red/green/blue/
# white corners) uploaded via vkCmdCopyBufferToImage, sampled
# in the FS at per-vertex UVs interpolated across the
# triangle.  Asserts pixel(4,4) has all RGB channels non-zero
# AND alpha=255 -- proves the full chain works:
#   * vkCreateSampler -> SamplerDesc on daemon
#   * vkCmdCopyBufferToImage -> daemon CopyBufToImg handler
#     (runs pre-pass so texture is populated before draw)
#   * vkCmdBindDescriptorSets COMBINED_IMAGE_SAMPLER ->
#     PassState::bound_textures
#   * dispatch_draw builds uniforms-table with helper fn ptrs
#     + per-binding (TexDesc*, SamplerDesc*)
#   * FS's OpImageSampleImplicitLod loads through the table
#     and calls atrium_tex_sample_2d
#   * Rasterizer's barycentric UV interp + LINEAR sampler
#     filter blend reaches all four texels.
if [ -x "$GRAPHICS_TEXTURE" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (texture round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung O: texture sampling (2x2 RGBA8 + sampler2D FS) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_TEXTURE" 2>&1 | tail -2; then
        echo "FAIL: texture round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung O: need 'cargo build -p atrium-vk-icd --example loader_graphics_texture'"
fi

# ── Rung P: FS with TWO Location-decorated inputs ────────
# Catches a regression of the input-routing fix from commit
# 909b4ee on the FS side.  No prior rung had FS reading more
# than ONE varying; if locations 0 + 1 collide back at
# offset 0, the FS reads UV bytes as colour bytes and the
# output diverges wildly from expectations.  VS outputs
# colour (Loc 0) + UV (Loc 1); FS reads both, multiplies
# colour by (1.0 - uv.x).  pixel(2,2) = V0 corner where
# UV.x=0 -> bright red; pixel(5,2) = V1 corner where
# UV.x=1 -> modulator zeroes the green.
if [ -x "$GRAPHICS_MULTI_FS_IN" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (multi-fs-in round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung P: FS with 2 Location-decorated inputs ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_MULTI_FS_IN" 2>&1 | tail -3; then
        echo "FAIL: multi-fs-in round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung P: need 'cargo build -p atrium-vk-icd --example loader_graphics_multi_fs_in'"
fi

# ── Rung Q: uniform buffer (UBO) ─────────────────────────
# loader_graphics_ubo: FS reads `Block { vec4 color; }`
# from a UNIFORM_BUFFER descriptor at binding 0.  Daemon
# copies the buffer's 16 bytes into the uniforms scratch
# (which the FS's `StorageClass::Uniform` resolves to via
# `params[1]`) and the FS's `OpAccessChain` adds the Block
# member's offset.  Pushes the same `(0.0, 0.5, 0.75, 1.0)`
# colour as Rung I (push consts), so pixel(3,3) matches
# the same expected `(0, 128, 191, 255)` -- different
# data path, identical quantised result.
if [ -x "$GRAPHICS_UBO" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (UBO round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung Q: FS reads vec4 colour from a UBO ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_UBO" 2>&1 | tail -2; then
        echo "FAIL: UBO round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung Q: need 'cargo build -p atrium-vk-icd --example loader_graphics_ubo'"
fi

# ── Rung R: vkCmdSetViewport actually moves the triangle ─
# loader_graphics_viewport: 16x16 framebuffer with a viewport
# at (4,4,8,8).  The rasterizer must remap NDC into that sub-
# rect, so pixels outside [4..12) x [4..12) stay clear-black
# and the triangle interior lands at (8,7) (inside the
# viewport).  Pre-fix, dispatch_draw captured the SetViewport
# frame op but the rasterizer ignored it; this rung locks in
# the viewport-aware mapping.
if [ -x "$GRAPHICS_VIEWPORT" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (viewport round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung R: vkCmdSetViewport moves triangle into vp sub-rect ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_VIEWPORT" 2>&1 | tail -5; then
        echo "FAIL: viewport round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung R: need 'cargo build -p atrium-vk-icd --example loader_graphics_viewport'"
fi

# ── Rung S: vkCmdSetScissor actually clips the triangle ─
# loader_graphics_scissor: 16x16 framebuffer, fullscreen
# viewport, scissor at (8, 0, 8, 16) -- right half only.
# The triangle covers x in [4..12) so the left half (x<8)
# of the triangle's would-be-painted region must stay
# clear-black; the right half must be the FS colour.
# Pre-fix, SetScissor was on the "ops we don't yet act on"
# list and the triangle painted across the entire 16x16
# target.
if [ -x "$GRAPHICS_SCISSOR" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (scissor round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung S: vkCmdSetScissor clips triangle to right half ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_SCISSOR" 2>&1 | tail -5; then
        echo "FAIL: scissor round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung S: need 'cargo build -p atrium-vk-icd --example loader_graphics_scissor'"
fi

# ── Rung T: viewport min_depth/max_depth depth-range remap ─
# loader_graphics_depth_range: two overlapping triangles
# with viewport depth ranges swapped vs their NDC.z values.
# F draws first at ndc.z=0.1 with depth range [0.5, 1.0]
# (windowed 0.55); B draws second at ndc.z=0.5 with depth
# range [0.0, 0.5] (windowed 0.25).  With the remap, B wins
# the LESS test; pixel(3,3) ends up red.  Without the remap,
# F wins and pixel(3,3) is green.  Pre-fix, fill_image_
# triangle wrote raw NDC.z to the depth buffer and passed
# the same to fs_main; this rung verifies the windowed
# depth path lands.
if [ -x "$GRAPHICS_DEPTH_RANGE" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (depth_range round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung T: viewport depth-range remap flips depth-test winner ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DEPTH_RANGE" 2>&1 | tail -2; then
        echo "FAIL: depth_range round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung T: need 'cargo build -p atrium-vk-icd --example loader_graphics_depth_range'"
fi

# ── Rung U: pipeline-static cull_mode / front_face honoured ─
# loader_graphics_cull: pipeline declares cullMode=BACK +
# frontFace=COUNTER_CLOCKWISE.  Vertex order is CW in
# screen-space (Y-down NDC), so the triangle is back-facing
# under this policy.  The rasterizer must discard it before
# the per-pixel walk -- pixel(3,3) stays clear-black.
# Pre-fix, VkPipelineRasterizationStateCreateInfo wasn't
# parsed and the triangle painted unconditionally.
if [ -x "$GRAPHICS_CULL" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (cull round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung U: pipeline cull_mode=BACK discards CW triangle ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_CULL" 2>&1 | tail -2; then
        echo "FAIL: cull round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung U: need 'cargo build -p atrium-vk-icd --example loader_graphics_cull'"
fi

# ── Rung V: vkCmdSetCullMode / SetFrontFace dynamic override ─
# loader_graphics_cull_dynamic: pipeline is created with
# cullMode=NONE (no culling).  Mid-cmdbuf, the app calls
# vkCmdSetCullMode(BACK) + vkCmdSetFrontFace(CCW) -- the
# daemon must honour the dynamic override and discard the
# CW-wound triangle.  Pixel(3,3) stays clear-black.  Pre-fix,
# both entry points were `ext_state_stub_u32!` no-ops and
# the pipeline's NONE leaked through; triangle painted red.
if [ -x "$GRAPHICS_CULL_DYN" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (dynamic cull round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung V: vkCmdSetCullMode override discards CW triangle ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_CULL_DYN" 2>&1 | tail -2; then
        echo "FAIL: dynamic cull round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung V: need 'cargo build -p atrium-vk-icd --example loader_graphics_cull_dynamic'"
fi

# ── Rung W: vkCmdSetDepthTestEnable dynamic override ─────
# loader_graphics_depth_dynamic: pipeline turns the depth
# test ON; mid-cmdbuf the app calls
# vkCmdSetDepthTestEnable(VK_FALSE).  The daemon must skip
# the LESS test so the second-drawn back triangle (red,
# z=0.5) overpaints the first (green, z=0.1) at overlapping
# pixels.  Pixel(3,3) ends up red.  Pre-fix, the entry point
# was an `ext_state_stub_u32!` no-op and the pipeline's
# ENABLED static state rejected the back triangle -- pixel
# stayed green.
if [ -x "$GRAPHICS_DEPTH_DYN" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (dynamic depth round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung W: vkCmdSetDepthTestEnable(false) lets back triangle win ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DEPTH_DYN" 2>&1 | tail -2; then
        echo "FAIL: dynamic depth round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung W: need 'cargo build -p atrium-vk-icd --example loader_graphics_depth_dynamic'"
fi

# ── Rung X: vkCmdSetDepthCompareOp (+ pipeline-static op) ─
# loader_graphics_depth_cmp: same two-triangle scene as
# Rung L; pipeline declares depth test ON.  Mid-cmdbuf the
# app calls vkCmdSetDepthCompareOp(NEVER) -- every depth
# test must fail, so neither triangle produces colour.
# Pixel(3,3) stays clear-black.  Pre-fix, the rasterizer
# hardcoded the LESS comparison regardless of pipeline or
# dynamic state; pipeline-static `Tier2DepthState::compare_
# op` is now populated from the create info AND the
# dynamic override path is wired.
if [ -x "$GRAPHICS_DEPTH_CMP" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (depth_cmp round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung X: vkCmdSetDepthCompareOp(NEVER) rejects every fragment ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DEPTH_CMP" 2>&1 | tail -2; then
        echo "FAIL: depth_cmp round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung X: need 'cargo build -p atrium-vk-icd --example loader_graphics_depth_cmp'"
fi

# ── Rung Y: TRIANGLE_STRIP topology honoured ─────────────
# loader_graphics_strip: pipeline declares
# VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP; app submits 4
# fullscreen-NDC vertices.  Under strip rules these form
# two triangles covering the full quad; under the legacy
# TriangleList interpretation the daemon would see only
# one triangle from the first 3 verts and the 4th would
# be dropped.  All 4 corners + centre must paint red.
if [ -x "$GRAPHICS_STRIP" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (strip round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung Y: TRIANGLE_STRIP draws 4 verts as 2 triangles ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_STRIP" 2>&1 | tail -2; then
        echo "FAIL: strip round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung Y: need 'cargo build -p atrium-vk-icd --example loader_graphics_strip'"
fi

# ── Rung Z: vkCmdSetRasterizerDiscardEnable honoured ─────
# loader_graphics_discard: round-trip pipeline with discard
# OFF in the create info; mid-cmdbuf the app calls
# vkCmdSetRasterizerDiscardEnable(true) -- the daemon must
# short-circuit the dispatch so no fragments reach the
# framebuffer.  Pixel(3,3) stays clear-black.  Pre-fix,
# the entry point was an `ext_state_stub_u32!` no-op AND
# the pipeline's static `rasterizerDiscardEnable` wasn't
# read; both axes now wired.
if [ -x "$GRAPHICS_DISCARD" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (discard round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung Z: vkCmdSetRasterizerDiscardEnable(true) skips dispatch ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DISCARD" 2>&1 | tail -2; then
        echo "FAIL: discard round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung Z: need 'cargo build -p atrium-vk-icd --example loader_graphics_discard'"
fi

# ── Rung AA: depth bounds test honoured ──────────────────
# loader_graphics_depth_bounds: pipeline enables the depth
# bounds test with range [0.0, 0.5]; depth attachment is
# cleared to 1.0.  Per Vulkan spec the bounds test compares
# the EXISTING buffer value (1.0) against the range and
# discards every out-of-range fragment.  pixel(3,3) stays
# clear-black despite the LESS depth compare on its own
# would have let the front triangle paint.  Pre-fix, the
# depthBoundsTest* fields were not extracted from the
# create info and the rasterizer had no bounds-test gate.
if [ -x "$GRAPHICS_DEPTH_BOUNDS" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (depth_bounds round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung AA: depth bounds [0.0, 0.5] vs depth-buffer 1.0 rejects all ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DEPTH_BOUNDS" 2>&1 | tail -2; then
        echo "FAIL: depth_bounds round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung AA: need 'cargo build -p atrium-vk-icd --example loader_graphics_depth_bounds'"
fi

# ── Rung BB: pipeline-static stencil test + ops ─────────
# loader_graphics_stencil: two-pass stencil masking.
# Pass 1 paints a small red triangle with stencil REPLACE/
# ALWAYS reference=1.  Pass 2 paints a fullscreen blue
# triangle but stencil EQUAL/reference=1 KEEP gates colour
# output -- blue lands only where pass 1 covered.
# pixel(4,4) inside small triangle = blue.
# pixel(0,0) outside small triangle = clear-black.
# Pre-fix, neither the static stencil state nor the rasterizer
# stencil-test gate existed; the blue would have painted the
# entire framebuffer.
if [ -x "$GRAPHICS_STENCIL" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (stencil round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung BB: stencil masking (REPLACE then EQUAL) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_STENCIL" 2>&1 | tail -3; then
        echo "FAIL: stencil round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung BB: need 'cargo build -p atrium-vk-icd --example loader_graphics_stencil'"
fi

# ── Rung CC: dynamic stencil state (5 setters) ───────────
# loader_graphics_stencil_dynamic: same two-pass mask as
# Rung BB but pipelines declare stencilTestEnable=false +
# all-default face state.  Every effective stencil field
# is driven by vkCmdSetStencilTestEnable +
# vkCmdSetStencilOp(FRONT_AND_BACK, ...) +
# vkCmdSetStencilCompareMask/WriteMask/Reference.  Final
# pixels match Rung BB: pixel(3,3) blue inside mask,
# pixel(0,0) clear-black outside.  Pre-fix, all five entry
# points were stubs / no-op u32 takers; pipeline-disabled
# stencil leaked and pass-2 blue painted everything.
if [ -x "$GRAPHICS_STENCIL_DYN" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (dynamic stencil round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung CC: dynamic stencil setters drive the mask ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_STENCIL_DYN" 2>&1 | tail -3; then
        echo "FAIL: dynamic stencil round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung CC: need 'cargo build -p atrium-vk-icd --example loader_graphics_stencil_dynamic'"
fi

# ── Rung DD: depth bias (polygon offset) honoured ────────
# loader_graphics_depth_bias: front triangle ndc.z=0.1 with
# depthBiasEnable + constant_factor=4e6 (≈ 0.477 bias),
# back triangle ndc.z=0.5 with no bias.  Drawn front-then-
# back.  Without bias, front wins the LESS test (0.1<0.5)
# and pixel stays green.  With bias honoured, front's
# effective depth is ~0.58, back's 0.5 is less, back wins
# and pixel turns red.  Pre-fix, pipeline depthBias* fields
# weren't extracted and the rasterizer had no bias offset.
if [ -x "$GRAPHICS_DEPTH_BIAS" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (depth_bias round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung DD: depth bias pushes front past back; back wins ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DEPTH_BIAS" 2>&1 | tail -2; then
        echo "FAIL: depth_bias round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung DD: need 'cargo build -p atrium-vk-icd --example loader_graphics_depth_bias'"
fi

# ── Rung EE: TRIANGLE_STRIP + primitive restart ──────────
# loader_graphics_restart: indexed strip with 7 u16
# indices [0,1,2,0xFFFF,3,4,5].  Sentinel 0xFFFF restarts
# the strip; two disjoint triangles get rendered.  Pre-fix,
# primitiveRestartEnable wasn't read from the create info,
# vkCmdSetPrimitiveRestartEnable was an ext_state_stub no-
# op, and the indexed strip-walk treated 0xFFFF as a real
# vertex index (= u16::MAX) -- the dispatch would have
# failed assembly or produced garbage geometry.
if [ -x "$GRAPHICS_RESTART" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (restart round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung EE: TRIANGLE_STRIP + primitive restart draws 2 disjoint triangles ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_RESTART" 2>&1 | tail -4; then
        echo "FAIL: restart round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung EE: need 'cargo build -p atrium-vk-icd --example loader_graphics_restart'"
fi

# ── Rung FF: explicit-LOD mip sampling ───────────────────
# loader_graphics_mipmap: 2-level mip chain (level 0: 2x2
# all red, level 1: 1x1 single blue texel) uploaded via
# vkCmdCopyBufferToImage with two regions (one per mip
# level).  FS uses OpImageSampleExplicitLod with LOD=1.0;
# helper must fetch from TexDesc.mip_descs[1].  Pixel(4,4)
# ends up pure blue.  Pre-fix, the daemon dropped
# mipLevel>0 copy regions on the floor (ImageStorage only
# held the base) and TexDesc.mip_descs was always null,
# so pick_tex_mip fell back to level 0 = red.
if [ -x "$GRAPHICS_MIPMAP" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (mipmap round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung FF: explicit LOD=1.0 fetches mip level 1 (blue) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_MIPMAP" 2>&1 | tail -2; then
        echo "FAIL: mipmap round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung FF: need 'cargo build -p atrium-vk-icd --example loader_graphics_mipmap'"
fi

# ── Rung GG: 2D-array texture sampling ───────────────────
# loader_graphics_array: 2-layer array texture (layer 0
# red, layer 1 green, 1x1 each).  FS samples a
# sampler2DArray with a 3-lane coord (u, v, layer=1.0);
# cranelift picks atrium_tex_sample_2d_array which reads
# `data + layer * slice_bytes`.  pixel(4,4) = green.
# Pre-fix, array_layers wasn't plumbed from the create
# payload, ImageStorage only held one layer, and
# TexDesc.depth/slice_bytes were always (1, 0) so the
# layer offset was zero (always layer 0 = red).
if [ -x "$GRAPHICS_ARRAY" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (array round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung GG: sampler2DArray fetches layer 1 (green) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_ARRAY" 2>&1 | tail -2; then
        echo "FAIL: array round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung GG: need 'cargo build -p atrium-vk-icd --example loader_graphics_array'"
fi

# ── Rung HH: cubemap sampling ────────────────────────────
# loader_graphics_cube: 6-face cube texture (1x1 each,
# distinct colour per face).  FS samples a samplerCube with
# a constant direction (0,0,1) -> +Z = face 4 = magenta.
# cranelift picks atrium_tex_sample_cube which does major-
# axis face selection + sc/tc/ma projection and reads
# `data + face * slice_bytes`.  pixel(4,4) = magenta.
# Reuses Rung GG's layered-image plumbing with depth=6.
if [ -x "$GRAPHICS_CUBE" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (cube round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung HH: samplerCube +Z face selection (magenta) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_CUBE" 2>&1 | tail -2; then
        echo "FAIL: cube round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung HH: need 'cargo build -p atrium-vk-icd --example loader_graphics_cube'"
fi

# ── Rung II: shadow-map (Dref) depth-comparison sampling ─
# loader_graphics_shadow: FS samples a sampler2DShadow with
# OpImageSampleDrefImplicitLod, dref=0.75, against a depth
# texture storing R=0.5.  The frontend lowers the Dref
# opcode to `r <= dref ? 1.0 : 0.0`; 0.5 <= 0.75 -> 1.0
# (lit).  FS outputs (cmp, 0, 0, 1) so a lit sample is red.
# pixel(4,4) = red (255,0,0,255).  A raw sample would read
# back R=128 (dark red) -- the comparison promotes it to
# 255.  Exercises the existing frontend Dref lowering
# end-to-end through the loader.
if [ -x "$GRAPHICS_SHADOW" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (shadow round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung II: shadow Dref compare (0.5 <= 0.75 = lit) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_SHADOW" 2>&1 | tail -2; then
        echo "FAIL: shadow round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung II: need 'cargo build -p atrium-vk-icd --example loader_graphics_shadow'"
fi

# ── Rung JJ: multiple render targets (MRT) ───────────────
# loader_graphics_mrt: FS writes Location 0 = red (-> colour
# attachment 0) + Location 1 = green (-> colour attachment 1)
# in one invocation, against a 2-attachment framebuffer.
# Each attachment is copied back independently.  attachment0
# pixel(3,3)=red, attachment1 pixel(3,3)=green.  Exercises
# the cranelift FS multi-output byte routing + the
# BindColorAttachments wire op + the daemon's per-attachment
# scatter.  Pre-MRT, the FS had a single output and the
# render pass a single attachment.
if [ -x "$GRAPHICS_MRT" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (MRT round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung JJ: MRT -- 2 FS outputs to 2 colour attachments ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_MRT" 2>&1 | tail -3; then
        echo "FAIL: MRT round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung JJ: need 'cargo build -p atrium-vk-icd --example loader_graphics_mrt'"
fi

# ── Rung KK: per-attachment blend / write-mask (MRT) ─────
# loader_graphics_mrt_blend: FS writes white to both colour
# attachments; the pipeline gives attachment 0 an RGBA write
# mask and attachment 1 an R-only mask.  attachment0
# pixel(3,3)=(255,255,255,255), attachment1=(255,0,0,0).
# Proves the daemon keeps independent per-attachment blend/
# write state (was shared in MRT v1 / Rung JJ).
if [ -x "$GRAPHICS_MRT_BLEND" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (MRT-blend round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung KK: per-attachment write mask (RGBA vs R-only) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_MRT_BLEND" 2>&1 | tail -3; then
        echo "FAIL: MRT-blend round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung KK: need 'cargo build -p atrium-vk-icd --example loader_graphics_mrt_blend'"
fi

# ── Rung LL: BeginRenderPass colour clear applied ────────
# loader_graphics_clear: clears the framebuffer to blue
# (0,0,1,1), draws a small red triangle.  pixel(0,0)
# (outside the triangle) must read the clear colour blue;
# pixel(3,3) (inside) reads red.  Pre-fix, tier-2 never
# applied the BeginRenderPass colour clear (relied on the
# zero-allocated image), so a non-covered pixel would have
# been (0,0,0,0) and a non-zero clear was dropped.
if [ -x "$GRAPHICS_CLEAR" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (clear round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung LL: BeginRenderPass colour clear (blue) applied ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_CLEAR" 2>&1 | tail -3; then
        echo "FAIL: clear round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung LL: need 'cargo build -p atrium-vk-icd --example loader_graphics_clear'"
fi

# ── Rung MM: sampler compareOp (beyond LEQUAL) ───────────
# loader_graphics_shadow_cmp: sampler compareEnable=true +
# compareOp=LESS, stored depth 0.5, dref 0.25.  Vulkan's
# `dref compareOp texel` -> 0.25 < 0.5 = lit (red).  The
# legacy hardwired-LEQUAL path (texel<=dref = 0.5<=0.25 =
# false) would give black, so red proves the runtime Dref
# helper read the sampler's compareOp.
if [ -x "$GRAPHICS_SHADOW_CMP" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (shadow_cmp round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung MM: sampler compareOp=LESS (dref 0.25 < texel 0.5 = lit) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_SHADOW_CMP" 2>&1 | tail -2; then
        echo "FAIL: shadow_cmp round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung MM: need 'cargo build -p atrium-vk-icd --example loader_graphics_shadow_cmp'"
fi

# ── Rung NN: PCF (bilinear shadow comparison) ────────────
# loader_graphics_pcf: LINEAR comparison sampler, dref 0.5,
# 2x2 depth texture (top row 0.25 fails, bottom row 0.75
# passes), sampled at the constant centre (0.5,0.5).  PCF
# compares all 4 taps (0,0,1,1) + bilinearly blends -> 0.5
# -> mid-grey ~128.  A point compare would snap to 0/255.
if [ -x "$GRAPHICS_PCF" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (pcf round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung NN: PCF 4-tap shadow blend -> mid-grey ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_PCF" 2>&1 | tail -2; then
        echo "FAIL: pcf round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung NN: need 'cargo build -p atrium-vk-icd --example loader_graphics_pcf'"
fi

# ── Rung OO: implicit-LOD mip selection (derivatives) ────
# loader_graphics_lod: 2-level mip texture (mip0 red, mip1
# blue), implicit sampling (OpImageSampleImplicitLod), UV
# 0..8 over a ~4px triangle -> heavy minification.  The
# rasterizer finite-differences the perspective-correct UV
# varying across the pixel quad, computes LOD, and redirects
# the descriptor to the coarse mip -> blue.  Pre-OO,
# implicit sampling always used mip 0 (red) since the
# dispatcher zeroed derivatives.
if [ -x "$GRAPHICS_LOD" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (lod round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung OO: implicit LOD picks coarse mip on minified texture ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_LOD" 2>&1 | tail -2; then
        echo "FAIL: lod round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung OO: need 'cargo build -p atrium-vk-icd --example loader_graphics_lod'"
fi

# ── Rung PP: 4x MSAA (coverage-resolved) ─────────────────
# loader_graphics_msaa: pipeline rasterizationSamples=4,
# pure-red triangle over a black clear.  The rasterizer
# tests 4 sub-pixel sample points per pixel and blends the
# fragment by the covered fraction, so triangle edges come
# out as partial reds (antialiased).  Asserts >=1 pixel with
# 0<R<255 + solid-red interior + black corner.  Without MSAA
# every pixel is binary (R=0 or 255).
if [ -x "$GRAPHICS_MSAA" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (msaa round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung PP: 4x MSAA antialiased edges ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_MSAA" 2>&1 | tail -4; then
        echo "FAIL: msaa round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung PP: need 'cargo build -p atrium-vk-icd --example loader_graphics_msaa'"
fi

# ── Rung QQ: half-float (f16) vertex attribute ───────────
# loader_graphics_half: per-vertex colour as
# R16G16B16A16_SFLOAT (4 x f16).  All verts share
# (1.0,0.5,0.0,1.0); the daemon's vertex assembler decodes
# f16 -> f32.  pixel(4,4) = (255,128,0,255) -- the 0.5
# green lane proves the mantissa/exponent decode, not a raw
# byte copy.
if [ -x "$GRAPHICS_HALF" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (half round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung QQ: R16G16B16A16_SFLOAT half-float vertex attribute ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_HALF" 2>&1 | tail -2; then
        echo "FAIL: half round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung QQ: need 'cargo build -p atrium-vk-icd --example loader_graphics_half'"
fi

# ── Rung RR: A2B10G10R10_UNORM_PACK32 vertex attribute ───
# loader_graphics_rgb10a2: per-vertex colour as a packed
# 32-bit A2B10G10R10 word.  All verts share (1023,512,0,3)
# -> (1.0, 0.5005, 0.0, 1.0).  The daemon unpacks the
# 10/10/10/2-bit fields + normalises.  pixel(4,4) ~
# (255,128,0,255); the green lane proves field extraction.
if [ -x "$GRAPHICS_RGB10A2" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (rgb10a2 round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung RR: A2B10G10R10_UNORM_PACK32 vertex attribute ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_RGB10A2" 2>&1 | tail -2; then
        echo "FAIL: rgb10a2 round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung RR: need 'cargo build -p atrium-vk-icd --example loader_graphics_rgb10a2'"
fi

# ── Rung SS: screen-space derivatives (2x2-quad lockstep) ─
# loader_graphics_deriv: the FS receives a scalar varying
# `u` ramping with screen-x and emits dFdx(u*u) into the
# red channel.  The daemon shades each covered pixel in a
# 2x2 quad -- a probe pass re-runs the FS at all four lanes
# (recording the operand of every derivative op into a
# thread-local QuadState), then a final pass runs for the
# real pixel where `atrium_deriv` returns the lane finite
# difference.  Because the differentiated expression is
# non-affine (u*u), its derivative GROWS left->right; a
# trivial "derivative-of-a-varying-is-constant" shortcut
# could not produce that, and the pre-quad zero-lowering
# would leave the frame black.  Asserts R(left) < R(right),
# both > 0.
if [ -x "$GRAPHICS_DERIV" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (deriv round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung SS: dFdx(u*u) via 2x2-quad lockstep derivatives ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DERIV" 2>&1 | tail -4; then
        echo "FAIL: deriv round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung SS: need 'cargo build -p atrium-vk-icd --example loader_graphics_deriv'"
fi

# ── Rung TT: instanced rendering via gl_InstanceIndex ────
# loader_graphics_instanced: one bottom-third quad drawn with
# instanceCount=3.  The VS reads gl_InstanceIndex to shift the
# quad up by instance*(2/3) clip-y AND to pick a red intensity
# (instance+1)/3, so the three instances tile the framebuffer
# into bands of R = 85 / 170 / 255.  The daemon replays the
# draw once per instance (firstInstance..+instanceCount),
# handing each its index as VS params[5].  Without the loop
# only one band paints; without the index plumbing all bands
# share instance 0's colour.  Asserts 3 distinct, strictly
# increasing band reds.
if [ -x "$GRAPHICS_INSTANCED" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (instanced round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung TT: instanced rendering via gl_InstanceIndex ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_INSTANCED" 2>&1 | tail -3; then
        echo "FAIL: instanced round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung TT: need 'cargo build -p atrium-vk-icd --example loader_graphics_instanced'"
fi

# ── Rung UU: per-instance vertex input rate ──────────────
# loader_graphics_instance_rate: two vertex bindings -- binding
# 0 per-vertex (the quad positions), binding 1 with inputRate=
# INSTANCE carrying a (y_offset, red) record per instance.  The
# VS adds the per-instance y_offset and emits the per-instance
# red; no gl_InstanceIndex is read.  3 instances -> 3 bands of
# R = 85 / 170 / 255.  The assembler indexes a per-instance
# binding by the instance number and re-gathers per instance;
# ignoring the rate would make all bands read record 0.  Asserts
# 3 distinct, strictly increasing band reds.
if [ -x "$GRAPHICS_INSTANCE_RATE" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (instance-rate round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung UU: per-instance vertex input rate ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_INSTANCE_RATE" 2>&1 | tail -3; then
        echo "FAIL: instance-rate round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung UU: need 'cargo build -p atrium-vk-icd --example loader_graphics_instance_rate'"
fi

# ── Rung VV: gl_FrontFacing in the fragment shader ───────
# loader_graphics_frontface: two triangles with opposite
# winding (the second's vertex order reversed) drawn with
# cullMode=NONE, default front-face CCW.  The FS colours
# front-facing fragments green and back-facing red via two
# scalar OpSelects on gl_FrontFacing (the trailing FS param
# the rasterizer fills from the triangle's screen-space winding
# vs VkFrontFace).  Triangle A (left) and B (right) thus come
# out one green + one red.  If gl_FrontFacing were constant,
# both halves would match.  Asserts exactly one green + one red.
if [ -x "$GRAPHICS_FRONTFACE" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (frontface round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung VV: gl_FrontFacing front/back split ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_FRONTFACE" 2>&1 | tail -3; then
        echo "FAIL: frontface round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung VV: need 'cargo build -p atrium-vk-icd --example loader_graphics_frontface'"
fi

# ── Rung WW: PointList topology ──────────────────────────
# loader_graphics_points: three vertices drawn with
# VK_PRIMITIVE_TOPOLOGY_POINT_LIST.  The daemon's point path
# runs the VS per vertex, viewport-maps it to a window pixel,
# and shades a single 1x1 fragment with that vertex's colour.
# The three points land at pixels (2,2) red, (4,4) green,
# (6,6) blue and exactly three pixels are lit -- proof the
# vertices were rasterized as points, not assembled into one
# filled triangle (the prior TriangleList fallback).
if [ -x "$GRAPHICS_POINTS" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (points round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung WW: PointList topology (1x1 fragment per vertex) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_POINTS" 2>&1 | tail -2; then
        echo "FAIL: points round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung WW: need 'cargo build -p atrium-vk-icd --example loader_graphics_points'"
fi

# ── Rung XX: LineList topology ───────────────────────────
# loader_graphics_lines: a single segment drawn with
# VK_PRIMITIVE_TOPOLOGY_LINE_LIST from a red left endpoint to
# a blue right endpoint at framebuffer row 4.  The daemon's
# line path runs the VS for both endpoints, viewport-maps
# them, and walks the segment by DDA, shading each fragment
# with the perspective-correctly interpolated colour.  Row 4
# comes out a red->blue gradient; off-line pixels stay clear.
# Two vertices can't form a triangle, so the prior
# TriangleList fallback would have drawn nothing.  Asserts a
# multi-pixel row-4 line, red-leaning left + blue-leaning
# right, clear elsewhere.
if [ -x "$GRAPHICS_LINES" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (lines round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung XX: LineList topology (DDA segment per vertex pair) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_LINES" 2>&1 | tail -2; then
        echo "FAIL: lines round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung XX: need 'cargo build -p atrium-vk-icd --example loader_graphics_lines'"
fi

# ── Rung YY: LineStrip topology ──────────────────────────
# loader_graphics_linestrip: three vertices drawn with
# VK_PRIMITIVE_TOPOLOGY_LINE_STRIP forming an L -- segment 0
# (V0->V1) horizontal along row 6, segment 1 (V1->V2) vertical
# down column 4, sharing the corner.  Proves the strip connects
# consecutive vertices (a LineList would draw only segment 0
# and drop V2).  Asserts row 6 lit + column 4 vertical run +
# clear elsewhere.
if [ -x "$GRAPHICS_LINESTRIP" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (linestrip round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung YY: LineStrip topology (connected polyline) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_LINESTRIP" 2>&1 | tail -2; then
        echo "FAIL: linestrip round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung YY: need 'cargo build -p atrium-vk-icd --example loader_graphics_linestrip'"
fi

# ── Rung ZZ: TriangleFan topology ────────────────────────
# loader_graphics_trifan: four vertices drawn with
# VK_PRIMITIVE_TOPOLOGY_TRIANGLE_FAN -- a centre vertex + three
# outer vertices make two triangles (0,1,2) and (0,2,3) that
# both share vertex 0.  Pixel (4,1) lands in the first wedge,
# (6,4) in the second; both lit proves the fan reused vertex 0
# (a TriangleList of 4 verts makes one triangle and drops v3,
# leaving (6,4) clear).  Far corner stays clear.
if [ -x "$GRAPHICS_TRIFAN" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (trifan round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung ZZ: TriangleFan topology (shared vertex 0) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_TRIFAN" 2>&1 | tail -2; then
        echo "FAIL: trifan round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung ZZ: need 'cargo build -p atrium-vk-icd --example loader_graphics_trifan'"
fi

# ── Rung AAA: BGRA texture format (channel-order swap) ───
# loader_graphics_bgra: a 2x2 texture created as
# B8G8R8A8_UNORM whose texels encode RED in BGRA byte order
# ([B=0,G=0,R=255,A=255]).  The daemon now propagates each
# image's VkFormat into the runtime TexDesc, so the sampler
# reads R from byte 2 + B from byte 0 -> red.  A format-blind
# daemon (the prior hardcoded Rgba8Unorm) would have produced
# blue.  Asserts pixel(4,4) is red-dominant.
if [ -x "$GRAPHICS_BGRA" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (bgra round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung AAA: BGRA texture format (R/B swap at sample) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_BGRA" 2>&1 | tail -2; then
        echo "FAIL: bgra round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung AAA: need 'cargo build -p atrium-vk-icd --example loader_graphics_bgra'"
fi

# ── Rung BBB: sRGB texture format (EOTF at sample) ───────
# loader_graphics_srgb: a 2x2 R8G8B8A8_SRGB texture whose
# texels are mid-grey 188/255.  The sampler applies the
# sRGB->linear EOTF, so 0.737 sRGB becomes ~0.503 linear ->
# ~128 in the linear RGBA8 target.  A format-blind daemon would
# pass 188 straight through.  Asserts pixel(4,4) near 128.
if [ -x "$GRAPHICS_SRGB" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (srgb round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung BBB: sRGB texture format (EOTF at sample) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_SRGB" 2>&1 | tail -2; then
        echo "FAIL: srgb round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung BBB: need 'cargo build -p atrium-vk-icd --example loader_graphics_srgb'"
fi

# ── Rung CCC: gl_PrimitiveID in the fragment shader ──────
# loader_graphics_primid: two triangles drawn TriangleList; the
# FS reads gl_PrimitiveID (gated by the SPIR-V Geometry
# capability) and colours primitive 0 red, primitive 1 green.
# The rasterizer supplies the per-triangle index as the FS's
# trailing param.  Left triangle red + right green proves each
# primitive saw its own index.
if [ -x "$GRAPHICS_PRIMID" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (primid round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung CCC: gl_PrimitiveID per-primitive colour ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_PRIMID" 2>&1 | tail -2; then
        echo "FAIL: primid round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung CCC: need 'cargo build -p atrium-vk-icd --example loader_graphics_primid'"
fi

# ── Rung DDD: R8_UNORM single-channel texture (native) ───
# loader_graphics_r8: a 2x2 R8_UNORM texture -- ONE byte per
# texel (4 bytes total), stored natively at 1 byte/texel with a
# width*1 row stride.  The sampler returns (R, 0, 0, 1).  Proves
# the daemon allocates + uploads + strides narrow texels
# natively (not expanded to RGBA8).  Asserts pixel(4,4) ~
# (R, 0, 0, 255).
if [ -x "$GRAPHICS_R8" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (r8 round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung DDD: R8_UNORM single-channel texture ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_R8" 2>&1 | tail -2; then
        echo "FAIL: r8 round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung DDD: need 'cargo build -p atrium-vk-icd --example loader_graphics_r8'"
fi

# ── Rung EEE: R8G8_UNORM two-channel texture (native) ────
# loader_graphics_rg8: a 2x2 R8G8_UNORM texture -- TWO bytes per
# texel (8 bytes total), stored natively at 2 bytes/texel with a
# width*2 row stride.  Texels are (R=200, G=40) so R != G proves
# both channels read from the right offsets.  Sampler returns
# (R, G, 0, 1).  Asserts pixel(4,4) ~ (200, 40, 0, 255).
if [ -x "$GRAPHICS_RG8" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (rg8 round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung EEE: R8G8_UNORM two-channel texture ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_RG8" 2>&1 | tail -2; then
        echo "FAIL: rg8 round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung EEE: need 'cargo build -p atrium-vk-icd --example loader_graphics_rg8'"
fi

# ── Rung FFF: gl_FragDepth (shader-written depth, late-Z) ─
# loader_graphics_fragdepth: a screen-covering triangle into a
# depth buffer cleared to 0.5, depth test LESS + write on.  The
# FS writes gl_FragDepth = (t < 0.5) ? 0.25 : 0.75 along a
# left->right varying.  Left half (0.25 < 0.5) passes -> green;
# right half (0.75) fails -> stays clear.  gl_Position.z is 0
# everywhere, so without honouring gl_FragDepth the whole
# triangle would be green.  Proves the daemon's gated late-depth
# path (run FS, then test/write against the shader depth) +
# backend routing of the FragDepth store to out_depth.
if [ -x "$GRAPHICS_FRAGDEPTH" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (fragdepth round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung FFF: gl_FragDepth late depth test ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_FRAGDEPTH" 2>&1 | tail -2; then
        echo "FAIL: fragdepth round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung FFF: need 'cargo build -p atrium-vk-icd --example loader_graphics_fragdepth'"
fi

# ── Rung GGG: damage / dirty-rect (loadOp=LOAD preserve + scissor) ─
# loader_graphics_damage: two render passes into one 16x16 image.
# Pass 1 (loadOp=CLEAR) clears blue and draws a red triangle.
# Pass 2 (loadOp=LOAD) preserves the framebuffer, scissors to the
# right half, and draws a green triangle.  The discriminating
# pixels -- left-half triangle stays RED, right-half background
# stays BLUE -- can only survive if the ICD translated loadOp=LOAD
# into BEGIN_RP_FLAG_NO_CLEAR (a clear-path regression blacks them
# out).  Proves the in-app partial-update / per-window compositor
# damage primitive end-to-end through loader -> ICD -> daemon.
if [ -x "$GRAPHICS_DAMAGE" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (damage round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung GGG: damage/dirty-rect (loadOp=LOAD preserve + scissor) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_DAMAGE" 2>&1 | tail -2; then
        echo "FAIL: damage round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung GGG: need 'cargo build -p atrium-vk-icd --example loader_graphics_damage'"
fi

# ── Rung HHH: P1b.2 pass-level batch routes per-draw shaders ──
# loader_graphics_multidraw: TWO draws with DIFFERENT fragment
# shaders (red, green) in ONE render pass.  The daemon accumulates
# both into a single batched dispatch; each triangle's draw_idx
# must select its own draw's fs_main + state.  Left triangle comes
# back RED (draw 1), right triangle GREEN (draw 2) -- a routing bug
# would mis-assign or share the shader/state across the batch.
if [ -x "$GRAPHICS_MULTIDRAW" ]; then
    rm -f "$SOCKET"
    "$DAEMON" --socket "$SOCKET" \
        --backend tier2 --tier2 \
        --cache-root "$CACHE_ROOT" \
        --compile-binary "$COMPILE" \
        ${SPIRV_OPT:+--spirv-opt-binary "$SPIRV_OPT"} \
        > /tmp/aqueduct-loader-smoke.log 2>&1 &
    DAEMON_PID=$!
    if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
        echo "daemon failed to start (multidraw round-trip); log:" >&2
        cat /tmp/aqueduct-loader-smoke.log >&2
        exit 1
    fi
    echo
    echo "=== Rung HHH: pass-level batch routes per-draw shaders (draw_idx) ==="
    if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
        VK_DRIVER_FILES="$MANIFEST" \
        ATRIUM_VK_ICD_SOCKET="$SOCKET" \
        "$GRAPHICS_MULTIDRAW" 2>&1 | tail -2; then
        echo "FAIL: multidraw round-trip did not return 0" >&2
        exit 1
    fi
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""
else
    echo
    echo "SKIP Rung HHH: need 'cargo build -p atrium-vk-icd --example loader_graphics_multidraw'"
fi

echo
echo "OK: loader smoke clean through all rungs."
