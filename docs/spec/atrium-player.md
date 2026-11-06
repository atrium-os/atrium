# atrium-player: the native media player

**Status:** accepted, 2026-11-06 (Shankar agreed with all six §9 recommendations); M0–M3 in progress. **Owner:** the "Build the native media player"
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
- **Pure-Rust decoders later** (Shankar, 2026-11-06). The C libraries are v1 because they
  are the mature ones. Each can be swapped behind `Decoder` once a Rust decoder passes the
  codec's conformance vectors and our fuzzing. Checked 2026-11-06: rav1d (AV1, BSD-2) is a
  translation of dav1d that still uses dav1d's assembly and 500-odd `unsafe` blocks, and its
  last release (1.1.0, 2025-05) tracks dav1d 1.5.1. lewton (Vorbis, MIT/Apache) is widely
  used, but its last release was in 2021. rusty_vp9 (VP9, Apache-2.0, first released
  2026-07) and opus-rs (BSD-3, 2026-02) or ruopus (MIT, 2026-06) for Opus claim conformance,
  but they are months old and the claims are their own.

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

- **`Load`** takes a file descriptor limend passes. There is no `Load` by URL (decided by
  Shankar, 2026-11-06): the player's jail has no network, and the Navigator fetches, passing a
  descriptor or feeding segments. A standalone player that opens a URL would start the
  Navigator's jailed fetcher as a separate process, never giving the decode jail network.
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
  The design is [atrium-player-pip-keys.md](atrium-player-pip-keys.md) (proposal).

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
| M7 | later: subtitles, PiP, media keys, H.264 and AAC if decided, hardware decode (G1) | | |

M0 to M3 start now and wait on nothing. A headless `atrium-player --sink=hash` plays a clip
end to end and prints per-frame hashes and timing, so the whole pipeline is tested before
Fresco's layers land.

**As built (2026-11-06), in `atrium-player/` (its own cargo workspace):**
- M0 (48caf43): `limen::media_player`, the role's messages with a checked `decode` on both
  sides. `CanPlay` became a query and an answer, and `Playing`/`Paused` were added
  (limen.md §2.2 updated).
- M1 (b5ffd58): `demux/` (`atrium-media-demux`), push parsers for WebM and MP4 (plain and
  fragmented) with `#![forbid(unsafe_code)]`. A plain MP4 whose `moov` comes last asks the
  driver for an offset. Seeks use WebM cues and the MP4 sample table. `fuzz/` has cargo-fuzz
  targets for both.
- M2 (fd41298): `decode/` (`atrium-media-decode`). Each library sits behind a C shim
  (`decode/shim/`) compiled against its real headers, so no struct layout is copied into
  Rust. One `unsafe` FFI module, and safe NV12/P010 conversion.
- M3 (a3f567f): `player/` (`atrium-player`, the supervisor, which links no decoder) and
  `worker/` (`atrium-player-decode`). The wire is framed postcard with descriptors from the
  supervisor, and fixed 32-byte records back. Layer buffers and the PCM ring are shared
  memory. The worker announces the video track only after decoding its first picture, so
  the bit depth (NV12 or P010) is known before buffers are made. `atrium-player --headless`
  plays a file or a feed and prints frame hashes. The worker's descriptors are narrowed with
  `cap_rights_limit` (a78962b).
- M3 on FreeBSD (16-CURRENT arm64 VM): in capability mode the worker cannot open or connect.
  Capsicum allows `fork`, so the worker also sets `RLIMIT_NPROC` to 0 before `cap_enter`;
  root is exempt from that limit, so the worker must run unprivileged (it does in the jail).
  Every clip, loaded or fed, gives the same layer hashes as on Linux and as ffmpeg.
- M4: `player/src/fresco_sink.rs`. The video goes into a Fresco video layer under a
  transparent window, letterboxed, with each frame's `present_ns` taken from a monotonic
  clock (audio takes over as the master in M6). `VideoReleased` returns buffers to the
  worker, and `VideoPresented` gives the skew. `atrium-player FILE` plays in a window; in the
  VM's headless frescod, `red-blue.webm` shows red and then blue, scaled 2x into the window.
  Headless frescod captures at most every 200 ms under TCG, so most frames are counted as
  dropped there, and the skew it reports is the capture's, not the player's.
- Test clips and reference hashes are made by `scripts/media-test-clips.sh` into
  `test-assets/media/` (ffmpeg, a dev tool). Every clip's decoded frames, and what lands in
  the layer buffers, match ffmpeg's `framemd5` exactly.
- M5: `player/src/app.rs` is one loop for both the standalone window and the Limen child
  (`atrium-player --limen`: limend's channel on stdin, the embed-only frescod connection on
  it). The parent's messages are checked with `ToChild::decode` and malformed ones dropped.
  `player/src/controls.rs` draws the controls with Fresco scene nodes over the video:
  play/pause, a scrubber showing buffered and played time, hiding after 3 s while playing.
  A press on the picture or the space key toggles, the arrow keys step 5 s, and the scrubber
  seeks. Pause holds decoded frames in the supervisor, so the worker stops once its buffers
  are all held. A child starts paused, showing its first frame as a poster, and a seek
  while paused shows its target frame. A hidden slot pauses playback, and showing it again
  resumes. `Append` is capped at 60 KiB, to fit limend's 64 KiB messages.
  `atrium-media-probe` (`player/examples/`) runs limend's code in process against a live
  frescod. It starts the player as another uid with its channel on stdin, as portcullisd
  would, and feeds `red-blue.webm` MSE style. All 16 checks pass in the VM: attach,
  CanPlay, appends acknowledged one at a time, buffered ranges, Ready, the poster, red then
  blue in the slot, Time, Ended, a seek while paused, suspend and resume, a malformed
  message dropped, and detach ending the player.
- `Load` by descriptor: limend carries one read-only file or pipe with a message for roles
  that take one (limen.md §2.2, `SEND_FD`), checks it and narrows it with `cap_rights_limit`.
  The player plays it like a local file. The probe's second embed loads `red-blue.webm`
  this way, after limend refuses a socket in its place: 24/24 checks pass in the VM. limen's
  daemon tests (11) pass on Linux and FreeBSD.
- Not in M5: the controls
  have no time text, because text waits for Pergola's in-app shaping. They are drawn
  directly, not with Pergola widgets, because a Pergola window can't hold a video layer
  yet. There is one source per embed; a new `src` is a new embed.
- M6 (on Linux, against a stand-in choragusd in `worker/tests/audio.rs`): one Lyra stream as
  role `Media` per player, and the audio clock as the master the video follows
  (`audio_out.rs`). Every decoded sample reaches the stream in order; pause stops it; a seek
  flushes and the next run starts at the target, with the pre-roll trimmed; the media clock
  runs at real time to within 30 ms over 500 ms. Opus start delay is now taken from the
  container (WebM `CodecDelay`, MP4 `elst`) and the decoder's pre-skip, so the timestamps
  line up. `SetRate` plays at 0.25 to 4 times with the pitch kept: a streaming WSOLA
  (`wsola.rs`, 20 ms Hann segments, ±8 ms search). The new rate takes effect from a seek to
  where playback is. At 1.5 times, the clock runs 1.5 times as fast and the clip plays a third
  shorter. WSOLA keeps its last segment (about 30 ms) at the end of a clip. At the end the stream is
  drained (`Drain`, then `Drained`), so the last partial period is heard.
- M6 on real lyrad in the VM (lyrad as `_lyra` on QEMU's HDA, choragusd as `_choragus`, the
  player as registry app `appa`, `examples/audio_play.rs`), with `vp9-opus.webm`'s 440 Hz
  tone read back from `LYRA_DUMP`:

  | rate | clock (media s per s) | sound heard | expected | pitch | underruns |
  |------|----------------------|-------------|----------|-------|-----------|
  | 1    | 0.995 | 2.000 s | 2.014 s | 439.5 Hz | 0 |
  | 1.5  | 1.500 | 1.327 s | 1.342 s | 440.0 Hz | 0 |
  | 0.75 | 0.749 | 2.647 s | 2.685 s | 439.7 Hz | 0 |

  The level is the same at every rate. Lyra's own VM check passes on the same boot. A/V skew
  against vblank isn't measured here, because headless frescod under TCG captures at most
  every 200 ms; that needs the Mac's VM or hardware.
- M7, WebM Cues: a file (not a pipe) whose Cues sit after the clusters, where ffmpeg puts
  them, has them read through the SeekHead before the first cluster, then playback goes
  back. A SeekHead that points past the end or at anything but Cues is ignored. Seeks start
  sound 80 ms before the target (MP4 and WebM alike). A pipe, or a file with no Cues, still
  reads again from the start and drops frames up to the target.
- M7, colour: the bitstream's and the container's colour descriptions go to
  `video_layer_color` (09a0787); unsaid fields are guessed (BT.601 below 720 lines). M7,
  streaming seeks (ff5ca7d): a feed seek waits for its segment however long it takes, and
  the page is told `Waiting`.
- M7, subtitles: WebVTT text tracks in WebM and tx3g in MP4 become cues the worker sends
  as records; the supervisor keeps them (bounded) and the window draws the ones at the
  playback time, shaped in-app with swash and drawn as glyph runs (checked in the VM).
- M7, for Navigator: the player answers in the order messages come, and each accepted
  `Append` gets its `Buffered` before its `Appended` (a test checks it). `Error` names a
  `track`: an append over the cap gets `Error { TOO_BIG, track: Some(t) }` and no
  `Appended`, and the feed goes on; `track: None` means the player failed. A progressive
  download is `Load { growing: true }`: the worker waits at the file's end instead of
  ending (a note every second keeps the watchdog quiet), doesn't go looking for Cues that
  aren't written yet, and ends only after `LoadDone`. A seek back into what has arrived
  works meanwhile (`worker/tests/play.rs`).
- M7, page subtitles: `TextTrack { id, lang, label }` carries a WebVTT file (`<track>`) as a
  regular-file descriptor, and `ShowText { None | InBand | Page(id) }` picks what shows
  (in-band by default). The decode worker parses the file (`cue::vtt_file`: signature,
  timings, NOTE/STYLE/REGION skipped, markup stripped, 4 MiB and 8192 cues at most, a cue at
  most a minute), so the supervisor still parses nothing. A restarted worker reads the
  files again; a new file under an id replaces the old one's cues.
- M7, a URL in the standalone player (`atrium-player URL`, windowed or `--headless`): the
  player starts `atrium-player-fetch`, a process of its own that reads its trust store,
  enters Capsicum (network through casper, as navigator-fetchd), and only then reads the
  URL. It fetches through Nuntius with `Fd` delivery and hands back the read-only
  descriptor as soon as the head is in; the player plays it as a growing `Load` and gets
  `LoadDone` at `Done`. A download cut off part way plays what arrived. Neither the
  supervisor nor the worker gets network. It stands in for nuntiusd and the `net.fetch`
  launch (nuntius.md §6 steps 2 and 4) and should move to them when they exist.
  In the FreeBSD VM the fetcher runs in capability mode, and `vp9-opus.webm` served at one
  16 KiB piece a second plays with every frame matching ffmpeg's hashes. That needed two
  Nuntius fixes on FreeBSD: the anonymous file is `memfd_create` (a plain `SHM_ANON`
  object refuses writes past its size), and it is written at explicit offsets (the
  read-only descriptor is a dup that shares the file offset).
- M7, keys and the page's media session (atrium-player-pip-keys.md §6 step 1): in the
  focused player, space and k play or pause, j and l step 10 s, the arrows 5 s, up and down
  change the volume, m mutes, c turns subtitles off and back on, Home, End and 0 to 9 jump.
  A volume change is reported as `ToParent::Volume`. The page's `MediaMetadata` and
  `MediaActions` are kept for now-playing; shift-N and shift-P send `ToParent::Action` when
  the page handles next or previous.

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

**Accepted by Shankar, 2026-11-06: all six recommendations below.**

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
