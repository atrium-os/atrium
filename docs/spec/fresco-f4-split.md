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
- Pointer events over window 0, the shared background, still reached every client until F4c gave the background to the WM.

### F4b. The process split

Done, renderer jailed, and on by default (atrium a46ec90 and later). Split mode hands every connection on `fresco.sock` to its own `fresco-renderer`; the WM socket stays in the broker. The renderer's stdin is the record channel, and the client's socket arrives over it in the first record (`CLIENT`), because a one-shot jail gets only its three stdio fds.

Starting renderers (2026-11-05):
- frescod runs as `_frescod`, not root. For each connection it asks portcullisd itself (`ExecInstance` over the portcullisd socket, as `portcullis exec --daemon` does, so frescod's jail needs no CLI) for a one-shot jail of `org.atrium.fresco.renderer` (`FRESCOD_RENDERER_APP`), instance `fresco<handle>`, with `require_memory_limit`.
- The system policy's worker rule (portcullis.md §6.5.2g) runs it as `_fresco-render` with the bundle's 512 MiB / 100 % CPU caps. `scripts/fresco-renderer-bundle.sh` builds the signed bundle and sets up the users and the rule.
- It fails closed: if portcullisd is not there or refuses, the client is refused and logged; frescod never falls back to an unconfined renderer.
- Split mode is on unless `FRESCOD_SPLIT=0`, when the renderer can show renderer frames (`compute`, `gpu`). `compute` is the default renderer (2026-11-05); with `FRESCOD_RENDERER=vulkan` split mode stays off and frescod warns.
- `FRESCOD_RENDERER_LAUNCHER=<command>` replaces portcullisd with a command (`{client}` is the client id; the fuzz harness uses it), and `FRESCOD_RENDERER_LAUNCHER=unconfined` runs `FRESCOD_RENDERER_BIN` directly, for development.

Verified in the amd64 VM with `atrium-surfaces-demo`:
- Unconfined: headless on Tier-2, and on gpusim with the GPU renderer (screenshot `frescod-f4b-split-gpu.png`).
- Jailed: the renderer ran in its own jail as an unprivileged user, and drew all three windows (`frescod-f4b-jailed-renderer.png`). A probe bundle in the same jail configuration saw only its own tree, a /dev of null, zero, random, urandom, stdio and fd, no /etc, no GPU, display or HID node, and no network (connections to the host and to loopback refused).
- When the client quits, or frescod is killed, the renderer exits and its jail and mounts are gone.
- Native launch, frescod as `_frescod` with no launcher set: the renderer ran as `_fresco-render` in jail `app-org-atrium-fresco-renderer--fresco1` with `memoryuse:sigkill=536870912` and `pcpu:deny=100`, and the text demo drew. With portcullisd stopped the client was refused and no renderer started. A renderer stopped with SIGSTOP (so it ignores end-of-file) was gone 5 s after its client hung up: frescod hangs up on portcullisd, which kills the jail. `fresco-split-fuzz.sh 60 11` still passes.

How it works:
- Records and fds go over `fresco_split::chan` (SOCK_SEQPACKET; a record of the wrong size, a truncated one, or one with too many fds is refused).
- Slots are memfds the broker creates sealed against shrinking and growing, so a renderer cannot truncate one under the broker's mapping; a renderer refuses slots that are not sealed.
- The broker sends `GEOMETRY` whenever the WM's size for a window differs from the last one it sent, and accepts only frames of that size. A frame drawn at the old size is dropped and the renderer draws again.
- Retiring a renderer closes its channel first, so it exits and its jail goes. The backstop after 5 s: a child is killed; a portcullisd jail is ended by hanging up the `ExecInstance` connection, on which portcullisd kills the worker and removes the jail (portcullis.md §6.5.2g).

Known gaps:
- Fonts: the bundle carries them (`scripts/fresco-renderer-bundle.sh`), at the path the renderer's text looks in.
- A client drawing on window 0 draws only in its renderer's own window 0, which the broker does not show. Clients that want a desktop draw in a window of role Background, as forum-dock does.
- The Vulkan backend does not take renderer frames, so with it (no longer the default) split mode is off.
- frescod's services.d manifest (`frescod/etc/services.d/40-atrium-frescod.toml`) runs it as `_frescod` (65010) in its own jail. Run end to end through atrium-portcullisd-bootstrap in the amd64 VM (2026-11-05): frescod drove the gpusim display from jail `atrium-frescod`, a client's renderer started in its own jail as `_fresco-render`, and `atrium-surfaces-demo` showed on screen (`frescod-service-jail.png`). Its jail has no writable mount but `/atrium/sockets`: the Tier-2 kernels come precompiled in `/usr/local/share/atrium/fresco-kernels` (read-only; `scripts/fresco-renderer-bundle.sh` installs them), and the Vulkan-only mounts (fonts, ICDs, LLVM) are gone, since the compute and gpu renderers read none of them. Found on the way and fixed: jaild mounted a manifest's tmpfs volume as root 0755, ignoring its `mode` and `owner_uid` (atrium-volumes.md §6.4).

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

### F4c. Input routed by the layer list. Done.

- frescod records the layers it last put on screen (`Compositor::shown`: each window's id and screen rect, rounded as compose rounds them, at its surface's size). Pointer input hits those, top first, so the window a pixel shows is the window that gets the click. A window the WM stopped rendering, or a split-mode window with no frame yet, is not on screen and gets nothing.
- Events carry window coordinates, measured from that rect's corner. A renderer forwards them to its client as they come.
- Window 0, the background under every window, belongs to the WM. Pointer events over no window and keys with no focused window go to the WM alone, so the broadcast exception of F4a′ is gone. The desktop shell draws in a window of its own (role Background, as forum-dock does), and gets its input there.
- Verified in the amd64 VM, split mode with the renderer jailed, using the injector (`FRESCOD_INPUT_SOCK`) on `atrium-surfaces-demo` (`SURFACES_STILL=1`, which prints the input each connection gets). A click on the background reached only the WM. Clicks on A, on an undrawn (transparent) part of B, and on C each reached that window's client and the WM, in that window's coordinates. A key went to the focused window C.
- Hit-testing was by rect at first, so an undrawn part of a window still took its clicks. F4c′ fixes that.

### F4c′. Clicks go where the user sees. Done.

The risk is clickjacking: a window that is mostly transparent, placed over another app, would take the clicks meant for the app showing through it. Input regions declared by the app would not help, because a hostile app declares its whole rect. So frescod decides from the pixels.

- Each composed layer carries an alpha probe built from the frame it was drawn from (`reference::alpha_at`, which matches the kernels' alpha exactly; tested against `render` on fuzzed frames). A pointer over a pixel with alpha below `MIN_HIT_ALPHA` (16, about 6%) passes through to the window underneath, so a veil too faint to notice cannot catch clicks.
- It works the same in split mode (the frame the broker checked) and in-process mode (the frame the backend packed). The Vulkan backend has no per-window frames, so its windows hit by rect.
- Verified in the amd64 VM, split mode, renderer jailed: a click on B's undrawn part over A went to A at A's coordinates; one on B's undrawn part over the background went to the WM alone; clicks on B's drawn rect and on C's half-transparent background went to B and C.

Still to do, with F4f: an "obscured" mark on input when another app's visible content overlaps the target window, so sensitive UI (password fields, Navigator permission prompts) can refuse a click that may have been steered. It needs a field in the input events, so it waits for the wire change F4f makes anyway.

### F4d. Trusted chrome. Done (split mode).

- frescod builds its own scene from its own state, packs it like any window and composes it as the top layer (`frescod/src/chrome.rs`). No client or renderer contributes to it.
- **The strip.** The top 24 pixels of the screen are frescod's alone: an opaque layer above everything, so no app pixel ever shows there. It names the focused window's origin. An app can paint a look-alike badge anywhere in its own window, but never in the strip, so the strip is the one place a user can trust. An in-window badge was tried first and dropped for exactly that reason: an unfocused window could paint an identical one.
- **The focus ring.** A 2 px ring just inside the focused window's edge, in the same colour as the mark before the origin in the strip, ties the strip's claim to a window on screen.
- **Text.** IBM Plex Mono, rasterized at build time (frescod build.rs) into coverage tables for exactly the characters an origin may contain (lowercase ASCII letters, digits and `.:/-[]_`). frescod parses no font file at run time, and a look-alike letter has no glyph to be drawn with: the chrome channel refuses an origin with any other character, so the Navigator must send hosts in punycode.
- **Clicks** on chrome pixels go to the WM, never to a window under them; transparent chrome pixels pass through (F4c′).
- **The chrome channel** (`FRESCOD_CHROME_SOCK`, `fresco_split::chrome`). Reaching the socket is the grant, as for the WM socket. The Navigator asks it to open a document's fresco connection (`OPEN_CLIENT`); frescod answers `OPENED` with a handle and the client end as an fd, and serves the connection with a renderer like any other. `SET_ORIGIN(handle, window, origin)` then names a window without trusting anything the document said; only the connection that opened a handle may set its origins, and handles are never reused.
- Verified in the amd64 VM with both renderers jailed, using `atrium-chrome-demo`, which plays the Navigator with two documents. Document 2 paints a fake badge in its own window. The strip named `https://bank.example` while document 1 had focus and `https://evil.example:8443` while document 2 had focus, and the ring moved with it (screenshots `frescod-f4d-chrome-*.png`). A click on the strip went to the WM.

- **The WM lays out below the strip.** `DISPLAY_INFO` carries `reserved_top`, the rows frescod's chrome holds (24 in split mode, 0 otherwise). forum-wm starts its screen there, so the bar sits just under the strip and documents under the bar. Verified in the VM: a document under forum-wm on a 1280x720 screen got 1280x624 (strip 24, bar 24, dock 48).

Still to do:
- The chrome is drawn only in split mode, where the chrome channel exists.
- Secure-input affordances come with F4f.

### F3. The broker, fuzzed through its own sockets. Done (split mode).

The broker reads only fixed formats: 96-byte records from renderers and from the chrome channel, and frame slots, which it copies out and checks against the size it decided. Both record decoders are fuzzed for canonical decoding (`fresco-split/tests/protocol.rs`, `tests/chrome.rs`: whatever decodes re-encodes to the same bytes; only origins of the allowed characters get through). Slots are fuzzed in `protocol.rs` and the frames in them by F2.

`atrium-split-fuzz` fuzzes the live broker. frescod runs it in place of every renderer, and it speaks the record protocol badly on purpose: mutated and random records, fds attached, the broker's own kinds, windows it does not have, corrupt or wrong-size slots. Two personas push on cost rather than parsing. A *staller* stops reading and keeps opening and closing windows. A *flooder* sends valid full-size frames as fast as the broker answers. The same binary plays a hostile Navigator on the chrome socket. `scripts/fresco-split-fuzz.sh` runs it all in the guest with forum-wm and an animated well-behaved client as the probe. It passes when frescod lives, nothing panics, the frame loop is never quiet for more than 15 s, and a well-behaved client is served at the end.

What it found, all fixed:
- **A renderer that stops reading froze the whole display.** Every send to a renderer was blocking, and the one in `WINDOW_CREATE` held that renderer's window lock, which the frame loop also takes. A staller filled its channel, its thread blocked under the lock, and the frame loop waited on the lock forever. Measured with stallers only: the display froze within seconds and never recovered, and no renderer was ever retired. Now the broker sends to renderers only with `Chan::send_now` (`MSG_DONTWAIT`) over a 256 KiB channel, and a renderer whose channel is full is retired. FreeBSD reports a full SEQPACKET socket as a short write, not `EAGAIN`, so both count.
- **Churn was unbounded.** A window open costs two slots, the compositor lock and a WM relayout. Fifty churning renderers kept the frame loop quiet for 40 s or more. Each renderer now has a budget of 480 records/s, and 2 window opens/s after a burst of 16. Past the budget its serving thread waits, so a flooder slows only itself.
- **A renderer outlived its client.** Renderers that ignore their client's hang-up kept their client id, so after a few minutes all 255 were taken and a new client was refused. The broker now keeps an unread copy of the client's socket, sees the hang-up itself, and retires the renderer.

Measured in the amd64 VM (TCG), 150 s mixed run with the Navigator: 261 hostile renderers and 2,527 Navigator-opened documents. Frame gap at most 4 s, at most 8 renderers alive at once, frescod's fds back to their baseline, no panic, and the client at the end was served.

Still open:
- **Connections per peer: capped.** One peer uid may hold 32 connections on a Fresco socket (`MAX_CLIENTS_PER_UID`). Every app has its own uid (portcullis.md §9.0), so this is a per-app limit. Verified in the VM: 40 connections from one uid, 32 kept, the rest refused. The last 64 free ids are a reserve: once only that many are free, a uid holding 2 or more is refused (`socket_server::may_take`). So colluding apps stop at 191 ids, and the rest go two at a time to apps that hold few; a new app still connects unless 32 more apps collude (the id is a `u8` on the wire). Verified in the VM: 8 uids asking for 40 each got 32, 32, 32, 32, 32, 31, 2, 2, and a ninth app got its 2. Connections the Navigator opens over the chrome channel are capped per chrome connection instead (64).
- **Renderer memory: capped by its jail.** `scripts/fresco-renderer-bundle.sh` builds the renderer's bundle with `[resources] memory = "512M"` and `cpu = 100`, plus the fonts its server-side text needs (the jail had none, so text failed there before). The launcher should pass `--require-memory-limit`, so a machine booted without `kern.racct.enable=1` refuses to start renderers instead of running them uncapped. A renderer touches its slots through its own mappings, so the slots count against its cap. Verified in the VM: a renderer that writes every slot it gets was killed at about 512 MiB, frescod reaped it, its jail went, and frescod's fds were back to baseline.
- frescod's RSS: a 600 s run (1,023 hostile renderers, seed 23, PASS) went from 34 to 65 MiB in three steps of about 10 MiB, at roughly 0, 4 and 5 minutes, then stayed flat (64.3 to 65.5 MiB over the last 4 minutes). That looks like allocator arenas settling, not a per-renderer leak: a leak of even 2 KiB per renderer would show as steady growth. A heap profile would settle the last 1 MiB.
- `FUZZ_GPU=1` adds the app GPU channel (F6 G2): `atrium-gpu-fuzz hostile` for the whole run, then `honest`. On gpusim 3d3700d plus the ABI rev 4 driver, a 60 s run passed except honest's VM-fault check (the work came back done, not EIO), because that gpusim predates VM-fault reporting (354fffb). The RDNA4 thread's run on 354fffb passes it.
- The WM socket still speaks the full envelope protocol. The WM is trusted (F3 allows it as "equally trusted"), so it is not fuzzed here.

### F4e. One GPU VM per window. Done (gpu renderer, 2026-11-05)

- Each window's `bin` and `raster` run in a VM of its own (`GpuCompute`'s `WinVm`), which maps only the kernels' code (read-only), that window's inputs, staging and ring (staging read-only to the GPU), and its surface.
- Compose runs in frescod's VM, which maps every window's surface read-only (`Vm::share`, VM_BIND2 `BIND_READONLY`; no driver change was needed) plus the scanout buffers. The compose heap is the span of those mappings: each layer's offset is its surface's VA minus the lowest, in words, so `compose.comp` is unchanged; frescod refuses a span over 16 GiB.
- Ordering: one submission per drawn window in its VM, each signalling that window's syncobj, then compose. The driver starts another VM's work on a queue only once the queue is idle, so compose sees every raster; if that wait times out (EBUSY), frescod waits on the windows' syncobjs and submits again.
- A window that is resized or new gets a fresh VM and surface; a closed window's VM goes with it.
- Verified on gpusim 354fffb with the ABI rev 4 driver: three windows draw (`frescod-f4e-per-window-vms.png`). With `FRESCOD_GPU_ISOLATION_PROBE=1`, windows 2 and 3 raster to the address window 1's surface has in frescod's VM: both fault (EIO, `isolation.vm_faults` 2), and window 1 is drawn intact (`frescod-f4e-isolation-probe.png`). In the one-VM design that write would have landed in window 1.
- The compute (Tier-2) renderer gets the same protection another way: F4g.

### F4g. Compute renderer: renderers raster their own windows. Done (2026-11-05)

The CPU counterpart of F4e. With the compute renderer (the default), frescod never rasters a client's scene; each client's jailed renderer does.

- frescod tells the renderer in `CLIENT` (`pixels`, a new word) to raster. The renderer packs its window as before, runs `bin` and `raster` with the Tier-2 kernels inside its jail, and writes the finished pixels to its slot (`FRP1`: magic, seq, width, height, reserved zeros, then `width * height` words).
- frescod checks only the header (magic, the size it gave the window, the reserved words) and copies the pixels out once (`slot::read_pixels`). Any pixel value is fine to compose. Compose and hit-testing (F4c′, from the pixels' alpha) run in frescod, as do the background and the trusted chrome (F4d), which are frescod's own.
- Slots are sized to the window (`pixel_slot_bytes`); when the WM makes a window bigger, frescod sends new slots before the new size.
- The kernels come compiled with the bundle: `scripts/fresco-renderer-bundle.sh` runs `fresco-renderer --seed-kernels` into `usr/local/share/fresco-kernels`, so the jail neither compiles nor writes code. A renderer with no kernels in pixel mode exits, and the client gets no window, never one rastered in frescod.
- So a frame that found a bug in the kernels (F2's bounds checks are the other line) would hit only its own renderer's memory, which holds only its own client's windows; the 512 MiB cap covers the extra raster memory.
- `FRESCOD_SPLIT_PIXELS=0` sends frames to frescod again (as before F4g). The gpu renderer keeps taking frames: F4e confines its raster per window on the GPU.
- Fixed on the way: `Tier2::seed_cache` wrote kernels where the loader no longer looked (`v4/` instead of `v4/<arch>/`, since `affaa3e`), and frescod as `_frescod` could not write the system shader cache. frescod now uses `FRESCO_COMPUTE_CACHE`, else the system cache if writable, else a private 0700 directory of its own in the temp dir.
- Verified in the amd64 VM: frescod as `_frescod`, renderers jailed by portcullisd as `_fresco-render`, `atrium-surfaces-demo` drawn from the renderers' pixels (`frescod-f4g-pixel-mode.png`, the same picture as with frames). `fresco-split-fuzz.sh` passes in both modes (90 s each): hostile renderers write pixels of any value and size, frames where pixels belong, and bytes (`FUZZ_PIXELS=0` fuzzes the frame parser as before).

### F4h. A per-client budget in the renderer: refuse, don't kill. Done (2026-11-14)

Asked by Shankar (officina.md §10.5): a huge document must not take down compositing. Split mode already keeps a big scene in its own renderer; what was left is that an app growing its scene without bound ended when its renderer passed the 512 MiB cap and was killed, which looks like a crash.

- The frontend (`fresco-scene-server/src/budget.rs`) counts what each client holds, across its windows: nodes, node-parameter bytes, bound texture bytes (width × height × bytes per texel) and glyph atlas pages. Each window keeps a ledger by node (and slot) id, so whatever removes a node (a clear, a group's subtree, a disconnect, the window closing) gives its share back.
- An op that would pass a limit is not applied. The client gets `EV_BUDGET_REFUSED` (`BudgetRefusedEvent`: window, `budget::*` resource, the refused node or slot id, used, limit; `fresco_client::Event::BudgetRefused`), once per window and resource until an op of that resource goes through again, so a client stuck sending is not flooded. It evicts and sends again.
- Limits (`fresco_protocol::budget`): 262,144 nodes, 64 MiB of node parameters, 96 MiB of textures, 32 atlas pages (1 MiB each). Officina keeps about 5,000 nodes per window; a 1440p sheet is about 1,500 cells. With the windows' pixels this stays under the cap.
- A new font size past the page limit first evicts the pages no text run draws from (their slots are freed in the renderer); only if all are in use is the run refused. `OP_TEXT_MEASURE` past it answers zeros and sends the refusal.
- Found on the way: the scene server's CAS kept every texture blob ever sent, bound or not, so an app cycling textures grew its renderer forever. It now keeps only blobs a slot is bound to and those sent since the last upload pass.
- `OP_BUDGET` (65,536 kernel ops per frame after culling) still drops whole nodes at pack time; this budget is the memory side, that one the per-frame raster side.

### F4f. Secure input for passwords (waits on F5 and Navigator C0)

- A password field is marked in the window's semantic tree, and the broker validates the mark (§6.4).
- Keystrokes while that field has focus go to the broker's request builder, never to the renderer. The renderer is told only how many bullets to draw.

## Order

F4b, then F4c (and F4c′), then F4d. F4f follows once F5 and the vault exist. F4e, F4g and F4h are done.

## Decisions taken (recommended; say if you want otherwise)

- **One renderer per client connection, not per window.** An app's windows are one trust domain, and the Navigator opens one connection per document, so for web content this is per document anyway. It also avoids moving one client's windows across processes.
- **Client ids.** Client ids were a wrapping `u8` counter, so after 255 connections a new client could get the id of a live one, and with it that client's windows. Fixed alongside F4a′: a new connection takes a free id, and is refused if all 255 are in use. F4b replaces them with broker-issued 32-bit ids.
