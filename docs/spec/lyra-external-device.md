# Lyra: external output devices (Bluetooth first)

Status: **accepted** by Shankar, 2026-11-07 (all four §9 recommendations); the Bluetooth thread agreed (persistent id, initial delay,
low rates and reconfiguration added at its request). The lyrad side of [bluetooth.md](bluetooth.md) §11
(Vinculum B5). Builds on [lyra-v1.md](lyra-v1.md) (the app wire, the clock page, choragusd's
sockets) and [atrium-player-pip-keys.md](atrium-player-pip-keys.md) §4.6 (media sessions).

## 1. What is needed

A Bluetooth headset is an output device whose samples leave through a process, not a
kernel driver: `vinculum-audio` encodes them (SBC first) and sends them over an AVDTP
channel. lyrad must mix for it as it mixes for the OSS device, and the clock page must say
when a frame is *heard*, including the 100–300 ms the headset itself adds, so the player
keeps lip sync.

The same backend later serves any device driven from user space: LE Audio, a network
speaker, a USB device we drive ourselves.

## 2. Shape

```
 apps ─ring─► lyrad ──ring (device mix)──► vinculum-audio ──AVDTP──► vinculumd ──► headset
                ▲     ◄──device page (delay, state)──┘   │
                │                                         │ audio-device/ (announce,
          choragusd ◄─────────────────────────────────────┘  volume, gone)
                ▲
                └── media-control/ (AVRCP buttons, as today)
```

- **vinculum-audio never talks to lyrad.** lyrad keeps its single trusted peer
  (`_choragus`). The device announces itself to **choragusd** on a new socket,
  `/atrium/sockets/audio-device/`, behind a new restricted Portcullis capability
  **`audio-device`**. Only `vinculum-audio` holds it, and later other device drivers.
- **choragusd brokers, as it does for app streams.** It checks the announcement, asks lyrad
  for the device (`ToEngine::AddDevice`), and passes the two fds lyrad returns to
  `vinculum-audio`:
  - the ring, consumer end: the device's mix, interleaved f32 at the device rate;
  - the device page, a small shared page the device writes and lyrad reads (§4).
- **Output choice stays choragusd's.** The headset appears as an output (`DeviceKind::Bluetooth`,
  named by the device) in `ToControl::ListOutputs`, and choragusd's rules decide whether it
  becomes current (§6).

## 3. Pacing and the clock domain

The device is the clock, as the OSS device is today. lyrad mixes a period whenever the
ring has room for one, and otherwise sleeps until it should have room (one period at the
nominal rate, absolute `CLOCK_MONOTONIC` deadlines). `vinculum-audio` reads the ring at its
own pace:
- A2DP: an encode timer at the negotiated rate on `CLOCK_MONOTONIC` (A2DP has no clock of
  its own; the sink adapts). The domain is "system clock at 44.1 or 48 kHz".
- LE Audio, later: the controller's ISO interval, which may drift from the system clock.
  lyrad measures it either way.

The ring holds enough for the device's largest read plus two of lyrad's periods (about
40 ms for A2DP). A larger ring would only add delay.

**The clock.** lyrad's `DeviceClock` (9094ec5) takes, each period, the frame the listener
hears now. For the OSS device that is `written − GETODELAY`. For an external device it is
`ring.consumed() − page.delay_frames`. The same fit gives `rate_ppm` and the heard time of
each period's first frame, so the clock page and the apps' `heard_at` work unchanged. The
`starved` flag is "the ring was empty when the device read".

## 4. The device page

One 64-byte page, mapped writable by the device and read-only by lyrad, under a sequence
lock:

| field | meaning |
|---|---|
| `seq` | odd while the device writes |
| `state` | 0 stopped, 1 streaming (lyrad mixes only while streaming) |
| `delay_frames` | frames taken from the ring but not yet heard: encoder queue, L2CAP and controller buffers, plus the sink's AVDTP delay report (A2DP 1.3), converted to frames |
| `underruns` | times the device found the ring empty (counter) |
| `rate` | the frame rate it reads at (must equal the announced rate) |

lyrad trusts none of it beyond its effect on this one device. `delay_frames` is clamped to
one second (logged if it was more), a page that never settles (a writer stuck mid-update)
counts as stopped, and nothing in it is a length or an index into anything. A hostile
`vinculum-audio` can make its own device's clock wrong or its audio silent, which it could do
anyway by not sending packets. It cannot reach any other stream or lyrad's memory.

## 5. The wire: `audio-device/`

`ToDevice` (device → choragusd):
- `Hello { version }`
- `Announce { persistent_id: u64, name, kind: Headphones | Headset | Speaker | Car, rate,
  channels, hw_volume: bool, initial_delay_frames }`.
  - `persistent_id` is the Vinculum device handle: stable across reconnects, and not the
    Bluetooth address. choragusd keys the device's remembered volume and routing on it.
  - `rate` is one of 8000, 16000, 24000, 32000, 44100 or 48000 (HFP and LE Audio need the
    low ones), and `channels` is 1 or 2.
  - `initial_delay_frames` is the device's estimate (codec plus controller) until its first
    page write, since A2DP delay reports arrive only after streaming starts. It is clamped
    like the page's.
  - `name` is cleaned like session titles, because it is the remote device's own name.
- `Volume { volume }`: the headset's volume buttons (AVRCP absolute volume), 0 to 1.
- `Gone`: the link dropped. Closing the socket means the same.

**Reconfiguration** (an AVDTP reconfigure, or A2DP giving way to HFP for a call) can change
the rate or channels. The device sends `Gone` and then a new `Announce` on the same socket;
nothing on the page changes rate live. A new announcement with the same `persistent_id`
within 2 s of its `Gone` is the same output: it keeps its place in the output list and
stays current, and choragusd neither pauses media nor switches back to the speakers.

`FromDevice` (choragusd → device):
- `Ready { device }`, followed by the two fds.
- `SetVolume { volume }`: the output's volume changed in the Forum bar (only when
  `hw_volume`).
- `Denied { code }`.

**Volume.** With `hw_volume`, the headset does the attenuation (that is what absolute volume
is for), so choragusd sends the user's master volume to the device and leaves it out of the
digital gain. Without it, the master volume stays digital, as for speakers. Volume therefore
travels on the device socket, and `vinculum-audio` doesn't need `audio-control`, which would
also let it change every app's level.

**Buttons.** Play, pause, next and previous from the headset use `media-control`
(`ToMedia::Control`), as bluetooth.md §11 says. Now-playing text sent to a car display or
headset counts as a lock screen: app and state only, unless the user turned titles on.

## 6. Connect, switch, disconnect

- **Connect.** The device is added and becomes the current output if it is headphones or a
  headset (what a user expects when they put headphones on). Speakers and cars don't take
  over by themselves. Every stream moves to it, and each clock page jumps once, by the
  headset's delay.
- **One output at a time in this version.** lyrad v1 has a single sink, so switching closes
  one backend and opens the other between periods, the same way a layout change reopens
  the OSS device. Playing on speakers and headphones at once, or on two headsets, waits for
  multi-sink lyrad.
- **Disconnect.** After 2 s without a re-announcement (§5), choragusd removes the device and returns to the previous output. If the
  device was current and was headphones or a headset, the active media session is
  paused first (`SessionAction::Pause`), so the music doesn't carry on through the speakers.

## 7. A side benefit for the clock test

A test device consumer (`lyra-vmtest device`) paced by `CLOCK_MONOTONIC` is a device with
a perfectly stable clock. Check 6 can run against it in the cloud VM, which the virtual HDA
card's speed swings prevented (lyra-v1.md §8). That checks lyrad's clock arithmetic and the
external path. The OSS path still needs real hardware.

## 8. Build steps

| # | work | gate |
|---|---|---|
| E1 | lyrad `Sink` trait (OSS, external); `AddDevice`/`RemoveDevice`; device page; pacing by ring room | host test: a fake device thread with a 150 ms delay; heard times within 1 ms |
| E2 | choragusd `audio-device/`, the `audio-device` capability, `DeviceKind::Bluetooth`, switch and disconnect rules, hardware volume | choragus e2e with a fake device: announce, switch, volume both ways, disconnect pauses the session |
| E3 | `lyra-vmtest device` and check 6 against it in the VM | ±2 ms over 10 minutes in the cloud VM |
| E4 | with the Bluetooth thread: vinculum-audio on the real path | B5's gate: a Bumble sink decodes the tone, A/V offset within lyra-v1 §8's bound |

E1 to E3 need nothing from Vinculum, so they can start now.

## 9. Decisions, with recommendations

1. **Devices announce to choragusd, not lyrad**, through a new restricted `audio-device`
   capability; lyrad keeps one trusted peer. *Recommend yes.*
2. **Hardware volume on the device socket**, so `vinculum-audio` holds `audio-device` and
   `media-control` but not `audio-control`. *Recommend yes.* (bluetooth.md §11 proposed
   `audio-control`; this narrows it.)
3. **One output at a time** until lyrad goes multi-sink. *Recommend yes.*
4. **Headphones take over when they connect, and media pauses when they leave.** Speakers
   and cars don't take over. *Recommend yes.*
