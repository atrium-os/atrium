# fresco-poc — proof-of-concept for the Fresco rendering stack

Validates the architecture in [`docs/spec/fresco-rendering-stack.md`](https://github.com/atrium-os/atrium/blob/main/docs/spec/fresco-rendering-stack.md):

- **Top contract**: scenegraph protocol over atrium-rpc envelope (display class)
- **Middle**: SPIR-V bundle (atrium-core) loaded into fresco-server
- **Bottom**: Vulkan + SPIR-V via vendor driver (MoltenVK on macOS during POC,
  vendor Vulkan on FreeBSD VM later)

The POC's success criteria: send Scene B (one 4MB texture × 100 instances)
over the wire; observe one upload, one vkImage, 100 indirect-instanced draws,
correct pixels (verified vs tiny-skia).

## Layout

```
crates/
  atrium-rpc-display/   display dictionary on opcode_class = 1
                        (control ops: SLOT/FRAME/NODE; scene op IDs)
  fresco-bundle/        manifest format + loader + dispatch table
  fresco-vulkan/        Vulkan setup; per-frame compute + render
  fresco-server-poc/    binary; opens a window, listens on UDS
  fresco-test-client/   binary; emits Scene A and Scene B
bundles/
  atrium-core/          SPIR-V bundle for rect + texture ops
```

## Build

```sh
# Compile bundle GLSL → SPIR-V (one-time per bundle source change):
./bundles/atrium-core/build.sh

# Compile crates:
cargo build --release
```

The bundle's `.spv` files are gitignored — they're rebuilt from source.
Requires `glslangValidator` and `spirv-val` on `PATH` (`brew install glslang spirv-tools` on macOS, `pkg install glslang spirv-tools` on FreeBSD).

## Run

```sh
# Server (opens a window):
cargo run --release --bin fresco-server-poc

# Test client (in another shell):
cargo run --release --bin fresco-test-client -- scene-a   # 1000 rects
cargo run --release --bin fresco-test-client -- scene-b   # texture × 100
```

## What this POC does NOT exercise

- Tessera CAS (FreeBSD-only). atrium-rpc's in-process CAS is fully exercised.
- Engine compat bundles (UE/Godot). atrium-core is the only bundle; second-bundle
  composition is a nice-to-have step deferred to post-POC if time.
- Multi-app / per-app GPU contexts. One client at a time.
- Portcullis-jail integration. Server runs as a regular process.
- FreeBSD VM port. Done after architecture validation.
