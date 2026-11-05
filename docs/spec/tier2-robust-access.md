# Tier-2: robust memory access for untrusted shaders

Status: step 1 done 2026-11-05 (bounds checks, ABI v5); steps 2 and 3 next. Owner: the Tier-2 thread. Asked for by Shankar: Tier-2 runs
app-supplied SPIR-V as native code, which is arbitrary code execution, so it has to be
bounded in the code and jailed around it.

## 1. What is wrong today

| Gap | Where |
|---|---|
| SSBO indices are not checked. A dynamic index (`buf[i]`) adds `i * stride` to the binding's base; constant offsets past the buffer's end aren't checked either. A shader reads and writes the host process's memory. | `atrium-spv-frontend/src/functions.rs` (AccessChain: "SSBO indices stay as-is"), all three backends |
| Dynamic indices into Uniform, PushConstant and Workgroup arrays are not clamped (only Function arrays are). | same |
| The VS storage table can hold a null or zero-length buffer for a declared binding. | `aqueduct-gpu-host/src/tier2_backend.rs` (VS SSBO table) |
| `atrium-spv-compile` is spawned with a plain `Command::new`; spec D3 says Portcullis. | `atrium-spv-loader/src/lib.rs` |
| App shaders execute inside `aqueduct-gpu-host --backend tier2`, which has no jail or services.d manifest. Tier-1 runs in the same process. | `aqueduct-gpu-host` |

Fresco's renderers are not affected: their kernels are ours, precompiled, and already run
in each renderer's jail (`fresco-f4-split.md`).

## 2. Bounds checks (step 1)

**Semantics.** Vulkan `robustBufferAccess`: an out-of-bounds access may read any value
inside the buffer or zero, and may write anywhere inside the buffer. Tier-2 is stricter:
an out-of-bounds access goes to a 64-byte *sink* owned by the descriptor table, so it never
touches any buffer (an out-of-bounds read returns whatever earlier out-of-bounds writes left
there). This works for empty and tiny buffers, which an "offset 0 of the same binding" rule
would not.

**ABI v5.** Every StorageBuffer access goes through a descriptor table of 16-byte entries
`{ u64 base, u64 size_bytes }`, one per binding, for compute (`out_buffer` argument, which
loses its single-binding "direct pointer" mode) and vertex (`storage_table`). The 16 bytes
before entry 0 hold `{ u64 sink, 0 }` (`SSBO_SINK_OFFSET = -16`). `atrium_spv_runtime::
SsboTable` builds the table and owns the sink. `TIER2_SHADER_ABI_VERSION` goes 4 → 5, which
invalidates cached blobs.

**Check.** At each load, store or atomic through an SSBO-derived pointer, with `doff` the
dynamic byte offset (sum of `zext(index) * stride`, 64-bit), `off` the constant offset and
`n` the access size:

```
end  = doff + off + n            (64-bit; doff < 2^32 * stride, no overflow)
addr = end <= size ? base + doff + off : sink
```

One compare and a conditional move per access; constant-only accesses check too, since
`off` can be past a small buffer.

**Static arrays elsewhere.** A dynamic index into any array of known length (Uniform,
PushConstant, Workgroup, Function) is clamped in the frontend; a constant index past a
sized array is a compile error.

**Storage-image texel pointers.** `OpImageTexelPointer` coordinates clamp per axis
(`c < dim ? c : 0`), so image atomics stay inside the image.

**Backends.** x86 bespoke and Cranelift implement the check. The arm64 bespoke backend
returns `Unsupported` for StorageBuffer access until it has the check, so those shaders use
Cranelift (it compiles few real shaders anyway; see the real-shader test).

**Tests.** Differential tests with in-bounds indices stay bit-exact (116 in
`atrium-spv-backend-bespoke/tests/differential_compute.rs`, also with forced spills). The
`robust_*` tests there index far out of bounds (loads, stores and atomics; dynamic,
constant, u32-wrapping; empty and unaligned buffers; texel pointers) on both backends, with
the buffer inside a canary-filled allocation, and require no crash and no write outside
it. Disabling the Cranelift guard makes all of them fail.

## 3. Jail the compiler (step 2)

`atrium-spv-compile` runs with read access to its input file and write access to the cache
directory only: Capsicum (`cap_enter` after opening both) on FreeBSD, plus the Portcullis
spawn the spec already calls for.

## 4. Jail the executor (step 3)

App shaders (Tier-2) and app draw records (Tier-1) run in a jailed `aqueduct-gpu-host`
with its own user and no writable mounts but its socket and shader cache, through a
services.d manifest like frescod's. Tier-1 also gets size limits on wire data (texture and
path dimensions), so a client can't make it allocate without bound.
