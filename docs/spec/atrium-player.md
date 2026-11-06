# atrium-player: the native media player

**Status:** plan for review, 2026-11-06. **Owner:** the "Build the native media player"
thread. This is item P1 of [fresco-video.md](fresco-video.md) §8.

`atrium-player` is the app behind Limen's `media-player` role ([limen.md](limen.md) §2.2,
legacy-web §8.1). The Navigator, a mail app or a podcast app embeds it. It demuxes, decodes,
keeps picture and sound in step, presents frames into a Fresco video layer, and draws its own
controls. Its parent only feeds it a resource or MSE-style segments and hears back the state
events MSE exposes anyway. The same binary also runs on its own, to open a file.

Decoders are the riskiest code in media playback, so this plan treats every byte the player
is given as hostile, and most of its shape follows from that.

## 1. What already exists, and what doesn't

- **Limen** has the role in its catalogue (`limen/src/catalogue.rs`), the child side
  (`limen::Child`), and one throwaway jail per embed (limen.md §13.4). The role's message
  types (§2.2's table) are not in the crate yet. The player adds them as
  `limen::media_player`.
- **Fresco's video layers** (fresco-video.md V1 to V4) are specified, not built. The player
  needs V1 (`RGBA8`, then `NV12` and `P010`) to show anything, and V2 (`PRESENTED`) for
  sync.
- **Audio does exist.** fresco-video.md §8 says Atrium has no audio service. That is out of
  date. Lyra (lyrad) and Choragus (choragusd) run in the VM, with shared-memory stream rings,
  per-role ducking and a grant store ([atrium-lyra-architecture.md](atrium-lyra-architecture.md)
  §5.4). The A/V sync contract is settled there too (§12.1): `CLOCK_MONOTONIC` on both
  sides, audio is the master, and the player owns the policy. What is missing is the
  wire-level audio presentation clock, choragusd brokering the ring fd, and jail-verified
  identity. So P2 is "finish Lyra for the player", not "build an audio service" (§9.6).
- **No demuxer or decoder** is in the tree. ffmpeg is present in the cloud container, which
  is fine for making test clips and reference hashes (a dev tool, never shipped).

## 2. Shape: a supervisor that never parses, and a decode worker in Capsicum

One jail per embed, as Limen already does, holds two processes.

```
 jail (one per embed, from portcullisd ExecInstance)
 ┌──────────────────────────────────────────────────────────────────────┐
 │  atrium-player (supervisor)                 atrium-player-decode      │
 │   - limen::Child: role messages             (cap_enter after setup)   │
 │   - Pergola window: controls, subtitles      - demux WebM / MP4       │
 │   - Fresco video layer: owns it,            - dav1d, libvpx, Opus,    │
 │     queues frames, reads PRESENTED            Vorbis                  │
 │   - Lyra stream: writes PCM, reads clock    - writes frames straight  │
 │   - the clock and the A/V sync policy         into layer buffers      │
 │   - buffered ranges, seeks, back-pressure   - returns PCM in a ring   │
 │                                                                       │
 │   bytes ──(socketpair, length-framed)──►  samples, frames, PCM ◄──    │
 └──────────────────────────────────────────────────────────────────────┘
```

- **The supervisor never parses media.** It holds every connection that matters: the Limen
  channel to the parent, the Fresco connection, the Lyra stream. It treats bytes from the
  parent as opaque and forwards them. Its only inputs from the worker are small fixed-size
  records (frame ready in buffer N with this PTS, PCM ready, buffered ranges, error).
- **The worker does all the parsing**, demux and decode, and enters Capsicum capability mode
  as soon as it has its descriptors: the socket to the supervisor, the video layer's buffer
  fds, and a PCM ring. In capability mode it can open no file, socket or process, so a
  decoder exploit holds the worker's memory and those descriptors, and no more. It cannot
  reach the parent's channel, the network, Fresco's socket or Lyra's.
- **Limits on the worker:** an address-space rlimit (the 150 MB feed cap plus decoder
  working memory, about 512 MiB by default), a CPU-time watchdog per decode call held by the
  supervisor, and a cap on decoded frame size (7680x4320, matching the layer) checked from
  the container header and again from the decoder's own output before any copy.
- **A crashed or hung worker** is killed and reported to the parent as `Error { DECODE }`.
  The supervisor may start one fresh worker for the same feed, from the next keyframe, and
  gives up after a second crash.
- **One worker per embed**, with threads inside for dav1d. A worker per track would keep an
  Opus exploit away from video frames, but the jail already holds nothing else of value, and
  muxed files would need a third process to demux. Not worth it in v1.

Frames go straight from the decoder into the layer's shared memory. The worker converts
dav1d's and libvpx's planar output (I420, I010) to the layer's `NV12` or `P010` while copying,
which is one pass over the frame either way. 4:2:2 and 4:4:4 content is down-sampled to 4:2:0
in the same pass, so the layer formats stay the three Fresco has.

## 3. Demux

Both demuxers are Atrium code in safe Rust (`#![forbid(unsafe_code)]`), in one crate,
`atrium-media-demux`, with no I/O of their own: they take byte slices and return samples.

- **WebM / Matroska:** EBML with checked element sizes, a depth limit, at most 8 tracks,
  lacing, `SimpleBlock` and `BlockGroup`, cues for seeking, `CodecPrivate` passed to the
  decoder as an opaque blob with a size cap.
- **MP4:** both plain MP4 (`moov` with sample tables, for `Load`) and fragmented MP4
  (`moof`/`mdat`, which is what MSE feeds use). Box sizes checked against their parents,
  sample tables bounded by the 150 MB feed cap.
- **What they refuse:** anything over a limit is an error, never a reallocation. Unknown
  boxes and elements are skipped by size.

Writing our own rather than taking Mozilla's `mp4parse` or Symphonia's demuxers keeps them
under the licence policy without a case-by-case MPL review, lets them parse incrementally from
MSE appends, and keeps the limits ours. They are small: a few thousand lines together.

Each demuxer has a `cargo fuzz` target from its first commit, and a corpus seeded from the
test clips.

## 4. Decoders

| codec | library | licence | in v1 |
|---|---|---|---|
| AV1 | dav1d | BSD-2 | yes |
| VP9, VP8 | libvpx | BSD-3 | yes |
| Opus | libopus | BSD-3 | yes |
| Vorbis | libvorbis | BSD-3 | yes |
| H.264 | OpenH264 | BSD-2 (source); patents, see §9.3 | behind a build feature |
| AAC | none permissive and mature in C; Symphonia's (Rust) is MPL-2.0 | see §9.4 | no |

- They are linked from FreeBSD packages (`multimedia/dav1d`, `multimedia/libvpx`,
  `audio/opus`, `audio/libvorbis`) into the worker only. The supervisor links none of them.
- Each sits behind a small `Decoder` trait in `atrium-media-decode`, so rav1d (the Rust port
  of dav1d) or a hardware decoder can replace one later. Hardware decode (G1) slots in here,
  in the worker, once the GPU thread finds it feasible.
- Each is configured with its own limits where it has them (dav1d's `frame_size_limit`,
  thread count), and its output is checked against the container's declared size.
- The FFI is one `unsafe` module per library, reviewed as such, with the rest of the crate
  safe.

## 5. Timing and A/V sync

The policy is the one Lyra's spec settled (§12.1).

- **The clock.** With an audio track, the master is Lyra's audio presentation clock: the
  stream frame now reaching the speaker, as a `CLOCK_MONOTONIC` instant. With no audio
  track, or before Lyra exposes that clock, the master is `CLOCK_MONOTONIC` itself, started
  at play.
- **Video chases.** For each decoded frame the supervisor computes when its PTS will be
  heard, subtracts the display pipeline's reported latency, and queues it with that
  `present_ns`. With VRR on, frescod shows it at that instant. On a fixed refresh it snaps
  to the nearest vblank, which holds sync within half a refresh.
- **Feedback.** `EV_VIDEO_PRESENTED` gives the vblank each frame really showed at. The
  supervisor tracks the error and corrects later `present_ns` values. If decode falls behind
  by more than a few frames, it drops non-reference frames and then skips to the next
  keyframe, and reports the drops in its own stats, never to the parent (limen.md §10:
  frame timing is not a parent-visible channel).
- **Rate and volume** (`SetRate`, `SetVolume`) are applied in the supervisor to the PCM it
  writes to Lyra. Rates other than 1 need time-stretching to keep pitch. v1 does this with a
  simple WSOLA stretch in the supervisor, so the worker's output never changes with rate.
- **Off screen.** When the window is off screen or released from its slot (Limen's
  `Suspended`, fresco-video §4), video decode pauses, audio keeps playing (background audio),
  and on return the player decodes from the keyframe before the clock's position.

## 6. Feeds: `Load` and the segment feed

- **`Load`** takes a `DocRef` blob or a file descriptor limend passes. URLs need a fetcher in
  the player's jail. v1 leaves `Load` by URL to a later step, which reuses the Navigator's
  jailed fetcher rather than a second network stack.
- **The segment feed** implements a reduced MSE coded-frame model per track: init segments,
  `timestamp_offset`, buffered ranges (at most 64), `Remove`, `Abort` and `EndOfStream`.
  One `Append` is in flight per track, `Appended` releases the next, and the 150 MB cap
  answers `Error { TOO_BIG }`, as limen.md §2.2 says. The supervisor counts bytes; the worker
  owns the parsed samples and reports buffered ranges.
- **`CanPlay`** answers from the build's decoder list. With H.264 off, YouTube is told
  `avc1` is unsupported and serves VP9 or AV1 with Opus, which is what Firefox without
  OpenH264 gets.
- **Seeking** within buffered data goes to the keyframe at or before the target and decodes
  forward silently. A seek outside it sends `Seeking` (from native controls) or `Waiting`, and
  the parent fetches.

## 7. The window and controls

- The player is a Pergola app. Its window holds one video layer `Below` its drawing, placed
  letterboxed in the window, and the controls drawn over it with alpha: play and pause, a
  scrubber with buffered ranges, time, volume, full screen, picture-in-picture. They hide
  after a few idle seconds and come back on pointer motion or a key.
- Subtitles (WebVTT, and Matroska's text tracks) are text in the window's drawing, shaped
  in-app like any Pergola text. That is a later step.
- **Media keys, PiP, the lock screen.** PiP is the WM moving the player's window out of the
  slot into a small floating window, and back. It needs a Limen message pair and Forum
  support. Media keys need an input route from Forum to "the playing media". Both are listed
  as later steps with their owners, because they are not the player's code alone.

## 8. Build plan

| step | work | runs on | depends on |
|---|---|---|---|
| M0 | `limen::media_player`: the role's message types (postcard), with tests | Linux | — |
| M1 | `atrium-media-demux`: WebM and MP4 (plain and fragmented), limits, fuzz targets; tests against ffmpeg-made clips | Linux | — |
| M2 | `atrium-media-decode`: dav1d, libvpx, Opus, Vorbis behind `Decoder`; frame hashes match ffmpeg's `framemd5` on the test clips | Linux, then FreeBSD | — |
| M3 | the worker and the supervisor: process split, Capsicum on FreeBSD (unconfined on Linux, for tests only), limits, watchdog, crash restart; the MSE feed model; a headless sink | Linux, then the VM | M0–M2 |
| M4 | presenting: a Fresco video layer, `RGBA8` first, then `NV12`/`P010`; the clock with `CLOCK_MONOTONIC` as master and `PRESENTED` feedback | the VM | Fresco V1, V2 |
| M5 | the Pergola window and controls; running as a Limen `media-player` child in the VM, fed by a probe parent (`Load` and segment feed) | the VM | M4, Limen L4 |
| M6 | audio: a Lyra stream through choragusd as role Media, audio-master sync, volume, rate with WSOLA | the VM | Lyra clock and fd brokering (P2) |
| M7 | later: `Load` by URL, subtitles, PiP, media keys, H.264 and AAC if decided, hardware decode (G1) | | |

M0 to M3 start now and wait on nothing. A headless `atrium-player --sink=hash` plays a clip
end to end and prints per-frame hashes and timing, so the whole pipeline is tested before
Fresco's layers land.

**Tests along the way:**
- every demuxer limit has a test with a crafted file that exceeds it;
- fuzzing runs on each demuxer and on the worker's whole input path for a fixed time in CI;
- decoded frames match ffmpeg's reference hashes for every clip and codec;
- in the VM: a worker that crashes (a test build with an abort switch) is reported as
  `DECODE`, the supervisor keeps its window, and a second worker resumes;
- in the VM: the worker cannot `open`, `connect` or `fork` after setup (a test build that
  tries each and reports the error);
- A/V sync: measured skew from `PRESENTED` and Lyra's clock stays within half a refresh.

## 9. Decisions, with recommendations

1. **Two processes in the jail, with demux and decode only in a Capsicum'd worker.**
   *Recommend yes.* The jail alone would leave a decoder exploit holding the Limen channel,
   the Fresco connection and the audio stream. The cost is one extra process and a copy of
   compressed bytes over a socket, which is small next to decoding.
2. **Our own WebM and MP4 demuxers in safe Rust**, rather than `mp4parse` or Symphonia.
   *Recommend yes*, for the limits, incremental MSE parsing and licence reasons in §3.
3. **H.264 is off by default in v1.** OpenH264's source is BSD-2, but its patent licence
   covers only Cisco's prebuilt binaries, which Cisco doesn't build for FreeBSD. Building it
   ourselves means shipping H.264 without that cover. *Recommend:* keep it behind a build
   feature for development, answer `CanPlay` false for `avc1`, and decide later between
   Cisco's binary under Linux ABI emulation (the CDM route) and no H.264. YouTube and most
   MSE sites fall back to VP9 or AV1.
4. **No AAC in v1.** There is no permissive, mature AAC decoder in C, and Symphonia's Rust
   one is MPL-2.0, which the policy reviews case by case. AAC mostly travels with H.264 in
   MP4, so *recommend* deciding it together with item 3.
5. **The name `atrium-player`** (display name "Player"), as a foundation app under
   NAMING.md's plain-name rule. *Recommend yes.*
6. **P2 becomes "finish Lyra for the player"**, owned by whoever owns Lyra: the audio
   presentation clock on the wire (Lyra §12.1's follow-up), choragusd brokering the ring fd,
   and jail-verified identity. *Recommend yes*, and fresco-video.md §8 is corrected to say
   so. Until it lands the player plays video against `CLOCK_MONOTONIC` and drops decoded
   audio.
