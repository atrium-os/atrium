# Toolkit backends — apps for free via the backend-multiplier

Status: strategy (settled direction 2026-06-17). Complements Pergola (Atrium's own
toolkit) — this is about inheriting *existing* app catalogs.

## 1. The thesis

Most GUI toolkits abstract windowing + rendering behind a **backend interface**
(GTK's GDK backends, Qt's QPA, winit's platform layer, SDL's `SDL_VideoDriver`,
Dear ImGui's platform+renderer backends). Implement that interface **once** against
Atrium (libatrium / fresco-client + the Tier-2/Carillon GPU path) and **every app
built on that toolkit runs unmodified**. One backend, a whole catalog. This is how
Wayland, winit, and SDL each bootstrapped their app ecosystems; Atrium does the same
without writing the apps.

Pergola remains Atrium's *native* toolkit (best integration, retained-scenegraph,
server-side render). Toolkit backends are the *import* path for the existing world.

## 2. The license filter (permissive only)

Per `LICENSING-POLICY.md`, the runtime ships permissive licenses; an app linking an
LGPL/GPL toolkit inherits those obligations, which kills the "anyone ships a
proprietary app" thesis. So:

- **Excluded:** GTK (LGPL), Qt (LGPL/commercial), **Slint** (GPL / commercial /
  royalty-free — not BSD/MIT-permissive; dropped 2026-06-17, was the old D5 plan).
- **In (truly permissive):**

| toolkit | license | brings | render output |
|---------|---------|--------|---------------|
| **winit + wgpu** | Apache-2.0 / MIT | the Rust GUI+game ecosystem — egui, iced, Bevy, anything on winit+wgpu | wgpu → Vulkan → Tier-2 / Carillon |
| **SDL2 / SDL3** | Zlib | a large C/C++ games + media catalog | SDL_Render → GL/Vulkan → Tier-2 / Carillon |
| **Dear ImGui** | MIT | dev tools, debug/inspector UIs | draw lists → Tier-2 (SW Vulkan) |
| **Servo / WebRender** | MPL-2.0 | web content as first-class apps (the old D6 play; weak-copyleft, file-level — acceptable) | WebRender → Vulkan → Tier-2 / Carillon |

## 3. Architectural fit (why this drops onto what we already have)

These toolkits **render their own pixels** — they are **Tier-2/3 self-composite
clients** (the games path in `reference render-paths`), not retained-scenegraph
apps. So a backend needs exactly two things, both of which Atrium already provides:

1. **A window + input + lifecycle** — `libatrium` (C toolkits: SDL, ImGui) or
   `fresco-client` (Rust: winit). `atrium_window_open` / `window_create`,
   `poll_event`, present. The 2026-06-17 client unification means a backend targets
   **one** scene-graph client, not three.
2. **A surface to present the toolkit's rendered output** — the toolkit's
   GL/Vulkan/SW renderer targets:
   - **Tier-2** (`atrium-vk-icd`, the bespoke SW Vulkan ICD) — works **today**, no
     host GPU. The path for ImGui and any software-rendered toolkit, and the
     CI/dev path for the rest.
   - **Carillon → MoltenVK/Metal** (or native GPU on bare metal) — for real GPU
     perf. Gated on finishing the Carillon graphics coverage (vertex/index buffers,
     indexed draws, texture sampling — the `moltenvk.rs:1235` catch-all; see
     `carillon.md` §12a). Until then, GPU toolkits run on Tier-2.

So no new transport is needed — toolkit backends ride the same Tier-2/Carillon
present path as native games. The work per toolkit is the *backend shim* (map its
window/input/surface calls onto libatrium/fresco-client + Tier-2/Carillon).

## 4. Delivery vector: source ports, not Linux binaries

Do **not** run Linux binaries (that needs linuxulator + drags in the Linux ABI
shape the charter rejects — evdev, etc.). Instead, **`insula` (the source/ports
tool, Homebrew/ports-style) recompiles permissive apps from source for
FreeBSD/aarch64 against the Atrium backend.** An egui app swaps its winit/wgpu
backend feature for the Atrium one and rebuilds — native, permissive, no compat
layer. `opifex` then installs the resulting bundle. This is the "apps for free"
loop, charter-clean. (`insula` itself is to be (re)built Atrium-native; see
`atrium-bundle-format.md` §7.)

## 5. Recommended order

1. **winit + wgpu** — highest leverage, charter-pure, Rust-native (trivial source
   ports), rides the Carillon/Tier-2 path we're already building. One backend
   unlocks egui + iced + Bevy at once.
2. **Dear ImGui** — the easiest end-to-end *proof* of the multiplier: immediate-mode,
   renders on Tier-2 **today** (no Carillon dependency). Good first validation.
3. **SDL** — the biggest catalog (games/media), but wants the Carillon GPU coverage
   first for non-trivial titles.
4. **Servo/WebRender** — the web-content-as-apps play; largest undertaking.

## 6. Dependencies / open items

- **Tier-2 (`atrium-vk-icd`)** must accept a real toolkit's Vulkan (it's the bespoke
  SW ICD; verify coverage as the first GPU toolkit lands).
- **Carillon graphics coverage** (`moltenvk.rs:1235`) for GPU-accelerated toolkits.
- **`insula` source/ports tool** — the recompile-from-source delivery vector.
- A **vertex/triangle-mesh path** question: ImGui/egui emit arbitrary textured
  triangles, which don't map to Fresco's rect/path/texture/glyph scene nodes — they
  go through the toolkit's *own* Vulkan renderer onto Tier-2/Carillon and present the
  finished surface to frescod (self-composite), NOT through the scene graph. That's
  the correct boundary: scene graph = native (Pergola); self-composite = imported
  toolkits + games.
