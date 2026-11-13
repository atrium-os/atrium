# Lyra: hi-res and bit-perfect output

Status: **building** (2026-11-13). Shankar asked for audiophile DACs to work at their full
rate (384 kHz or more, when the DAC offers it) and bit-perfect. Builds on
[lyra-v1.md](lyra-v1.md) (the engine, the app wire) and the bit-perfect device settings in
[atrium-lyra-architecture.md](atrium-lyra-architecture.md) §4.1.

## 1. What was there

- The built-in device always opened at **48 kHz** (`Config::rate`). Every stream was resampled to it.
- The output was **16-bit**. Each mix sample became `(x · 32767) as i16`, which truncates and is
  not the inverse of an integer source's `s / 32768`, so even a 16-bit source did not come out
  bit for bit.
- Apps could open streams at **8 to 192 kHz**.
- The first real DACs (2026-11-13) offered only 32-bit (EarMen Sparrow, ESS ES9281) or only
  packed 24-bit (AudioQuest DragonFly Red). In bit-perfect mode lyrad refused them until the
  sink learned those formats.

## 2. What bit-perfect means here

A period is **bit-perfect** when the samples the device receives are exactly the integers the
app's source had. lyrad holds that when all of these are true:

1. **One stream is sounding.** Two streams are a mix.
2. **It runs at the device's rate and layout.** The adapter's ratio is exactly 1 with phase 0,
   and the channel map is the identity, so each output is `carry[i]·1 + carry[i+1]·0`.
3. **It is at unity gain** (user level, master and ducking together are 0 dB), with no ramp in
   progress, and the output's balance is centred. Gain ramps now land exactly on their target.
   An f32 one-pole ramp used to stall about 2e-4 short of 1.0 at 384 kHz.
4. **It is outside virtual surround.** A Pro stream, or one the user opted out, bypasses the
   binaural stage.
5. **The device is wide enough** for the source, and the kernel does not convert. The sink
   asks for S32. A device in `dev.pcm.N.bitperfect=1` answers with its own depth (S16, S24 or
   S32), and lyrad writes that depth.

The ring is f32, which holds 24 significant bits, so **a 16- or 24-bit source is carried
exactly**. Output quantization is `round(x · 2^(bits−1))`, clamped: the exact inverse of
`s / 2^(bits−1)`. A 32-bit integer source loses its low 8 bits in the ring. No DAC
resolves them, and carrying them would need an integer ring format.

lyrad logs every change: `lyrad: bit-perfect: stream 3 at 192000 Hz, S32` or
`lyrad: mixing at 192000 Hz, S32`.

## 3. The device rate follows the music

- **A stream that starts while nothing else is playing sets the device rate.** lyrad reopens
  the device at that rate before the stream's first frame, so a 192 kHz track plays at
  192 kHz untouched. A stream that starts while others play is resampled to the current rate,
  so a notification never forces a reopen (and a gap) under the music.
- **A rate the device refuses** (bit-perfect mode returns its nearest rate) sends lyrad back
  to the configured 48 kHz. lyrad never asks for that rate again while it runs, and the stream
  is resampled.
- **Nothing switches back** when playback stops: the next stream that starts alone decides.
- **Periods scale with the rate.** The period is the 48 kHz base (128 frames), doubled for
  each doubling of the rate, so it lasts about 2.7 ms at any rate. That is 1024 frames at
  384 kHz.
- Spatial audio's measured HRTFs are 48 kHz. At other rates the model renders, as for external
  devices.
- External devices (Bluetooth) keep their own rate, as before.

## 4. Limits

- App streams: **8 kHz to 768 kHz** (`wire::MAX_RATE`), rings up to 2²¹ frames (2.7 s at
  768 kHz).
- DSD: DoP rides this path as 24-bit PCM at 176.4 kHz (DSD64) or 352.8 kHz (DSD128) once a
  player sends it. passthrough.rs packs it. Native DSD needs a driver format.

## 5. Open items

- **Hardware volume:** done, see §8.
- **Show it.** A `FromEngine::BitPerfect { stream }` event for choragusd and a badge in the
  Forum bar's output menu.
- **bitperfect on hot-plug.** `lyra-service.sh` sets `dev.pcm.N.bitperfect=1` for the cards
  present at start. A DAC plugged in later needs it from dispensatord when it hands the card to
  lyrad (device-assignment.md, the `pcm` rule).
- **Gapless at a rate change.** A player that opens each track as a new stream gets a reopen
  between tracks of different rates (inevitable), and none between tracks at the same rate.
- **USB on the Mac's VM.** qemu's EHCI passes a high-speed DAC through cleanly; its xHCI
  buzzes, and full-speed DACs on its UHCI companions panicked the Mac (2026-11-13). Real
  hi-res listening tests belong on real hardware.

## 6. Tests

Host (`lyra`, run on FreeBSD because the rings need its shared memory):
`integer_sources_come_out_bit_for_bit` (every 257th 24-bit value, and 16-bit at every
depth), `out_of_range_and_non_finite_samples_clamp`, `a_ramp_to_unity_lands_exactly_on_it`,
`the_period_keeps_its_length_as_the_rate_rises`,
`a_stream_that_starts_alone_sets_the_device_rate`,
`a_stream_opened_while_the_device_follows_another_rate_is_converted_to_it`,
`a_lone_stream_at_the_device_rate_and_unity_gain_is_bit_perfect` (24-bit at 384 kHz through
the engine and the S24 encoder, integer for integer), `a_second_stream_or_resampling_ends_bit_perfect`.

## 7. As built (2026-11-13)

- `gain.rs`: ramps land exactly on their target. `oss.rs`: `quantize`/`encode_f32`
  (round, clamp, S16/S24/S32), `OssSink::open_as` asks for S32, and `write_f32`.
  `server.rs`: `period_for`, `Engine::want_rate`/`refused_rates`, rate-follow on `Start`,
  `Adapter::is_identity` and `stream_rate`, the bit-perfect detector and its log line, and the
  run loop reopening at the wanted rate. `wire::MAX_RATE` 768 kHz, `MAX_RING_FRAMES` 2²¹.
- Fixed on the way: a stream opened while the device ran at a followed rate got a ratio
  against 48 kHz. `Cmd::Open` now re-points its adapter at the engine's rate.
- Unit tests: 97 of lyra's 102 pass in the Mac VM. The 5 that fail need a C compiler for the
  plugin, or no `/dev/dsp`, and fail the same way without these changes. lyra-protocol 28/28
  and choragus 19/19 pass.
- End to end in the Mac VM on qemu's virtual HDA card (16-bit, up to 96 kHz),
  `scripts/usb-dac-check.sh` with `RATE=`:
  - 44.1 kHz: `device rate 48000 -> 44100`, then `bit-perfect: stream 1 at 44100 Hz, S16`;
  - 48 kHz: `bit-perfect` with no reopen;
  - 96 kHz: reopened at 96 kHz, `bit-perfect`, periods of 256 frames, 0 underruns;
  - 192 kHz: `the device refused 192000 Hz (... got 96000)`, back to 48 kHz, resampled.
- No real hi-res DAC has played through this yet. The Sparrow (S32, up to 384 kHz) is the
  first to try, on real hardware or on the Mac's EHCI passthrough.

## 8. Where the volume goes (2026-11-13)

Bit-perfect needs the mix at 0 dB, so the volume has to go somewhere else. Shankar: a DAC with
no volume control of its own usually feeds an amplifier whose knob is the volume, so its volume
is fixed at 100%. Each output therefore has a **volume mode** (`wire::VolumeMode`):

| mode | master volume | when |
|---|---|---|
| `Hardware` | the device's own control (the card's `vol` mixer, or an external device's absolute volume); the mix stays at 100% | automatic when the device has a control |
| `Fixed` | none: 100% always (mute still mutes) | the user's choice, for line out to an amplifier |
| `Digital` | in the mix (bit-perfect only at 100%) | automatic when the device has no control, or the user's choice |

- **Headphone DACs without a control stay `Digital` by default.** A dongle such as the Sparrow
  or DragonFly drives headphones itself, so a fixed 100% would be far too loud. `Fixed` is the
  user's choice ("Fixed volume (line out)" in the Forum bar's volume menu), never automatic.
- **The choice is kept per card**: `card:<sound(4) name>` in choragusd's `.outputs.toml`, so
  fixing a USB DAC never fixes the laptop speakers when it is unplugged.
- **lyrad** opens `/dev/mixer` beside `/dev/dsp`. sound(4) emulates only `pcm` in software,
  so a `vol` control is the hardware's. lyrad reports `FromEngine::BuiltIn { name, hw_volume }`
  at each open, and again to each new session, and applies `ToEngine::SetHwVolume` (also at
  each reopen).
- **choragusd** applies the master in the mix only for `Digital`, sends it to the device for
  `Hardware`, and sets a device control to 100% for `Fixed` and `Digital`, so a control left
  low can't hold the level down. A Fixed output ignores the master but not mute.
  `ToControl::SetVolumeMode { output, mode }` (`None`: automatic) refuses `Hardware` on a device
  with no control.
- **Forum bar**: the switch, a disabled slider and "Fixed at 100%: use your amplifier" while
  fixed.

Checked in the Mac VM on virtual HDA, which has a `vol` control, with the master at 0.5:
- automatic → `Hardware`: mixer `vol` 0.50, and the tone `bit-perfect`;
- `Digital`: mixer 1.00, and the tone mixed (scaled by 0.5);
- `Fixed`: mixer 1.00, and the tone `bit-perfect`;
- automatic again: mixer 0.50.

Unit tests: `resolve_volume_mode`, `master_in_mix`, the per-card setting, the mixer ioctl
numbers (checked against the sysroot's soundcard.h), and the bar's switch and label.

Still open: reading the DAC's output terminal type (UAC headphones vs line connector) to
suggest `Fixed` for a line-out DAC. That needs dispensatord, which sees the USB descriptors.
