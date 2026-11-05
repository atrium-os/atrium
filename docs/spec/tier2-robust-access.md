# Tier-2: robust memory access for untrusted shaders

Status: all three steps done 2026-11-05 (bounds checks, compiler sandbox, per-app executor jails); verified in the amd64 FreeBSD VM. Owner: the Tier-2 thread. Asked for by Shankar: Tier-2 runs
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

## 3. Jail the compiler (step 2): done

The loader spawns `atrium-spv-compile --stdio`. The compiler sandboxes itself before it reads
a byte of SPIR-V, reads the module from stdin (at most 16 MiB) and writes `ASPVOUT1`, then
the blob and the pcmap (each a u64 length plus bytes), to stdout. It never touches the
filesystem; the loader writes the cache files itself (temp file + rename), so a compromised
compiler can't read files or poison other cache entries.

- FreeBSD: Capsicum `cap_enter` (fatal if it fails).
- Linux (development): `PR_SET_NO_NEW_PRIVS` plus a deny-all Landlock ruleset
  (filesystem, and TCP on Landlock ABI ≥ 4); rlimits only on kernels without Landlock.
- Everywhere: RLIMIT_CPU 30 s, RLIMIT_AS 4 GiB (not on macOS).

This replaces D3's Portcullis spawn: with no filesystem or network the compiler needs no
mounts, and it inherits the executor's jail from step 3. `--sandbox-check PATH` lets tests
confirm the sandbox refuses to open a file (`binary_sandbox_refuses_file_access`). The
`--input/--output-dir` mode stays as an unsandboxed developer tool; `--hash` must be hex.

Limits: the executor still maps whatever the compiler returns, so a compromised compiler can
hand back unchecked code; the executor jail (step 3) is the backstop for that. The optional
`spirv-opt` pre-pass in `aqueduct-gpu-host` (`--spirv-opt-binary`, off by default) parses
app SPIR-V unsandboxed and should stay a development option. Not yet run on FreeBSD
(cross-checked only); the first run is part of step 3's VM test.

(Update: in the amd64 VM, `--sandbox-check` exits 0 and the per-app workers compile through the sandboxed compiler.)

## 4. Jail the executor (step 3): done, one jail per app

One `aqueduct-gpu-host` used to serve every app, and frames name resources by raw,
client-chosen ids, so one app could read another's images. Shankar chose one executor
jail per app (2026-11-05).

- **Broker** (`--transport broker`, `aqueduct-gpu-host/src/broker.rs`): the endpoint apps
  connect to. It reads nothing from a client. Each connection is handed, as stdin, to a
  one-shot jail of the bundle `org.atrium.aqueduct.gpu` that portcullisd starts
  (ExecInstance, like Fresco's renderers). No portcullisd, no worker: the client is
  refused (fail closed). At most 64 workers, 16 per client uid.
  `AQUEDUCT_GPU_WORKER_LAUNCHER=unconfined` runs workers as plain child processes, for
  development and tests only. Runs as `_aqueduct-gpu` (65014) from
  `aqueduct-gpu-host/etc/services.d/45-atrium-aqueduct-gpu.toml`.
- **Worker** (`--transport stdin`; the binary named `aqueduct-gpu-worker` starts in this
  mode with Tier-2 defaults): serves one session on fd 0 and exits on hang-up, which ends
  the jail. Runs as `_aqueduct-gpu-app` (65015) under the system policy's worker rule,
  with the bundle's 1 GiB memory cap (racct required). It writes no files: an empty
  `--cache-root` makes the loader map blobs straight from the compiler's reply and keep
  them in memory, so compiled code is per app.
- **Install**: `scripts/aqueduct-gpu-bundle.sh` (users, signed bundle, policy rule, broker
  binary, socket dir). The broker's socket is mode 0666; who can reach it is decided by
  which jails mount `/atrium/sockets/aqueduct-gpu`.
- **Size limits** (0da3967): Tier-1 path command counts and Tier-2 colour-attachment
  counts are bounded by the record length, mip levels by the image's chain, images by
  2048 layers and 1 GiB. A per-app memory cap is the jail's rctl limit.
- **Tests**: `aqueduct-gpu-host/tests/broker.rs` (one worker per connection, a hang-up
  ends only its worker, refused without portcullisd). `examples/tier2_probe.rs` runs a
  compute shader through any endpoint and checks a far out-of-bounds store lands nowhere.
  VM run (amd64, racct on): four probes as an ordinary user, each in its own
  `app-org-atrium-aqueduct-gpu--gpuN` jail as `_aqueduct-gpu-app`, all `PROBE OK`.

App access: a manifest with `graphics = "vulkan"` gets the broker's socket dir mounted
(`portcullis-jail` `apply_graphics`), and atrium-vk-icd uses
`/atrium/sockets/aqueduct-gpu/aqueduct-gpu.sock` when it exists (`ATRIUM_VK_ICD_SOCKET`
still overrides).

Not done yet: the old shared listener (`--transport socket`) still
exists for development and the Mac host, and the arm64 bespoke backend still declines
SSBO shaders (they use Cranelift).
