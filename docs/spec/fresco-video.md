# Fresco video layers

**Status:** proposal for review, 2026-11-06. **Owner:** Fresco thread (frescod parts); other
owners in §8.

Video playback from local files through to streaming, and what Fresco needs for it. This
builds on Limen's `media-player` role ([limen.md](limen.md) §2.2, legacy-web §8.1): a native
player in its own jail demuxes, decodes and presents in its own window, which Limen pins into
the parent as a plain slot. The engine, mail app or podcast app only feeds it. This spec is the
"presents" part: how decoded frames reach the screen.

## 1. What is missing today

- **No YUV anywhere.** Decoders produce NV12 (8-bit) or P010 (10-bit) frames. Fresco windows
  carry RGBA scene drawings or RGBA pixels (split pixel mode), and textures go through an
  upload path built for images that rarely change. That suits icons, not 60 new frames a
  second.
- **No frame timing.** frescod draws a window's latest frame at the next compose. A player
  needs each frame shown at a chosen time, and needs to hear when it actually showed, to keep
  picture and sound in step.
- **No colour description per window.** HDR video (PQ, HLG, BT.2020) has nowhere to say
  what it is, so frescod can't tone-map it for an SDR screen or pass it through on the HDR
  desktop (display 2.3).
- **A whole-window redraw per frame.** If the player drew each frame into its window, its
  renderer would redraw the window, controls included, 24 to 60 times a second.

## 2. Proposal in one paragraph

A window may own up to two **video layers**. A video layer is a small ring of frame buffers
that frescod allocates in shared memory, in one fixed format and size. The player writes a
decoded frame into a free buffer and queues it with a **present time**. frescod itself
composes the layer under the window's own drawing, at the layer's rect: it picks the right
frame for each refresh, converts YUV to the screen's colour space and scales it (whether that
conversion runs in frescod or in the window's renderer jail is decision 6). It then tells
the player which frame showed at which vblank, and which buffers are free again. The player's
controls and subtitles are its ordinary window drawing, drawn over the video with alpha.
Nothing about decoding enters frescod.

## 3. The layer

- `OP_VIDEO_LAYER_CREATE { window, format, width, height, buffers }` → `{ layer, fds }`.
  `format`: `NV12`, `P010` or `RGBA8` (for decoders that output RGB, and for tests).
  `width`, `height` at most 7680x4320; `buffers` 3 to 8. frescod allocates the buffers in
  shared memory and passes the fds, as it does for split frame slots. A
  buffer's size is fixed by format and size, so frescod never parses anything in it: hostile
  bytes are only odd pixels.
- `OP_VIDEO_LAYER_PLACE { layer, rect: Option<[x, y, w, h]>, z: Below | Above }` in the
  window's surface pixels; `None` hides it. `Below` (the default) puts it under the window's
  drawing, so the player leaves those pixels transparent and draws controls over them.
  Scaling to the rect keeps the aspect ratio the player asks for (it letterboxes itself by
  choosing the rect).
- `OP_VIDEO_LAYER_COLOR { layer, matrix, range, primaries, transfer, hdr: Option<Mastering> }`
  with `matrix` BT.601, BT.709 or BT.2020; `range` limited or full; `primaries` BT.709 or
  BT.2020; `transfer` sRGB, BT.1886, PQ or HLG; `Mastering` the static HDR metadata
  (max/average light level, mastering display), the same fields display 2.3 sends as an
  infoframe.
- `OP_VIDEO_LAYER_DESTROY { layer }`, and every layer goes with its window.

Limits per client: 4 layers, 64 MiB of video buffers (a 4K P010 ring of 4 is 48 MiB). Over
that the create is refused.

## 4. Presenting frames

- `OP_VIDEO_FRAME { layer, buffer, seq, present_ns }`: `buffer` now holds frame `seq`, to show
  at `present_ns` on `CLOCK_MONOTONIC` (0: as soon as possible). The player owns no buffer it
  has queued until frescod releases it.
- On each refresh, frescod takes for each layer the latest queued frame whose `present_ns` is
  at or before that refresh's vblank, skips older ones, and composes it.
- `EV_VIDEO_PRESENTED { layer, seq, vblank_ns, dropped: u32 }`: frame `seq` first showed at
  that vblank; `dropped` counts frames skipped since the last event. The player's audio clock
  steers `present_ns` from these.
- `EV_VIDEO_RELEASED { layer, buffer }`: frescod is done with `buffer`.
- `OP_VIDEO_LAYER_RATE { layer, content_hz_milli }`: the content's frame rate. With VRR on
  (display 2.2, the default), frescod paces the output so each video frame lasts a whole
  number of refreshes: 24p plays at 48 or 120 Hz with no judder. Without VRR it changes
  nothing, and there is no mode switch for video unless the user asks for one.

Only a window that is shown gets refresh timing. A layer whose window is off screen, or
released from its slot, gets no `PRESENTED` and every queued buffer back as `RELEASED`, so
the player can pause decoding. That is the same signal Limen's `Suspended` gives.

## 5. Colour

frescod composes in the desktop's space.

- **SDR desktop:** YUV to RGB by the layer's matrix and range, then to sRGB. PQ and HLG
  frames are tone-mapped to SDR with the BT.2390 EETF, using the layer's mastering metadata
  where it has some.
- **HDR desktop** (`FRESCOD_HDR=1`, display 2.3): PQ passes through; HLG and SDR are
  converted into PQ at the SDR reference white (203 nits), as the desktop already does.
- The conversion is a fixed kernel per (format, colour) pair, part of frescod's signed kernel
  set, run by the compute renderer, and by the gpu renderer on gpusim.

## 6. Protected frames

`OP_VIDEO_LAYER_CREATE` takes a `protected` flag for DRM'd content. In v1 it means only that
frescod never hands the layer's pixels to anything that captures the screen (screenshots,
recording, the WM's thumbnails). They show black there. Real hardware protection (frames
the CPU can't read, HDCP on the link) needs the GPU and display work in §8 and a CDM, and is
not in this spec.

## 7. Later: zero copy and overlay planes

- **GPU frames.** With hardware decode, frames are born in VRAM. The layer then takes GPU
  buffers instead of shared memory: the player's GPU context writes a frame, seals it (GPU
  ABI §5.13, read-only from then on), and passes a `share_fd`. frescod's broker binds it
  read-only and samples it, which is the same YUV kernel reading from VRAM. No frame touches
  the CPU. This needs the app GPU path (F6) to be in production.
- **Overlay planes.** A full-screen or large video can scan out on its own DCN plane with
  the colour conversion in the display hardware, so the GPU and CPU sleep between frames.
  frescod falls back to composing when the layer is covered, transformed or translucent.
  This is display pipeline work (D-series) on silicon.

## 8. Security

Who might be hostile, and what each can reach.

**Hostile media** (a crafted file or stream). Demuxers and decoders are where media exploits
land, so they run only in the player's jail (limen.md §2.2), one jail per embed, discarded at
exit, with no network of its own beyond a filtered fetcher. A decoder that is taken over holds
what that jail holds: its Limen channel, its frescod connection and its layers. It never gets
the parent's process, cookies or other windows.

**A compromised player** (the decoder was exploited). This is the case to design for.

- *It can draw anything in its own slot*, as any window can, including a fake prompt. The slot
  is pinned inside the parent and can't cover anything outside it, and frescod's strip still
  names the focused window's origin.
- *It must not escape the slot.* Today an embed child's frescod connection can also create
  ordinary windows, which a compromised player could use to put a convincing top-level window
  on screen. **Fix:** a connection that limend hands to an embed child is *embed-only*: every
  window it creates is `embed_pending`, and only limend can bind it. frescod marks the
  connection when limend opens it for the child, so the child never sees the ordinary app
  socket. Shape agreed with the Pergola thread: limend opens a new connection to the
  activation socket and sends `slot::op::EMBED_CONNECT` (0x0911, `EmbedConnect { owner_uid }`)
  first; on `Ok` frescod turns that same stream into a client connection owned by
  `owner_uid` (never a slot-allowlist uid) and marked embed-only, which in split mode the
  broker enforces on the renderer's window records. limend passes the connected fd to the
  child. User-role children only: system providers (Arca, Vestibulum) keep ordinary
  connections, and a provider's embed instance doesn't get the app socket mounted. Owners: Fresco and
  Pergola threads; it applies to every Limen role, not only video.
- *Its layers stay inside its window.* frescod clips every layer to its window's rect, and a
  layer takes its window's place in z-order, so it can never sit over another window or over
  a credential slot. frescod's visibility record (arca-autofill.md §4) counts a window's layers
  as part of that window.
- *Shared buffers can't hurt frescod.* Each buffer's size is fixed by format and size, so
  nothing in it is parsed or used as a length or an index. The player may write a buffer while
  it is being read; frescod copies out of it with plain loads, never forms a Rust reference to
  shared memory, and the worst outcome is a torn frame in the player's own slot.
- *It can't exhaust the compositor.* Converting and scaling video costs compose time in
  frescod's broker, which is part of the trusted base. Limits: 4 layers and 64 MiB per client,
  at most one frame per layer per refresh (extra frames are dropped, not converted), the
  layer's source no larger than 3840x2160 unless the window covers most of the screen, and a
  global cap of 256 MiB of video buffers across clients. **Alternative to weigh (decision 6):**
  run the YUV conversion in the window's own renderer jail, as split pixel mode (F4g) already
  does for drawing, so frescod only blends finished RGB surfaces and picks one per refresh.
  That keeps frescod's work "compose only", at the cost of one more converted copy per frame.
- *What it learns.* `PRESENTED` gives vblank times for its own layer, which a window's frame
  callbacks already reveal, and whether its window is on screen, which Limen's `Suspended`
  already tells it. It sees nothing of other windows.
- *Brightness.* An HDR layer is limited to the desktop's peak, and a layer that is not
  fullscreen is held to 2x SDR white, so a small embed can't flash full HDR brightness.

**A hostile parent or page.** The parent only feeds bytes and controls; it never handles
frames or decoded audio (limen.md §2.2), and can't read the slot's pixels, as no client can
read another window. It can try to overload the player with appends, which limen.md §2.2's
back-pressure and 150 MB limit cover.

**Other apps and capture.** No client can read a layer. Screenshots and recording go through
frescod's capture path, which skips `protected` layers (§6). Unprotected video is captured
like any other window, by the same approved capture apps.

**Hardware decode (G1) moves the parser into the GPU.** The VCN block and its firmware parse
the bitstream, with DMA access. So hardware decode must run in the player's own GPU address
space (its VMID), under the GPU ABI's isolation rules, with decode buffers scrubbed on free like
other VRAM, and frames passed on only as sealed read-only buffers (§7). The firmware is a
binary we may not inspect (amdgpu firmware licence), so hardware decode is an opt-in per
codec, and the CPU decoders in the jail stay the default until the G1 thread reports on that
isolation.

**Streaming fetches.** A whole-resource `Load` is fetched by the player through a jailed
fetcher with the Navigator's address filter (no LAN, loopback or rebinding), and with no cookies
unless Arca attaches them for that origin. Segment feeds are fetched by the engine, so they
are covered by the engine's own rules.

## 9. Who does what

| # | work | owner |
|---|---|---|
| V1 | frescod: video layers in shared memory, `NV12`/`P010`/`RGBA8`, composed under the window, in both modes; limits | Fresco thread |
| V2 | frescod: present times, `PRESENTED` and `RELEASED`, content rate with VRR | Fresco thread |
| V3 | frescod: colour (matrix, range, BT.2390 tone map, HDR passthrough); the protected flag | Fresco thread |
| V4 | a probe player: plays a test clip through a layer in the VM, checks timing and colour against reference frames | Fresco thread |
| P1 | the native player behind `media-player` ([atrium-player.md](atrium-player.md)): demux (WebM, MP4), dav1d, libvpx, OpenH264, A/V sync from `PRESENTED`, native controls, PiP and media keys | a new thread (Shankar to allocate) |
| P2 | audio: finish Lyra and Choragus for apps ([lyra-v1.md](lyra-v1.md)): the audio presentation clock on the wire (Lyra §12.1), the app wire (formats, pause, flush), registry-only identity, no `/dev/dsp` in app jails, lyrad and choragusd as non-root jailed services. choragusd already brokers the ring fd | audio thread |
| P3 | the Navigator's `<video>` and `MediaSource` as a front end to the role | Navigator thread, with the Pergola thread |
| G1 | hardware decode: the VCN block in atrium-gpu-amd, and a gpusim model of it | GPU thread; feasibility first (§10.4) |
| G2 | GPU frames into layers (§7): sealed buffers from the player's context | GPU thread with Fresco thread, after F6 |
| D1 | overlay planes for video on silicon | Fresco thread, after D5 |

V1 to V4 are ready to build once this is reviewed. P1 can start at once against V1's
`RGBA8` format and a CPU decoder. Nothing in it waits on G1.

Added for security (§8): S1, embed-only frescod connections for every Limen child (Fresco
thread with the Pergola thread), and the compose limits in V1.

## 10. Decisions, with recommendations

1. **frescod composes video itself, under the window**, rather than the player drawing frames
   into its window. *Recommend yes.* The controls aren't redrawn per frame, frescod owns the
   timing it reports, and HDR and overlay planes have one place to hook in. The cost is a
   second kind of layer in the compositor.
2. **Shared-memory frames and CPU decode first**, GPU frames later. *Recommend yes.* It works
   on gpusim and in the VM today, and the layer API doesn't change when GPU frames arrive
   (only the buffer kind does).
3. **No mode switch for video by default**; VRR does the pacing. *Recommend yes.* A mode
   switch blanks the screen for a second on most monitors.
4. **Hardware decode needs a feasibility check before anyone builds it.** gpusim is
   clean-room (MIT and public documents only), and AMD's VCN interface is less publicly
   documented than the rest of the GPU. *Recommend:* the GPU thread first reports what public
   documentation exists, and until then decode stays on the CPU, in the player's jail.
   Findings: [2026-11-06-vcn-decode-feasibility.md](../reviews/2026-11-06-vcn-decode-feasibility.md).
5. **v1's `protected` flag only keeps frames out of capture.** *Recommend yes.* It is honest
   about what it guarantees, and real protection waits for the CDM work.
6. **Where YUV is converted** (§8): in frescod's broker, or in the window's renderer jail with
   frescod only blending. *Recommend the renderer jail*, to match split pixel mode (F4g): a
   bug in conversion, scaling or tone mapping then lands in an unprivileged per-window jail,
   not in the compositor. It costs one extra RGB copy per frame, which is affordable up to
   4K. With GPU frames (§7) the same rule holds: the conversion runs in the window's GPU
   context, and frescod composes the result.
7. **Embed-only connections for Limen children** (§8). *Recommend yes, before any media or
   document role ships*, since otherwise a compromised child can open top-level windows.
