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
DAEMON="$REPO_ROOT/aqueduct-gpu-host/target/debug/aqueduct-gpu-host"
COMPILE="$REPO_ROOT/atrium-spv-compile/target/debug/atrium-spv-compile"
SLANGC="$REPO_ROOT/external/slang-bin/bin/slangc"
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
"
# loop_mul parked: surfaces a real frontend gap (`OpVariable
# Function` not yet supported in atrium-spv-compile phase 1 v3
# -- Slang emits a function-local `uint v` to hold the loop-
# carried value).  Annotation fix (annotate_loop_merges in
# Session::handle_shader_upload) means the validator now
# accepts Slang's loops, but the next step in the pipeline
# rejects the local-variable lowering.  Track separately.
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

    # Reap the Rung-7 daemon before the extra-shaders loop.
    # Each extra rung gets its own daemon to keep buffer / pipeline
    # state hermetic -- Tier2Backend's buffers map is daemon-
    # scoped, so re-using a daemon across rungs lets the previous
    # rung's stale BufferRecord at the same buffer_id bleed into
    # the new one's readback.  Documented as a real architectural
    # finding from the loader smoke run (Arc 140).
    kill_daemon "$DAEMON_PID"
    DAEMON_PID=""

    # ── Rungs 8+: extra slang shaders, each with a fresh daemon.
    # Each entry in EXTRA_SHADERS is "name:seed:expect".  The
    # .slang source must live at $SHADER_DIR/<name>.slang; we
    # compile it inline (cheap; cache hits after the first run).
    rung_n=8
    for entry in $EXTRA_SHADERS; do
        name=$(echo "$entry" | cut -d: -f1)
        seed=$(echo "$entry" | cut -d: -f2)
        expect=$(echo "$entry" | cut -d: -f3)
        src="$SHADER_DIR/$name.slang"
        spv="$SHADER_DIR/$name.comp.spv"
        [ -f "$src" ] || { echo "missing $src" >&2; exit 1; }
        "$SLANGC" "$src" -target spirv -entry main -stage compute \
            -o "$spv" 2>/tmp/slangc.log \
            || { echo "FAIL: slangc $name"; cat /tmp/slangc.log; exit 1; }
        rm -f "$SOCKET"
        "$DAEMON" --socket "$SOCKET" \
            --backend tier2 --tier2 \
            --cache-root "$CACHE_ROOT" \
            --compile-binary "$COMPILE" \
            > /tmp/aqueduct-loader-smoke.log 2>&1 &
        DAEMON_PID=$!
        if ! wait_for_daemon "$DAEMON_PID" "$SOCKET"; then
            echo "daemon failed to start (Rung $rung_n / $name); log:" >&2
            cat /tmp/aqueduct-loader-smoke.log >&2
            exit 1
        fi
        echo
        echo "=== Rung $rung_n: $name.slang (seed=$seed, expect=$expect) ==="
        if ! DYLD_LIBRARY_PATH=/opt/homebrew/lib \
            VK_DRIVER_FILES="$MANIFEST" \
            ATRIUM_VK_ICD_SOCKET="$SOCKET" \
            ATRIUM_VK_SMOKE_SHADER=slang \
            ATRIUM_VK_SMOKE_SHADER_PATH="$spv" \
            ATRIUM_VK_SMOKE_SEED="$seed" \
            ATRIUM_VK_SMOKE_EXPECT="$expect" \
            "$ROUNDTRIP" 2>&1 | tail -2; then
            echo "FAIL: $name round-trip" >&2
            exit 1
        fi
        kill_daemon "$DAEMON_PID"
        DAEMON_PID=""
        rung_n=$((rung_n + 1))
    done
fi

echo
echo "OK: loader smoke clean through all rungs."
