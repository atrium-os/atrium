# Fresco F4: per-window renderers and the broker

*Plan, 2026-11-04. Implements condition F4 of
`docs/spec/atrium-navigator-threat-model.md` §2.5.4: §2.5.1 (the compositor
writes data, the broker runs only our kernels) and §2.5.2 (one jailed renderer
per window, no process that sees two). F2, the kernel fuzzing it relies on, is
done (atrium 9323062).*

## What F4 must end with

| process | holds | parses hostile input | sees |
|---|---|---|---|
| **broker** (today's frescod, slimmed) | GPU control and VMs, display, input devices, WM socket, surface heap, layer list, chrome | nothing: fixed-format records only, plus `Frame::validate` on each renderer's frame | the layer list; surfaces on the GPU side only |
| **renderer**, one jail per client connection | the client socket (handed over by the broker), its own shm scene buffers, its own texture and glyph caches | the client's scene graph, textures, fonts | its own windows' content and its own input events |

A compromised renderer reaches only what its own client already had.

## Slices

### F4a. Per-window surfaces and the compose pass. Done.

- `compose.comp` stacks window surfaces into the screen by a layer list (atrium a20492f). A layer has a rect, an origin for scrolling, a surface in a heap, and an opacity.
  - It has a Rust reference, and runs on Tier-2, on the GIR interpreter and on RDNA4.
  - It is fuzzed against the reference, exact on Tier-2, GIR and gpusim. The gpusim runs place every buffer right before an unmapped page.
- Surfaces carry alpha, with premultiplied linear RGB. Each window's surface clears to transparent, so pixels a window leaves undrawn still show what is under it, as they did with the merged scene.
- frescod's `compute` and `gpu` backends now raster each window into its own surface (only damaged tiles), then compose. On the GPU, compose runs only where a surface changed since that scanout buffer was last composed, plus the old and new rects of a layer that moved. It covers the whole screen only when windows appear, disappear or change order. `vulkan` still merges the windows into one scene.
- Verified in the amd64 VM on gpusim with `atrium-surfaces-demo`: three overlapping windows, one with no background, one translucent and moving (screenshot `frescod-f4-surfaces-compose.png` in the project files). Moving the 480x300 window recomposes about 490x300 pixels, about 2.4 s a frame on gpusim, against 17 s for the full screen.
- frescod no longer allocates a BO every frame. The driver's per-VM VA is a bump allocator that never reuses ranges, so frescod ran out of VA (ENOSPC) after a few thousand frames.
- Window positions snap to whole pixels in compose.
- Content outside a window's size is now clipped. A window with no size yet gets the screen's.

### F4a′. Events go to their window's owner. Done.

Before this change, frescod's event fan-out sent every event to every connection: keystrokes, pointer, focus and window events. A client could read another client's typing by ignoring the window id. Now:

- An event reaches the owner of the window it names, plus the WM.
- Keys with no focused window reach the WM alone.
- Pointer events over window 0, the shared background, still reach every client, until the background gets an owner (F4c).

### F4b. The process split

Done, renderer jailed (atrium a46ec90 and the commit after it). `FRESCOD_SPLIT=1` hands every connection on `fresco.sock` to its own `fresco-renderer`; the WM socket stays in the broker. The renderer's stdin is the record channel, and the client's socket arrives over it in the first record (`CLIENT`), because a jail from `portcullis exec` gets only its three stdio fds. `FRESCOD_RENDERER_LAUNCHER` runs it confined, e.g. `portcullis exec --user <unprivileged> --instance fresco{client} org.atrium.fresco.renderer` (a signed bundle with no capabilities).

Verified in the amd64 VM with `atrium-surfaces-demo`:
- Unconfined: headless on Tier-2, and on gpusim with the GPU renderer (screenshot `frescod-f4b-split-gpu.png`).
- Jailed: the renderer ran in its own jail as an unprivileged user, and drew all three windows (`frescod-f4b-jailed-renderer.png`). A probe bundle in the same jail configuration saw only its own tree, a /dev of null, zero, random, urandom, stdio and fd, no /etc, no GPU, display or HID node, and no network (connections to the host and to loopback refused).
- When the client quits, or frescod is killed, the renderer exits and its jail and mounts are gone.

How it works:
- Records and fds go over `fresco_split::chan` (SOCK_SEQPACKET; a record of the wrong size, a truncated one, or one with too many fds is refused).
- Slots are memfds the broker creates sealed against shrinking and growing, so a renderer cannot truncate one under the broker's mapping; a renderer refuses slots that are not sealed.
- The broker sends `GEOMETRY` whenever the WM's size for a window differs from the last one it sent, and accepts only frames of that size. A frame drawn at the old size is dropped and the renderer draws again.
- Retiring a renderer closes its channel first, so `portcullis exec` can tear its jail down; a kill after 5 s is only the backstop.

Known gaps:
- Fonts: the renderer's server-side text looks in `/usr/local/share/fonts`, which a jail does not have. The bundle must carry the fonts (not done yet).
- Window 0 events do not reach renderers (F4c). A client drawing on window 0 draws only in its renderer's own window 0, which the broker does not show.
- `FRESCOD_SPLIT` is off by default, and the Vulkan backend does not take pre-packed frames.

Plan as written:

1. **Hand-off, not parsing.** The broker accepts on `fresco.sock` and reads the peer's credentials (`getpeereid`, as today). It then spawns a renderer for the connection through `portcullis exec`, with no filesystem, no network, and no GPU node in its devfs ruleset. It passes the client fd and one broker channel over `SCM_RIGHTS`. The broker never reads a client byte.
2. **Broker ⇄ renderer protocol.** Fixed-size records only, so it can be fuzzed (F3).
   - Renderer to broker: `WINDOW_CREATE(w, h, role, title[64])`, `WINDOW_RESIZE`, `WINDOW_DESTROY`, `FRAME_READY(window, slot)`.
   - Broker to renderer: the shm fds for a window's two frame slots, window-local input events, focus, resize, close.
3. **Scene buffer.** Each window has two shm slots of fixed capacity holding the packed `Frame`: header, ops, texture table, texels. On `FRAME_READY` the broker validates the header against the slot (`Frame::validate`), then stages it as today.
   - Zero-copy, importing the shm as a System BO in the window's VM, comes with F4e.
4. **What moves.** `EnvelopeFrontend`, the CAS store, slots, fonts and text all move into the renderer.
   - The broker keeps `Compositor`'s window table: ids, owner, position, size, z, focus, rendering. It also keeps the WM socket, the HID readers and the frame loop.
   - `WM_ENUMERATE` answers from the broker's table, using the title the renderer declared.
5. **Fallback.** `FRESCOD_SPLIT=0` keeps today's single process until the split is proven in the VM.

### F4c. Input routed by the layer list

- The broker hit-tests the pointer against the layer list it composes, so what is on top really is what gets the click.
- The renderer receives events already in window coordinates and forwards them to its client.
- Window 0 gets an owner, the desktop shell, so the broadcast exception above ends.

### F4d. Trusted chrome

- The broker draws its own scene: the origin indicator, the focus ring, and secure-input affordances. It runs the same `bin` and `raster` on a scene the broker builds itself, into a chrome surface that is always the top layer.
- The Navigator gives the origin to the broker over its own channel, never through the renderer.

### F4e. One GPU VM per window (waits on F1)

- Each window's `bin` and `raster` run in a VM that maps only that window's data and surface BOs.
- Compose runs in a VM that maps every surface read-only, plus the scanout. The surface heap becomes per-window BOs mapped back to back there.
- This needs the driver to map one BO into two VMs, and the isolation breaks of the 11-03 review fixed first.

### F4f. Secure input for passwords (waits on F5 and Navigator C0)

- A password field is marked in the window's semantic tree, and the broker validates the mark (§6.4).
- Keystrokes while that field has focus go to the broker's request builder, never to the renderer. The renderer is told only how many bullets to draw.

## Order

F4b, then F4c, then F4d. F4f follows once F5 and the vault exist. F4e follows F1.

## Decisions taken (recommended; say if you want otherwise)

- **One renderer per client connection, not per window.** An app's windows are one trust domain, and the Navigator opens one connection per document, so for web content this is per document anyway. It also avoids moving one client's windows across processes.
- **Client ids.** Client ids were a wrapping `u8` counter, so after 255 connections a new client could get the id of a live one, and with it that client's windows. Fixed alongside F4a′: a new connection takes a free id, and is refused if all 255 are in use. F4b replaces them with broker-issued 32-bit ids.
