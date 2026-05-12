# aqueduct-gpu-host

macOS-side daemon and toolchain for the **aqueduct-gpu** protocol —
Atrium's paravirt GPU substrate. See
[`docs/spec/aqueduct-gpu.md`](../docs/spec/aqueduct-gpu.md) for the
full spec.

This crate ships:
- A pluggable [`Backend`] trait with three impls (Stub, Software,
  MoltenVk).
- A tier-1 software renderer producing real pixels via tiny-skia.
- A SPIR-V validator / annotator / cache forming Atrium's universal
  shader sandbox.
- Two binaries: the host daemon and a developer-facing shader tool.

## What works today (Phase 1.3c + 2.0–2.12)

```
Atrium app (frescod or atrium-vk-icd)
  ├─ fresco-protocol scene  ──┐
  │                            │
  │                            ▼
  │             fresco-aqueduct-bridge translators
  │             (rect, path, textured_rect, glyph_run)
  │                            │
  │                            ▼
  │             aqueduct-gpu-client::GpuClient
  │                            │
  │                            ▼ Unix socket (host endpoint)
  │             ┌──────────────────────────────────┐
  │             │ aqueduct-gpu-host (this crate)   │
  │             │  Session per connection          │
  │             │  ResourceTable per session       │
  │             │  ShaderCache (warm path)         │
  │             │  ShaderValidator (strict mode)   │
  │             │                                  │
  │             │  Backend trait (one of):         │
  │             │   - StubBackend                  │
  │             │   - SoftwareBackend (tier-1 SW)  │
  │             │   - MoltenVkBackend (skeleton)   │
  │             └──────────────────────────────────┘
```

End-to-end smoke test:
```sh
cargo run --example demo -p fresco-aqueduct-bridge
# Renders a fresco scene with rect/path/glyph_run into a PNG using
# the in-process SoftwareBackend. No Vulkan needed.
```

## Crate layout

| Path | Purpose |
|---|---|
| `src/backend.rs` | `Backend` trait + StubBackend + SoftwareBackend |
| `src/moltenvk.rs` | MoltenVkBackend skeleton (loader+device; cmdbuf pending) |
| `src/software/` | Tier-1 tiny-skia renderer (rect/path/textured_rect/glyph_run, multi-renderpass) |
| `src/listener.rs` | Unix socket accept loop |
| `src/session.rs` | Per-connection dispatch |
| `src/resources.rs` | Per-session resource tables (memory/image/buffer/sampler/shader/pipeline/fence) |
| `src/shader_validator.rs` | Strict-mode SPIR-V validator (six independent layers; see §11 of the spec) |
| `src/shader_annotate.rs` | `OpLoopMerge` MaxIterations injector (slangc workaround) |
| `src/shader_cache.rs` | Disk + in-mem LRU shader cache (warm path for `OP_GPU_SHADER_RESOLVE`) |
| `src/shader_inspect.rs` | Diagnostic SPIR-V dump |
| `src/bin/main.rs` | The host daemon |
| `src/bin/shader_tool.rs` | `aqueduct-shader-tool` CLI (5 subcommands) |
| `scripts/atrium-pkg-precompile-hook.sh` | Reference install-hook script (`atrium-pkg.md` §3.6.5) |
| `tests/end_to_end.rs` | Real-socket integration tests (10) |

## Binaries

### `aqueduct-gpu-host`

The daemon. Listens on a Unix socket for guest-side
`aqueduct-gpu-client` connections; routes ops through one of three
backends.

```sh
aqueduct-gpu-host [--socket /tmp/aqueduct-gpu.sock]
                  [--backend stub|software|moltenvk]
```

`moltenvk` falls back to `software` if the Vulkan loader isn't
installed.

### `aqueduct-shader-tool`

Developer tooling and atrium-pkg install-hook engine. Five
subcommands:

| Subcommand | Purpose |
|---|---|
| `check <FILE>` | Validate a single SPIR-V module. Exit 0/1. |
| `inspect <FILE>` | Diagnostic dump: caps, extensions, entry points, loops, decorations. |
| `annotate --max-iters N <FILE>` | Inject `MaxIterations \| N` into bare `OpLoopMerge` instructions. Works around slangc 2026.8 silently dropping `[MaxIters(N)]`. |
| `verify-bundle <DIR>` | Parse `manifest.json`; validate every referenced shader. |
| `precompile [--cache DIR] [--backend NAME] <DIR>` | Recursive walk + validate + populate cache. |

Run `aqueduct-shader-tool --help` for the full flag set.

## The shader-tool pipeline

The expected flow for a third-party Vulkan app's package install:

```
slangc shader.slang → shader.spv               (compile)
  ↓
aqueduct-shader-tool annotate --max-iters N    (inject bound)
  ↓
aqueduct-shader-tool verify-bundle <DIR>       (validate against manifest)
  ↓
aqueduct-shader-tool precompile <DIR>          (cache populate)
  ↓
At runtime: OP_GPU_SHADER_RESOLVE hits the cache; no SHADER_UPLOAD round-trip.
```

`scripts/atrium-pkg-precompile-hook.sh` is an executable reference
implementation. See [`docs/spec/atrium-pkg.md`](../docs/spec/atrium-pkg.md)
§3.6.5 for the install-time contract.

## The validator (six layers)

All described in detail at
[`docs/spec/aqueduct-gpu.md`](../docs/spec/aqueduct-gpu.md) §11.

1. **Forbidden capabilities** — `PhysicalStorageBufferAddresses`,
   `RayTracing*`, `MeshShading*`, `CooperativeMatrix*`, `Kernel`.
2. **Forbidden extensions** — `SPV_KHR_physical_storage_buffer`,
   ray-tracing extensions, mesh-shader extensions.
3. **Forbidden storage classes** — `PhysicalStorageBuffer` pointers.
4. **Bounded loops** — every `OpLoopMerge` must carry a literal
   iteration bound (`MaxIterations` etc.). Strict mode; the annotate
   step injects the bound for slangc/glslang/dxc output.
5. **Descriptor-binding coverage** — every resource-class
   `OpVariable` must declare both `DescriptorSet` and `Binding`.
6. **Entry-point / capability cross-check** — every `OpEntryPoint`'s
   required `Capability` must be declared. `Kernel` execution model
   rejected outright.

Plus DoS caps: 16 MiB module size, 256K instructions, 64 loops,
1024 functions, 1<<24 max iteration literal.

## Testing

```sh
cargo test                # 83 unit + 10 e2e socket tests
cargo run --example demo -p fresco-aqueduct-bridge   # PNG artifact
./scripts/atrium-pkg-precompile-hook.sh ../bundles/atrium-text  # install hook
./target/debug/aqueduct-shader-tool verify-bundle ../bundles/atrium-core
```

## Goes away in D5+

This daemon is bring-up infrastructure for macOS-HVF dev. On D5+
(native FreeBSD GPU drivers) the wire stays the same, but
guest-side `aqueduct-gpu-client` talks directly to the atrium-gpu
kmod over `IOC_GPU_*` ioctls. The host daemon stays around as a
CI / dev mode but isn't in the data path.

See [`docs/spec/aqueduct-gpu.md`](../docs/spec/aqueduct-gpu.md) §6.5
for tier semantics, §9 for phase planning.

## Trust posture

This daemon is a privileged mediator — it has full visibility into
every connected guest's GPU memory. Per
[`docs/spec/aqueduct-gpu.md`](../docs/spec/aqueduct-gpu.md) §12.1
that's the explicit trust boundary: if the host endpoint is
compromised, the platform is compromised regardless of wire crypto.
Mitigation is to keep this code small, auditable, and held to the
same scrutiny as a kernel component.

The shader sandbox (§11; this crate's `shader_validator`) is the
boundary the host endpoint enforces on *untrusted* SPIR-V from
guest apps.
