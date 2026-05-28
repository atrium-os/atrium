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
echo "=== Rung 1: no daemon -- expect VK_ERROR_INITIALIZATION_FAILED ==="
set +e
DYLD_LIBRARY_PATH=/opt/homebrew/lib \
  VK_DRIVER_FILES="$MANIFEST" \
  vulkaninfo --summary 2>&1 | tail -5
set -e

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
echo "=== Rung 2: daemon up -- expect 1 device ==="
DYLD_LIBRARY_PATH=/opt/homebrew/lib \
  VK_DRIVER_FILES="$MANIFEST" \
  ATRIUM_VK_ICD_SOCKET="$SOCKET" \
  vulkaninfo --summary 2>&1 | grep -E "Devices:|GPU0:|apiVersion|deviceName|deviceType" | head -10

echo
echo "=== Rung 3: full vulkaninfo readout exit code ==="
DYLD_LIBRARY_PATH=/opt/homebrew/lib \
  VK_DRIVER_FILES="$MANIFEST" \
  ATRIUM_VK_ICD_SOCKET="$SOCKET" \
  vulkaninfo > /tmp/atrium-vulkaninfo.out 2>&1
echo "vulkaninfo exit=$?  (output in /tmp/atrium-vulkaninfo.out, $(wc -l </tmp/atrium-vulkaninfo.out | tr -d ' ') lines)"

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
      "$EXAMPLE" 2>&1 | grep -E "vk|trivial|done"
    n_afblob=$(find "$CACHE_ROOT" -name '*.afblob' 2>/dev/null | wc -l | tr -d ' ')
    n_pcmap=$(find "$CACHE_ROOT"  -name '*.pcmap'  2>/dev/null | wc -l | tr -d ' ')
    echo "cache contents: $n_afblob .afblob, $n_pcmap .pcmap"
    if [ "$n_afblob" != "1" ] || [ "$n_pcmap" != "1" ]; then
        echo "FAIL: tier-2 did not produce the expected cache artifacts" >&2
        exit 1
    fi
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
      "$EXAMPLE" 2>&1 | grep -E "vk|trivial|done"
    grep -E "tier-2 backend selected" /tmp/aqueduct-loader-smoke.log >/dev/null \
        || { echo "FAIL: --backend tier2 did not log selection" >&2; exit 1; }
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

echo
echo "OK: loader smoke clean through all rungs."
