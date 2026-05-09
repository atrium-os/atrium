# Pergola first light (2026-05-09)

Vestibulum running end-to-end against a live fresco-server instance
(via the `frescod-vulkan-smoke` socket binary, which renders frames to
PNG instead of using scanout — the real-window scanout path through
`frescod` proper hits a separate kmod-side `RESOURCE_CREATE_BLOB`
timeout that's unrelated to Pergola).

## What's pictured

- **`01-canvas-elevated-collision.png`** — first run. Canvas
  background (neutral_50) and elevated panel (neutral_50) shared the
  same color in light mode, so the panel is invisible. Only the
  subhead "Use your local account password." renders. Diagnosed as a
  bug in the visual-language token table.

- **`02-after-token-fix.png`** — after fix. `bg_canvas` in light mode
  changed to `neutral_100` (slightly darker than `bg_elevated`), so
  the panel reads correctly as raised. Username TextField background
  + Sign-in button background (Atrium amber-bronze, `accent_400`)
  are now visible.

- **`v4-perslot.png`** — after the fresco-vulkan multi-batch fix
  (per-atlas-slot dedicated buffers in `OpFrameResources::ensure_glyph_slot`).
  Heading "Sign in to Atrium" now renders. "username" placeholder
  visible. Sign-in button still rendered. TextField rect bgs and
  remaining size-15 strings still missing — server-side issues
  (see below).

- **`v6-stride-fix.png`** — complete render. Two more bugs fixed:
  (1) `extract_rect_nodes` / `extract_glyph_run_batches` now sort by
  `node_id` so HashMap iteration randomness no longer scrambles
  z-order (panel was overdrawing TextField bgs); (2) glyph_run
  compute kernel was reading scene nodes with stride 96 while Rust
  writes with stride 64 — every node beyond thread 0 in a batch
  was reading garbage offsets. Both fixed; all 11 scene nodes (5 rects
  + 5 text runs + 1 button label) now render correctly.

- **`v7-baseline.png`** — text baseline alignment. The wire `y` was
  being passed straight through to the kernel, which expected the
  baseline; Pergola was passing the top of the em-box (intuitive for
  layout / vertical centering). Result: every text element rendered
  with its baseline at the top of its container, leaving glyphs
  partly above. `shape_text_run` now adds the font's ascender to the
  incoming `y` so the on-wire convention is "top of em-box" and the
  GPU still sees a baseline. Placeholders sit cleanly inside their
  TextFields, button label centers on the button.

## What proved

- Pergola → fresco-client → fresco-server → MoltenVK → Metal →
  readback → PNG works end-to-end.
- Real anti-aliased text rendering (subhead) confirms the venus
  path executes the glyph_run shader through to the readback.
- Theme tokens flow through the wire correctly — `accent_400` arrives
  as the documented #9E6628 amber-bronze.
- `LogSurface` output during local test perfectly predicts wire ops:
  10 set_node calls in the right order, all reaching the server.

## What's not yet right

The full vestibulum login form renders end-to-end (`v6-stride-fix.png`).
Remaining items aren't Pergola/vestibulum bugs:

| Issue | Cause |
|---|---|
| Window position not centered | `frescod-vulkan-smoke` doesn't composite, just dumps the raw 1280×720 render target. A real WM would center the window. |
| Real scanout via `frescod` (not smoke) fails | Kernel log shows `RESOURCE_CREATE_BLOB` timeout — fresco→kmod→QEMU integration issue, separate from Pergola. |

## How this was produced

```sh
# Host: build (already done)
cd ~/src/bsd/vestibulum && cargo build --release --target aarch64-unknown-freebsd

# Host: ensure bundles are compiled (slangc → SPIR-V)
export PATH=/Users/girivs/src/slang-bin/bin:$PATH
cd ~/src/bsd/bundles/atrium-core && bash build.sh
cd ~/src/bsd/bundles/atrium-text && bash build.sh

# In-VM: start frescod-vulkan-smoke (PNG-output renderer, NOT the
# scanout daemon)
~/src/bsd/scripts/vssh "FRESCOD_BUNDLES_ROOT=/mnt/host/bundles \
    nohup /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod-vulkan-smoke \
    > /tmp/smoke.log 2>&1 &"

# In-VM: launch vestibulum
~/src/bsd/scripts/vssh "FRESCO_SOCK=/tmp/frescod-smoke.sock \
    /mnt/host/vestibulum/target/aarch64-unknown-freebsd/release/vestibulum"

# Pull the rendered frame
~/src/bsd/scripts/vssh "cp /tmp/frescod-smoke-frame-0000.png /mnt/host/vm/vestibulum.png"
open ~/src/bsd/vm/vestibulum.png
```
