# Lyra: multichannel output and spatial audio

Status: **accepted 2026-11-06** (Shankar agreed with all five §5 decisions). M1 (§2) and S1 (§3.1) are built; S2 waits for an app that needs objects. Follows
[lyra-v1.md](lyra-v1.md), which is built. Architecture: [atrium-lyra-architecture.md](atrium-lyra-architecture.md)
§12 gap 2 (channel layouts, surround, spatial).

## 1. What exists today

- **Streams can be multichannel.** An app opens a stream of 1, 2, 6 (5.1) or 8 (7.1)
  channels. lyrad converts each stream to the device's layout with a channel matrix
  (`lyra::channels`): a channel in both layouts passes at unity, a missing one folds in
  at −3 dB (centre and surrounds into left/right), and an upmix never invents channels.
- **The device was always stereo** until M1: lyrad opened `/dev/dsp` with 2 channels, so 5.1
  and 7.1 content was always downmixed, even on a 5.1 system.
- **Spatial pieces exist, unconnected.** `lyra::spatial` pans a mono object to any speaker
  layout (constant-power, between the two speakers around its azimuth). `lyra::binaural`
  renders a mono object for headphones from interaural time and level differences (no
  HRTF). Neither is reachable from the app wire.
- **Balance** (built with this proposal, 2026-11-06): each output has a left/right balance, a
  system setting kept by choragusd and applied by lyrad to the output's front pair.

## 2. Multichannel output (M)

The goal: a 5.1 or 7.1 system plays 5.1/7.1 content as is, and stereo stays bit-perfect on a
stereo system.

- **The output's layout is a setting, not a guess.** An HDA codec can say how many
  channels it can drive, not what is plugged in, so each output gets a *speaker layout*:
  stereo, 5.1 or 7.1, default stereo. It is a system setting of the output like balance,
  kept by choragusd in `/var/db/choragus/.outputs.toml`, set from the volume UI
  (`ToControl::SetLayout { output, layout }`), only by the active session, and offered
  only up to what the device takes: if the device refuses the layout, lyrad stays at the
  old one, reports it (`FromEngine::Layout`), and choragusd keeps and shows that instead.
- **lyrad opens the device with that layout.** choragusd sends `ToEngine::SetLayout { sink,
  channels }`; lyrad reopens the device with that channel count, between periods. Every
  stream's adapter is rebuilt for the new layout. Streams keep their rings and clock pages,
  so apps see one short gap, not an error. The clock stays continuous (`frame` counts stream
  frames, which don't change).
- **Apps learn the layout.** `Welcome` already carries `device_channels`; a change sends
  `ToApp::OutputChanged { device_channels }` so a player can pick its 5.1 track instead of
  stereo.
- **Balance in surround**: left-side channels (FL, SL, RL) take the left gain, right-side
  ones the right gain; FC and LFE are untouched.
- **Bit-perfect** stays as it is: a stream whose rate and layout match the device's, at
  unity gain, passes untouched.

## 3. Spatial audio (S)

Two different features go by this name. They solve different problems and cost different
amounts; they can be built in either order.

### 3.1 Headphone virtual surround (system)

**What:** on headphones, render the mix as if it came from speakers around you, so 5.1/7.1
content (films, games) keeps its surround and stereo sounds outside the head.

**How:** the output gets a *spatial* switch (off by default), which only makes sense on a
headphone output. When on, lyrad mixes to a virtual 7.1 bed, then renders each of the 8
virtual speakers binaurally at its azimuth (`lyra::binaural`, one renderer per speaker,
LFE straight to both ears) into the real stereo device. Stereo content uses the front pair
at ±30°, so it gains externalisation without being smeared.

- **No app change.** Every app benefits; nothing on the app wire.
- **Opt-outs:** `Role::Pro` streams (music production, measurement) bypass it, and so does
  any stream the user marks in the volume UI. The bypass mixes in after the binaural stage.
- **As built (S1):** `ToControl::SetSpatial { output, on }` (kept in `.outputs.toml`, active
  session only, shown as `Output.spatial`) and `ToControl::SetAppSpatial { app_id, on }` (the
  user's per-app opt-out, kept with the app's level, shown as `AppEntry.spatial`). choragusd
  sends lyrad `SetSpatial { sink, on }` and, before a stream's first level,
  `SetStreamSpatial { stream, on }` (off for Pro and opted-out apps). lyrad renders only while
  the device is stereo: on a 5.1/7.1 output the switch is kept but does nothing. A spatial
  stream's adapter mixes to the 7.1 bed; `lyra::binaural::Virtualizer` renders the bed (rear
  speakers low-passed at 5 kHz, output scaled by 1/1.75 so stereo content never gets
  louder); bypassed streams add to the device mix after it, bit-perfect. The stream gain now
  ramps per frame, so a ramp takes as long in 7.1 as in stereo.
- **Cost:** 8 renderers per device period, fixed, independent of the number of apps.
- **Quality:** today's renderer is ITD/ILD only. A measured HRTF (convolution per virtual
  speaker, with the partitioned-FFT convolver `lyra::convolve`) is the upgrade; it needs a
  freely licensed HRTF set (e.g. MIT KEMAR, SADIE II: to be checked before shipping).

### 3.2 Object audio for apps (games, VR, immersive media)

**What:** an app places sounds at positions (a footstep behind and to the left) and the
system renders them for whatever output there is: speakers via `lyra::spatial` panning,
headphones via `lyra::binaural`.

**How:** a new stream kind, `Open { params, objects: n }` with `n` mono objects (at most 16
per stream). The ring carries `n` interleaved mono channels, as any multichannel stream. A
second shared page, the **object page**, carries each object's position (azimuth,
elevation, distance) and gain. Unlike the clock page, the app writes it and lyrad only
reads it, with the same sequence lock in reverse. lyrad renders each object every period,
smoothing position changes so a jump doesn't click.

- **Hostile input:** the object page is the app's to write, so lyrad treats every field as
  untrusted: not finite → object silent, positions clamped, distance gain capped at unity
  (an object can't be louder for being close), a torn read is retried a bounded number of
  times and then keeps the last good position. lyrad maps the page read-only.
- **Cost bound:** objects per stream (16) and per app (32) are capped at Open, like the
  ring memory cap. Under load lyrad drops to cheaper rendering (plain panning instead of
  binaural) before it would miss a period (architecture §8, graceful degradation).
- **Elevation:** speaker panning in v1 is horizontal only (no height speakers in the
  layouts); elevation affects the binaural render only (spectral cue, later with HRTF).

### 3.3 Not proposed now

- **Head tracking**: no sensor path exists; on top of 3.1/3.2 it is a rotation of the
  listener, cheap to add once there is a device.
- **Ambisonics** (scene-based) input: a decoder node to the output layout; no client wants
  it yet.
- **Dolby Atmos / DTS:X bitstreams**: decoding is licensed. The architecture's exclusive
  passthrough (to an HDMI receiver that decodes) is the route, not lyrad decoding them.

## 4. Work items

| Item | What | Needs |
|---|---|---|
| M1 ✓ | Output speaker layout setting (choragusd, control wire), lyrad reopens the device with it, adapters rebuilt, `OutputChanged` to apps, balance across sides | — |
| M2 | VM check (QEMU's HDA codec is stereo only: today's check proves the fallback, sound(4) refusing 6 channels bit-perfect and lyrad staying at 2; the 5.1 path needs real hardware or a multichannel codec): 5.1 stream on a 5.1 output reaches all six channels untouched; on stereo, the downmix levels | M1 |
| S1 ✓ | Headphone virtual surround: virtual 7.1 bed + 8 binaural renderers, spatial switch per output, Pro/user bypass | M1 |
| S2 | Object streams: `Open` with objects, object page (app writes, lyrad reads, sanitised), per-period rendering to speakers or headphones, caps | M1 |
| S3 | HRTF upgrade for S1/S2 once a licence-clean HRTF set is chosen | S1 |

## 5. Decisions, with recommendations

1. **Speaker layout as a user setting per output** (not auto-detected). *Recommend yes*:
   HDA can't see what's plugged in, and a wrong guess sends sound to speakers that aren't
   there.
2. **Order: M1 now, then S1, then S2.** *Recommend*: M1 is small and unlocks both; S1 helps
   every app with no API; S2 is new API and should wait until a game or the player needs it.
3. **Virtual surround off by default.** *Recommend off*: some people dislike it, and the
   ITD/ILD renderer is a first version until S3.
4. **Object page written by the app, read-only to lyrad.** *Recommend yes*: the opposite
   direction of the clock page, same seqlock; treated as hostile input (§3.2).
5. **No Atmos/DTS decoding in lyrad.** *Recommend*: passthrough to a receiver instead.
