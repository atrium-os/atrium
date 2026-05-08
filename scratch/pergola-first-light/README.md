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

| Issue | Cause |
|---|---|
| Heading "Sign in to Atrium" missing (size 22) | fresco-side atlas-page allocation behaves oddly — only the subhead's size-13 page renders. |
| Username/password placeholder text + button label "Sign in" missing (all size 15) | Same. Subhead is the only text rendered; everything else of size 22 or 15 is silent. |
| Password TextField background (id=7) missing | One Rect inexplicably absent — same code path as the visible username Rect at id=5. |
| Window position not centered | `frescod-vulkan-smoke` doesn't composite, just dumps the raw 1280×720 render target. A real WM would center the window. |
| Real scanout via `frescod` (not smoke) fails | Kernel log shows `RESOURCE_CREATE_BLOB` timeout — fresco→kmod→QEMU integration issue, separate from Pergola. |

The remaining text-rendering anomalies need fresco-server debugging
(probably in `fresco-scene-server/src/text.rs`'s `shape_text_run`
or atlas-page allocator) — the LogSurface + smoke log confirm
Pergola sends everything correctly; the rendering layer drops some
glyph runs.

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
