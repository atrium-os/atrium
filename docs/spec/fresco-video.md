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
frame for each refresh, converts YUV to the screen's colour space and scales it. It then tells
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

## 8. Who does what

| # | work | owner |
|---|---|---|
| V1 | frescod: video layers in shared memory, `NV12`/`P010`/`RGBA8`, composed under the window, in both modes; limits | Fresco thread |
| V2 | frescod: present times, `PRESENTED` and `RELEASED`, content rate with VRR | Fresco thread |
| V3 | frescod: colour (matrix, range, BT.2390 tone map, HDR passthrough); the protected flag | Fresco thread |
| V4 | a probe player: plays a test clip through a layer in the VM, checks timing and colour against reference frames | Fresco thread |
| P1 | the native player behind `media-player` ([atrium-player.md](atrium-player.md)): demux (WebM, MP4), dav1d, libvpx, OpenH264, A/V sync from `PRESENTED`, native controls, PiP and media keys | a new thread (Shankar to allocate) |
| P2 | audio: Lyra and Choragus exist (atrium-lyra-architecture.md §5.4); the player needs Lyra's audio presentation clock on the wire (§12.1), choragusd brokering the ring fd, and jail-verified identity (atrium-player.md §9.6) | a new thread (Shankar to allocate) |
| P3 | the Navigator's `<video>` and `MediaSource` as a front end to the role | Navigator thread, with the Pergola thread |
| G1 | hardware decode: the VCN block in atrium-gpu-amd, and a gpusim model of it | GPU thread; feasibility first (§9.4) |
| G2 | GPU frames into layers (§7): sealed buffers from the player's context | GPU thread with Fresco thread, after F6 |
| D1 | overlay planes for video on silicon | Fresco thread, after D5 |

V1 to V4 are ready to build once this is reviewed. P1 can start at once against V1's
`RGBA8` format and a CPU decoder. Nothing in it waits on G1.

## 9. Decisions, with recommendations

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
