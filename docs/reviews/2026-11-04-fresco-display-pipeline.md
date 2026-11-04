# Fresco display pipeline: where it stands and what it needs

> 2026-11-04. Investigation and gap analysis, no code changes. Read against
> `docs/GRAPHICS-STACK.md`, `spec/atrium-display-architecture.md` (with its
> v2.1 amendments, §7.1), `spec/atrium-gpu-abi-v2.md` §7 and §18,
> `reviews/2026-11-03-gpu-kernel-and-tier2-review.md` §A4/§A5/§B4–B5,
> `spec/atrium-rdna4-vulkan-backend.md` §7, and gpusim at `f3f3455`
> (`engine/src/display.rs`, `qemu-device/gpusim.c`, `engine/src/render.rs`).

## 1. Summary

The display pipeline has two halves that can move independently:

- **Presentation**: getting a finished frame on screen. Kernel display module,
  display ABI, buffer handoff, fences, pacing. Today this works on gpusim, but
  in the "interim" shape: one scanout buffer, raw VRAM offsets, no fences, no
  exclusivity, and frescod ignores the vblank it has.
- **Rendering**: who draws the composited frame. Today that is Mesa
  **lavapipe** (CPU) through `fresco-vulkan::HeadlessRenderer`, followed by a
  full-frame CPU readback and a DMA copy into VRAM. The GPU never draws the
  desktop.

Getting "2D via Fresco on our own driver" means fixing presentation (all on
gpusim, mostly small, specified already) and moving rendering onto our own
GPU path. That second step has a long route and a short route:

- **Long route**: wait for the RDNA4 backend's graphics milestone (NGG vertex
  stage, rasterizer, blending; backend M4) plus a GFX12 graphics model in
  gpusim, which doesn't exist yet. gpusim's only draw path is a SIM-only
  fixed-function rasterizer (`render.rs` `DrawParams`, `DRAW_*` registers).
  Then `tier3_rdna4` in `aqueduct-gpu-host` (backend M5, not started) and the
  ICD hop.
- **Short route**: a **compute-only Fresco renderer** that rasterizes the four
  2D ops straight into the linear BGRA scanout buffer. The compute half of the
  RDNA4 backend already passes all 37 oracle jobs on gpusim and the R9700. The
  same shaders also run on Tier-2, whose compute path is its most mature. That
  removes the readback and copy, the ICD hop, and the dependency on M4.

My recommendation is to do presentation first (it's needed by every renderer)
and take the short route for 2D. M4 graphics stays on the RDNA4 thread's plan
for third-party apps and Orbis, without blocking the desktop.

Real silicon display (DCN 4 on the R9700) is a separate, large item. It needs
native Atrium boot on the box, and very likely a monitor or dummy plug. §6
covers it.

## 2. What exists today

### 2.1 Frame path (frescod main)

```
clients ─fresco-protocol─▶ EnvelopeFrontend → Compositor (z-order, per-window scene)
  render_one_frame: merge rect/path/texture/glyph nodes of all windows
  HeadlessRenderer (Vulkan, lavapipe): per op, compute expand → instanced quad draw
      (TRIANGLE_STRIP, src-over blend, B8G8R8A8_SRGB attachment)
  read_pixels → CPU Vec (full frame)
  Scanout::update_deadline: BO_WRITE into System staging BO → PM4 DMA_DATA → VRAM BO
  Display::page_flip(vram_offset, size, vsync=1)
  pacing: damage-driven (RedrawSignal), capped at 30 fps by thread::sleep
```

`frescod/src/main.rs`, `fresco-vulkan/src/headless.rs`, `atrium-gpu-rs/src/amd.rs:430-483`.

### 2.2 Kernel display module

`atrium-gpu-amd/display/display_dev.c` (508 lines) on `atrium_display_abi.h`:
`ENUM` (HPD, connector type, 128-byte EDID read byte by byte over the modeled
DDC), `MODES` (decodes the four DTDs), `SET_MODE` and `PAGE_FLIP` taking
`{vram_offset, size}`, `STATUS` (vblank count, dropped flips, tear line), plus
test knobs (`CONFIG`, `USBC`, `MST`, `DPTRAIN`). Vblank comes through the
shared IH ring (cause VBLANK) to a kqueue `EVFILT_READ` knote with `EV_CLEAR`.
It has reset prepare/restore hooks and an energy-federation member
(`display0`, PSR via posture/budget).

Every register it touches is a **SIM-only** `regDISP_*` register
(`atrium_gpu_amd.h:126-156`), not DCN.

### 2.3 gpusim display model

`engine/src/display.rs` (1,589 lines) is solid and matches the architecture
doc: picosecond virtual time, analytic beam position, per-scanline FB
selection (exact tear lines), depth-1 flip queue with drop-and-count, vsync
latch, EDID over DDC, HPD, connector types, link bandwidth, MST, DP training,
PSR power. It also has `flip_fenced(fb, vsync, fence_ready_ps)`, but no
register drives it.

The QEMU device pulses vblank from a ~60 Hz host timer and blits the scanout
buffer to a QEMU console each vblank (`gpusim.c:340-436`, a bulk `C_SCANOUT`
read). So you can see pixels.

The model knows one plane, linear 32-bit BGRA, pitch = width × 4. It has no
format, pitch, tiling, cursor or overlay. The register layer counts any
non-zero `FB_SIZE` as "resident".

### 2.4 The other compositor

`frescod-aqueduct` (legacy virtio `'G'`, Tier-1 tiny-skia) has a triple-buffer
ring, vblank pacing, queued flips and the three-level skip hierarchy
(per-screen flip skip, per-window raster skip, damage scissor). frescod main
has none of these.

## 3. Gaps

### 3.1 Presentation (kernel, ABI, frescod)

| # | Gap | Where | Effect |
|---|---|---|---|
| P1 | **One scanout buffer.** frescod DMAs each new frame into the same VRAM BO that's being scanned out, then "flips" to the same offset | `main.rs` `Scanout::new` once; `amd.rs:430` | Content tears mid-copy whatever the vsync setting; the flip is a no-op |
| P2 | **Scanout by raw VRAM offset.** Userspace `{vram_offset,size}` goes straight to `FB_BASE/FB_SIZE` | `display_dev.c:135`, `BO_EXPORT_SCANOUT` | Any opener can scan out any VRAM (isolation break 4 in the review). v2.1 §7 retires it for `bo_fd`/`share_fd` |
| P3 | **No exclusivity or lease.** `display_open` returns 0 for anyone; the node is 0600 root | `display_dev.c:145` | v2.1 wants exclusive open by the broker, `EBUSY` otherwise, per-plane leases |
| P4 | **No fences.** No in-fence (wait for render) or out-fence (old buffer released) | ABI | Can't pipeline render N+1 with scanout N safely; the model's `flip_fenced` is unused |
| P5 | **No flip-done event.** kqueue only carries vblank count | `display_dev.c` filter | Compositor can't tell when a flip latched or which buffer is free |
| P6 | **frescod ignores vblank.** Paces with `thread::sleep` at 30 fps | `main.rs` | Judder against a 60 Hz scanout, and half the refresh rate |
| P7 | **No buffer description.** No format, pitch or modifier in the ABI or the model | ABI, `display.rs` | Blocks tiled render targets (RDNA4 swizzle modes) and any mode whose pitch isn't width × 4 |
| P8 | **No mode choice.** `SET_MODE` always takes the EDID's preferred timing | ABI | Fine for now; atomic commit should name the mode |
| P9 | **No hold-last-frame or reset re-apply.** Restore only re-arms the vblank IRQ | `display_dev.c:216` | v2.1 §10.2: re-apply last mode and planes after a reset, post `NOTE_RESET` |
| P10 | **No `display_abi_v2.h`.** v2.1 says the display gets its own header with the GPU header's rules (`struct_size` first, `int32_t` fds) | `include/atrium/` | The commit ioctl has no home yet |
| P11 | **frescod runs as root** for `/dev/atrium-gpu0` | `etc/jaild.policy.toml:254` | Contradicts the `_frescod` uid and devfs rules |
| P12 | **Skip hierarchy and multi-buffering only on the legacy compositor** | `frescod_aqueduct.rs` | Need porting to main before retiring `'G'` |

### 3.2 Rendering

| # | Gap | Effect |
|---|---|---|
| R1 | **lavapipe at runtime** (Mesa, plus an LLVM mount in frescod's jail) | Against the no-Mesa decision; Tier-2 replacement is decided but not done (review §B5: bundle parity for texture, path and glyph; sRGB clear and BGRA store bugs; build hygiene; CI) |
| R2 | **Full-frame readback + copy every frame** | Wastes a real GPU; also slow on gpusim, where every VRAM access is a socket round trip |
| R3 | **No GPU path for frescod.** No Vulkan driver targets `'A'`, `tier3_rdna4` doesn't exist, and the RDNA4 backend has no graphics stage yet | The GPU never draws the desktop |
| R4 | **gpusim can't run GFX12 graphics.** Its RDNA4 core runs compute; draws use a SIM-only fixed-function rasterizer | Even with backend M4, there's nothing on gpusim to run it on. A GFX12 graphics model (NGG/primitive export, PA/SC raster, SPI interpolation, CB blend and export) is a new gpusim project |
| R5 | **Texture slots collide across windows** (`main.rs` header) | Multi-window apps with textures misrender |

### 3.3 Real silicon

| # | Gap |
|---|---|
| S1 | The display module programs only SIM registers. Real RDNA4 display is DCN 4.0.1 with DMUB firmware (loaded through PSP), HUBP/DPP/MPC/OPP/OTG pipes, SMU-controlled clocks, DP/HDMI link bring-up. amdgpu's `dc/` is the reference for the size of this, and it is very large |
| S2 | The R9700 runs Linux with amdgpu owning the device. Our kernel driver has never touched real hardware; the oracle runs there use amdgpu's libdrm (`atrium-rdna4-oracle/runner/amdgpu_runner.c`) |
| S3 | Whether the R9700 has any display sink attached is unknown. A headless cloud box usually has none, and DCN won't light a pipe without one (short of a dummy plug) |

## 4. The compute-only Fresco renderer (short route)

Fresco's four ops are already "compute expand, then a trivial instanced quad
with a per-pixel fragment shader": rounded-rect SDF, path segment, textured
quad, glyph quad sampling an R8 coverage atlas. All blend src-over into an
sRGB target. That maps directly onto a compute rasterizer:

1. **Bin**: one compute pass assigns each node's bounding box to 16×16 screen
   tiles (atomics plus a prefix sum; both pass on the R9700 today).
2. **Raster**: one workgroup per tile walks its node list in z-order,
   evaluates coverage per pixel (the same SDF/sample code as the fragment
   shaders), blends in registers, applies linear→sRGB, and writes BGRA8 once.
3. **Target**: the output is a storage *buffer* that is the scanout BO
   (linear, pitch = width × 4, which gpusim and DCN both scan out). No storage
   images needed, so it doesn't wait on that G4 gap.
4. **Textures and glyphs**: sample from buffers with manual nearest or
   bilinear, or use image sampling once backend M3 lands.
5. **Damage**: only tiles touched by the damage rect are dispatched, which
   gives the skip hierarchy (P12) almost for free.

What it buys:

- Runs on **what already works**: GIR + RDNA4 compute on gpusim and the
  R9700, and Tier-2 compute on CPU. **One renderer for every tier**, with
  lavapipe kept as a pixel-diff oracle.
- **Zero copy**: the GPU writes the scanout buffer directly. R2 is gone.
- **No ICD hop** for the compositor. frescod can drive `atrium-rdna4-cmd`
  and the compiler directly through `atrium-gpu-rs` (review §B5 item 6
  already suggested the compositor skip the ICD).
- **R9700 verification now**, under Linux: the oracle runner can execute a
  Fresco frame and read it back for a pixel diff against Tier-2. That checks
  "2D renders correctly on real RDNA4" without our display driver being on
  the box.

What it costs:

- A second renderer next to `HeadlessRenderer` (which stays as the Vulkan
  reference and the lavapipe and Tier-2 oracle path).
- Paths with many overlapping, large antialiased nodes need the binning to be
  decent. Vello and piet-gpu show this works well; Fresco's op set is much
  smaller.
- It doesn't exercise the graphics pipeline, so Orbis and third-party Vulkan
  still need backend M4. The RDNA4 thread keeps that.

## 5. Proposed work, in order

**Phase D1: presentation on gpusim** (kernel + ABI + frescod; independent of
the renderer; I'd start here)

1. `include/atrium/display_abi_v2.h`: `struct_size`-first structs; one
   `COMMIT` ioctl (`TEST_ONLY` flag) naming CRTC → mode, primary plane →
   `bo_fd` + format + pitch + modifier, optional in-fence
   `(syncobj_fd, value)`, out-fence `(syncobj_fd, value)`; kqueue notes for
   vblank, flip-done and hotplug.
2. Display module: resolve `bo_fd` to the BO through the GPU module (a small
   base-module hook, since the two modules share the softc), check VRAM
   residency, size, pitch and format, pin while latched, release after flip
   away. Exclusive open (`EBUSY`). Retire `BO_EXPORT_SCANOUT` and the offset
   ioctls.
3. gpusim: registers for pitch and format, the in-fence wired to
   `flip_fenced`, a flip-done IRQ, and a real VRAM range check in the
   register layer.
4. frescod: two or three scanout BOs, render into the back one, commit with
   fences, pace on kqueue vblank and flip-done at the mode's real refresh.
   Port the per-screen flip skip.
5. Hold-last-frame on close and re-apply after reset (v2.1 §10.2), with an
   engine test for each.
6. Engine and in-VM tests: exact tear line with vsync off, none with it on,
   no tearing from the new frescod loop, flip waits on its in-fence.

**Phase D2: Tier-2 under frescod** (review §B5 steps 1–4, with lavapipe as
the CI oracle). Removes Mesa from the runtime. If D3 is chosen, D2 shrinks to
"Tier-2 runs the compute renderer," which is its strongest path anyway.

**Phase D3: compute Fresco renderer on RDNA4** (§4)

1. Write the bin and raster compute shaders for rect, path, texture and glyph;
   golden images against `HeadlessRenderer` on lavapipe.
2. Run them on Tier-2 (CPU) inside frescod.
3. Compile them with GIR → RDNA4; run as oracle cases on gpusim and on the
   R9700 (read back and pixel-diff).
4. frescod submits them through `atrium-gpu-rs` on `'A'`, writing the
   scanout BO from D1. First GPU-drawn desktop, on gpusim.

**Phase D4: gpusim fidelity toward DCN** (review §A5 applied to display)

Replace the `regDISP_*` block with the real DCN 4 registers for the subset
we use: HUBP primary surface address, pitch and swizzle; the flip and update
lock; OTG vblank and vline interrupts with real IH source ids. That way the
display module written against gpusim is the code that runs on silicon.
Use the MIT-licensed amdgpu register headers (`dcn_4_*`), as the review
recommends.

**Phase D5: real silicon** (after the x86_64 port and native boot on the box)

1. **Inherit the firmware mode first.** The card's UEFI GOP driver lights the
   attached monitor at boot. Our module takes over only the surface address,
   flips and vblank interrupt, with no modeset. That's a small, achievable
   first light, and the boot framebuffer path (`atrium_bootfb`) is the
   fallback.
2. Full DCN modeset (DMUB, clocks, link training) later, as its own arc.

## 6. Open questions for you

1. **Short or long route for GPU 2D?** Compute-only Fresco renderer now, or
   wait for the RDNA4 graphics pipeline (M4) plus a gpusim GFX12 graphics
   model? I recommend the compute route; it changes what the RDNA4 thread
   needs next (buffer and atlas sampling before NGG), so I'd tell that thread.
2. **Is anything plugged into the R9700's outputs?** Real display testing
   needs a monitor or a DP/HDMI dummy plug, plus native Atrium boot. Until
   then the R9700 can only check rendering, not scanout. A read-only
   `cat /sys/class/drm/card*-*/status` on the next R9700 run would answer it.
3. **Should D1 start now?** It's all on gpusim, it's needed whatever the
   renderer, and it closes isolation break 4 from the kernel review.
