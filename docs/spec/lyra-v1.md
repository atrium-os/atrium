# Lyra v1: the audio service apps use

**Status:** accepted 2026-11-06 (Shankar agreed with all six §10 recommendations); being built. **Owner:** the audio thread (item P2 of
[fresco-video.md](fresco-video.md) §8).

Atrium already has an audio design and working prototypes of it. The architecture is in
[atrium-lyra-architecture.md](atrium-lyra-architecture.md) (June 2026): **lyrad** is the
real-time mixing engine, **choragusd** (Choragus) is the policy layer beside it, and the two
share the `lyra-protocol` crate. This spec does not replace that design. It says what is
missing before a real app, first the native media player (P1), can play sound through it
safely, and how to close each gap. Everything here sits inside the architecture's §5 to §9
and its A/V contract (§12.1). The player's side of it is [atrium-player.md](atrium-player.md)
(§4, M6, decision 6).

## 1. What exists today, checked in the code

| piece | state |
|---|---|
| lyrad `--control` | a mixer: N fd-passed shared-memory rings, per-stream gain with a zipper-free ramp, changes applied at a period boundary, out to OSS `/dev/dsp`. Measured in the VM on `intel-hda` |
| deadline lane | `lyrad --feed … lane` holds 0 underruns at a 5.3 ms buffer under 8 spinners (L6, `lyra/scripts/L6-RESULTS.md`) |
| choragusd `--daemon` | one front door for apps, and it already brokers the ring fd from lyrad (so that part of P2 is done). Identity from `getpeereid` and the Portcullis launch registry, but it falls back to the app's own hello when no registry is given; grants from the user's Portcullis `policy.toml`; roles and ducking; seat switching |
| Portcullis | `audio`, `microphone` and `audio-monitor` are in the manifest schema and the approval prompt |
| DSP | resampler, channel maps, spatial panning, biquad, reverb, convolution, passthrough, the C node ABI with a Capsicum-confined plugin |

## 2. What is missing

Each of these was found by reading the code, not the spec.

1. **App jails get the raw device.** `portcullis-jail` `apply_audio` and `apply_microphone`
   unhide `/dev/dsp*` in the app's devfs. An app with `audio = true` can open the codec
   directly, so it bypasses choragusd and every check in it, and with `microphone = true` it
   records with no mix in between. The architecture (§4.3) says only lyrad may hold the
   device. This is the first thing to fix.
2. **lyrad's data socket hands any caller any stream.** A connection sends a stream id and
   gets a fresh ring that *replaces* that stream's ring. Anything that can reach the socket
   can take over another app's stream. It must accept only choragusd.
3. **Both daemons are demos in how they run.** lyrad runs as root from `/root`, plays for a
   fixed number of seconds and exits, and synthesises a tone for a stream with no ring.
   choragusd is launched by ostiarius as the logged-in human. Sockets are in `/tmp`, and the
   app socket is mode 0666. Neither has a services.d manifest or a jail.
4. **The app wire is too small for a player.** It has `Register { role, caps }` and `Close`.
   There is no sample rate, channel count or format (everything is 48 kHz stereo f32), no
   pause, flush or drain, no app volume, and no events back (underrun, device change).
5. **No clock.** §12.1 settled the A/V contract (audio is the master, both sides timestamp on
   `CLOCK_MONOTONIC`) but left the wire for later. A player can't yet ask when a frame will
   reach the speaker.
6. **A hostile stream can silence everyone.** lyrad sums each stream's floats straight into
   the mix. One NaN from one app makes the whole sum NaN, which comes out as silence for
   every app on that sample. Samples are not checked.
7. **lyrad trusts the shared ring's `tail`.** The consumer reads its own position back from
   memory the producer can write. It stays in bounds (indices are taken modulo the
   capacity), but the app can move lyrad's read position, which would break the clock.
8. **No user volume control.** There is per-stream gain inside, but no per-app volume that
   persists, no master volume, no mute, and no socket for Forum or Curia to drive them.
9. **Embedded players have no owner.** Limen's `media-player` runs as the player's own user
   in a throwaway jail. To choragusd its sound would look like "the media player", not "the
   Navigator, youtube.com" or "Mail". Limen §10 also names a `media-output` capability that
   doesn't exist; the Portcullis name is `audio`.

## 3. Processes, users and jails

```
 app jail (own uid)          choragusd jail (_choragus)          lyrad jail (_lyra)
 ┌────────────┐  audio.sock  ┌─────────────────┐  lyrad.sock     ┌────────────┐  /dev/dsp
 │ player     ├─────────────►│ identity, grants├────────────────►│ mix, clock ├──────────► codec
 │            │◄─ ring fd ───┤ policy, volume  │◄─ ring fds ─────┤ resample   │
 │            │◄─ clock fd ──┤                 │                 │            │
 └────────────┘              └───────▲─────────┘                 └────────────┘
                     audio-control.sock │ (Forum, Curia only)
```

- **lyrad** runs as `_lyra` in its own jail, from a services.d manifest like frescod's.
  Its devfs ruleset shows only `dsp*`, `mixer*` and `laminar`; it has no writable mount but
  its socket directory. It needs no root: it touches only device nodes it is given. It
  listens on `/atrium/sockets/lyrad/lyrad.sock`, a directory owned by `_lyra` with group
  `_choragus` and mode 0750, and also checks that each peer's uid is `_choragus`. lyrad keeps
  serving with no deadline lane (bigger buffer, a warning), as it does today.
- **choragusd** runs as `_choragus` in its own jail. It reads the launch registry and each
  user's `policy.toml` read-only, and keeps user volumes in its own state directory
  (`/var/db/atrium/choragus/<user>.toml`, writable only by it). It listens for apps
  on one socket per audio capability, `/atrium/sockets/{audio,audio-capture,audio-monitor}/`,
  and on `/atrium/sockets/audio-control/` for the volume UI. One choragusd serves the seat. Per-session instances (architecture
  §7.1) come later; the seat gating it already does covers fast user switching.
- **Apps** get the socket directory for each audio capability they hold (`audio/` for
  `audio`, `audio-capture/` for `microphone`, `audio-monitor/` for `audio-monitor`), and
  never a device node.
- **Restart.** If lyrad exits, choragusd sees its socket close and tells every app its
  streams are gone (`StreamLost`); the app opens new ones when lyrad is back. If choragusd
  exits, lyrad closes every stream it brokered (their control connection is gone), and
  apps see their `audio.sock` connection end. The supervisor restarts both.
- **Legacy OSS apps** (ported programs that open `/dev/dsp`) get no sound in v1. The
  `cuse(3)` shim in architecture §4.3 is the way back for them, and is a later item.

## 4. Identity and grants

- **Who:** the peer uid from `getpeereid`, resolved through the launch registry to
  `(user, app id)`. An uid not in the registry is refused. The hello string an app sends today
  is dropped from the wire. The current fallback that trusts it stays only behind a
  `--dev-trust-hello` flag for the host tests.
- **May it play:** `audio` in the user's approved policy for that app, *and* the
  connection arrived on a socket that allows it. The socket an app reaches caps what it
  may ask for, and the grant is checked as well, because some jails (session
  components) see the whole socket tree. Recording later needs
  `microphone`, and tapping the mix needs `audio-monitor` (architecture §9). v1 is playback;
  the grant bits are checked at the door as they are now.
- **Seat:** a stream whose user is not the active session is held at silence, as now.
- **Embedded players.** limend already has a connection that choragusd can trust (its peer
  uid is `_limen`, the only uid served on `/atrium/sockets/audio-attribution/`; choragusd
  takes it as `--limen-uid`). When it starts a `media-player` embed, limend asks choragusd
  for an **attribution ticket** (`ToAttribution::Issue { child_uid, parent_uid }`): "the
  instance with uid U plays for the app with uid P". choragusd names P through the launch
  registry, refuses unless P's user granted it `audio`, and answers a random 128-bit
  ticket. limend passes it to the child as `AudioTicket`, right after `Attach`. The child
  puts it in `Hello` (`Client::connect(path, Some(ticket))`). choragusd accepts it only
  from a peer whose uid is U, only once, and only while limend's embed is alive: limend
  cancels it when the embed ends (`Cancel`, which also hangs up a connection that used
  it), and every ticket dies if limend's connection does. The streams are then listed,
  volumed and ducked as P's, under P's grant. The player itself runs as its provider's
  own user, which is not in the registry, so without a ticket it can't play at all; and
  its jail reaches `audio/` only if its own manifest holds `audio`. So to play, the parent
  P must hold `audio` and the player's own manifest must too. A mail app without `audio`
  cannot get sound by embedding a player. (Decision 4.)

## 5. The app wire

Messages are postcard-encoded, length-prefixed (u32 LE, at most 4 KiB), the same framing
Limen uses. Samples never cross the socket; they go through the ring.

| direction | message | payload |
|---|---|---|
| app → | `Hello` | `version: u16`, `ticket: Option<[u8; 16]>` |
| → app | `Welcome` | `device_rate`, `device_channels`, `period_frames` |
| app → | `Open` | `tag` (the app's number, echoed back), `role` (media, communication, notification, game, pro), `rate` (8000 to 192000), `channels` (1, 2, 6 or 8), `buffer_ms` (10 to 2000), `label: String` (at most 64 bytes, shown in the volume UI, e.g. "Video"). Samples in the ring are always interleaved f32; the client library converts i16 |
| → app | `Opened` | `tag`, `stream`, `ring_frames`, `latency_ns` (fd passed: the ring), (fd passed: the clock page) |
| app → | `Start`, `Pause` | `stream`. A new stream is paused until `Start`, so the app can fill the ring first |
| app → | `Flush` | `stream`: drop everything queued (a seek) |
| → app | `Flushed` | `stream`, `frame`: the stream position the next written frame will have |
| app → | `Drain` | `stream` |
| → app | `Drained` | `stream`: the last frame has reached the speaker |
| app → | `SetVolume` | `stream`, `gain: f32` (0 to 1): the app's own level, under the user's |
| app → | `Close` | `stream` |
| → app | `Underrun` | `stream`, `frames`: at most once a second |
| → app | `Changed` | `stream`, `latency_ns`: the route or device changed (a headset was plugged in) |
| → app | `Held` | `stream`, `held: bool`: policy is holding it silent (seat switch, exclusive claim) |
| → app | `StreamLost` | `stream`: lyrad went away |
| → app | `Denied` | `tag` (the `Open` it answers, else 0), `code`: not granted, too many streams, too much memory, bad format, engine unavailable, no such stream |

**Limits per app connection:** 8 streams, 4 MiB of rings in total, 64 messages queued. Over
the stream or memory limit `Open` gets `Denied`; a connection that overflows its queue is
closed. lyrad holds at most 64 streams in all.

Built (A4): `lyra_protocol::wire` holds these messages and the choragusd ⟷ lyrad ones
(`ToEngine`, `FromEngine`), and `lyra_protocol::client` is the app API. `lyra-play` is
the reference app.

**The ring.** lyrad creates it with `SHM_ANON` and passes the fd, as now, sized from
`buffer_ms` in the stream's own rate and format. lyrad keeps its read position in its own
memory and only *writes* it to the shared header for the app to see. It reads only `head`
from shared memory, and caps what it reads at the ring's size. Each stream's samples are
converted to f32 and checked before they are summed: NaN and infinity become 0 and values
are clamped to ±4.0, so a bad stream is only noise in its own slot. Resampling from the
stream's rate to the device's happens once, per stream, in lyrad (architecture §4).

## 6. The clock

A player needs one fact: when will frame *n* of my stream be heard?

- **The clock page.** With `Opened`, lyrad passes a second shared-memory fd, a 4 KiB page
  mapped read-only into the app. lyrad writes it every period under a sequence lock:

  ```c
  struct lyra_clock {            /* little-endian, one per stream */
      uint32_t seq;              /* odd while lyrad writes */
      uint32_t state;            /* 0 paused, 1 playing (a hold arrives as an event) */
      uint64_t frame;            /* stream position (stream rate) of the anchor */
      int64_t  heard_ns;         /* CLOCK_MONOTONIC when that frame reaches the speaker */
      int32_t  rate_ppm;         /* measured device speed vs nominal, parts per million */
      uint32_t flush_epoch;      /* bumps on each Flush */
      uint32_t rate;             /* the stream's nominal rate */
  };
  ```

  So frame *n* is heard at `heard_ns + (n - frame) / (rate × (1 + rate_ppm / 10⁶))`. The
  app reads it with `Stream::clock().heard_at(n)` (`lyra_protocol::clock`) whenever it
  likes, with no message and no wake. The page is created under a random name, reopened
  `O_RDONLY` for the app and unlinked at once, so the app's fd cannot be mapped writable
  (tested: `mmap(PROT_WRITE)` fails with `EACCES`).
- **How lyrad computes it.** When lyrad mixes a period it knows which stream frames went
  in. `heard_ns` = the time of that mix + the frames still queued in the device
  (`SNDCTL_DSP_GETODELAY`) at the device rate + the resampler's delay + the codec's fixed
  delay (0 on OSS path A, from the HDA driver on path B). `rate_ppm` comes from the device's
  consumed-frame count against `CLOCK_MONOTONIC` over a long window (architecture §4);
  as built, a window of at least 10 s, a new figure at most once a second, clamped to
  ±1000 ppm.
- **The position is lyrad's count**, not anything the app can write. `frame` counts frames
  lyrad has taken from the ring since `Open`. Pausing stops it; `Flush` keeps it counting
  from where the next frame will start (`Flushed { frame }`), so it never runs backwards.
- **A/V sync**, which belongs to the player (§12.1): for a video frame with timestamp *t*,
  the player finds the audio frame *n* with the same timestamp, takes its heard time from
  the page, and queues the video frame with that as `present_ns`
  ([fresco-video.md](fresco-video.md) §4). It then corrects from `EV_VIDEO_PRESENTED`. If
  the display has latency of its own beyond the vblank, the player subtracts it.
- **Accuracy target:** heard time within ±2 ms of the truth on OSS path A in the VM,
  measured by the test in §8. The A/V window that matters is EBU R37's +40/−60 ms, so this
  is far inside it.

A stream's clock says nothing about other apps or what they play.

## 7. Volume and the control socket

- **Balance and speaker layout** (added 2026-11-06, [lyra-spatial.md](lyra-spatial.md)): each
  output has a left/right balance (−1 to 1; only attenuates the far side) and a speaker layout
  (2, 6 or 8 channels), system settings of the output kept in `/var/db/choragus/.outputs.toml`
  and set by the active session (`SetBalance`, `SetLayout`; `Output` carries both).
- The level a stream plays at is `user volume for (user, app) × app's SetVolume × policy
  (ducking) × seat × master`. An app can lower itself but never raise itself past the user's
  setting. All levels are linear 0 to 1; anything out of range is clamped, and not a number
  counts as 0, so nothing on the wire or on disk can amplify.
- User volumes persist per `(user, app id)`, with the user's master level and mute, in
  `/var/db/choragus/<user>.toml` (choragusd's own directory, `_choragus` 0700, mounted rw
  into its jail only), written through a temporary file and a rename. An embedded player's
  streams use the parent's entry (A7).
- `audio-control.sock` (`/atrium/sockets/audio-control/`) is for the volume UI only: a
  restricted capability `audio-control` (like `window-management`), held by the Forum bar
  today and by Curia when it exists. It speaks its own dictionary (`ToControl`,
  `FromControl` in `lyra_protocol::wire`; `lyra_protocol::client::Control`), names apps,
  never streams, and carries no samples. Messages: `List` (the user's apps with a stream
  open: app id, label, role, level, muted, stream count; at most 16), `SetAppVolume`,
  `SetAppMute`, `SetMaster`, `SetMasterMute`, `ListOutputs`, `SetOutput`. choragusd sends
  `Apps` unasked whenever the list or a level changes.
- A control connection changes only its own user's levels and sees only that user's apps,
  and the list is empty while that user's session is not the active one. `SetOutput`
  moves the seat's output, so only the active session may send it.

## 8. Tests

Host tests run on Linux in the cloud; the rest run in the arm64 or amd64 VM with QEMU
`intel-hda` and `LYRA_DUMP` for an exact copy of what reached the device.

1. **Wire and limits** (host): every message round-trips; oversize frames, a ninth stream, a
   ring over the memory cap and a full queue are refused or disconnected.
2. **Hostile stream** (host and VM): a client writes NaN, infinity, huge values and a wild
   `head` and `tail`, while a second client plays a 1 kHz tone. The tone must come through
   untouched (Goertzel check on the dump), and lyrad's position for the bad stream must
   only move forward. A fuzzer in the shape of `fresco-split-fuzz.sh` runs this for 60 s.
3. **No device in app jails** (VM): an app launched with `audio = true` sees no `/dev/dsp*`,
   and with `audio` false has no `audio.sock`.
4. **Identity** (VM): an app at an uid not in the registry is refused; an app without `audio`
   is refused; a process that is not `_choragus` can't connect to `lyrad.sock`; a ticket from
   the wrong uid, reused, or after its embed ended is refused.
5. **Two apps, volumes** (VM): two apps at 500 and 700 Hz; `SetAppVolume` −12 dB on one
   shows −12 dB on its tone only; a communication stream ducks media by the policy's −18 dB.
6. **Clock accuracy** (VM): a client writes silence with a click at known frames; the
   clock page's predicted heard times are compared with where the clicks land in the
   device's output, read from the dump and the device position. Pass: within ±2 ms over a
   10-minute run, and `rate_ppm` stable. A click with a dry spell (the device had nothing
   queued because lyrad was late) in the 400 ms before it is reported, not judged: the gap
   moves every later frame and no prediction made before it can know it. Dry spells are a
   scheduling failure, counted and printed by the check, and the deadline lane's job; at
   least 80% of the clicks must be clear of them for the run to count.

   *Mac runs (2026-11-06, Homebrew QEMU 10.2 HVF, stock FreeBSD 16 arm64 guest, no
   Laminar).* Run 1 (4392d67) failed: lyrad measured the device speed from GETOPTR (on
   FreeBSD the frames taken into the buffer, not played) and timed each period from one
   raw GETODELAY reading; `rate_ppm` sat at ±1000. 9094ec5 replaced that with
   `DeviceClock` (a 20 s least-squares line through written − delay). Run 2: median error
   +0.001 ms and `rate_ppm` 4–8 between incidents, but the device ran dry in bursts on the
   timeshare thread (`delay` 0, gaps between periods up to 72 ms against a 21 ms buffer),
   giving errors to −196 ms. The clock now moves with a dry spell, keeping its slope, and
   lyrad logs the device's own underrun count each second. Run 3 (f15241e): no dry spells
   at all, median −0.001 ms, but one 10 s window where QEMU's HDA device (paced by a host
   timer with `-audiodev none`) consumed 8.6% slow, giving −122 ms and `rate_ppm` −1000. A
   real device's crystal doesn't do that, so this setup can't judge ±2 ms; check 6 needs a
   device with a real clock (a host audio backend, or hardware).
7. **Restart** (VM): kill lyrad while two apps play; both get `StreamLost`, reopen, and play
   again when the supervisor restarts it.

**Run so far (2026-11-06, cloud arm64 VM under QEMU TCG, `scripts/lyra-vm-check.sh`):**
2, 4, 5 and 7 pass (the turned-down app −12.0 dB, the other 0.0 dB; media ducked −18.0 dB
and back; the 1 kHz tone unchanged beside the hostile stream, nothing else above −76 dBFS,
no clipping; restart in one try). Ticket refusals in 4 are covered by the host e2e
(`choragus/tests/e2e.rs`). Still to run: 6 on the Mac's HVF VM, where timing is real.

## 9. Work items

| # | work | depends on |
|---|---|---|
| A1 | Portcullis: `audio` and `microphone` mount `audio.sock` only; no `dsp*` in app jails. Limen's §10 row says `audio` | — |
| A2 | lyrad as a service: `_lyra`, services.d manifest, devfs ruleset, `lyrad.sock` limited to `_choragus`, runs until stopped, no demo tones, `StreamLost` path | — |
| A3 | choragusd as a service: `_choragus`, its own jail, registry-only identity, sockets under `/atrium/sockets/` | A2 |
| A4 | the app wire of §5 in `lyra-protocol`, with formats, per-stream resampling, pause, flush, drain, limits, and the ring and sample checks | A2, A3 |
| A5 | the clock page of §6, and its accuracy test | A4 |
| A6 | user volume, persistence and `audio-control.sock` (§7) | A3 |
| A7 | Limen attribution tickets (§4), with the Pergola thread for limend's side | A3, A4 |
| A8 | the tests in §8, VM runs, RUNBOOK §10 updated | each of the above |

A1 to A5 are what P1 needs to play a film in sync. A6 and A7 make it right for the user.
P1 can start now against the wire in §5 and swap in the real daemon when A4 lands; a stub
`lyra-protocol::client` that writes to a file is enough for it to begin.

Later, in the architecture's own order: capture through the `microphone` grant (L5), the
OSS `cuse` shim for ported apps, the native HDA driver for an exact clock (§4.1 path B), and
passthrough formats (§12 gap 1).

## 10. Decisions, with recommendations

1. **Build on Lyra and Choragus, not a new daemon.** *Recommend yes.* The engine, policy
   layer, identity chain and lane results already exist and were measured. What is missing is
   the app surface and production hardening, which is this spec.
2. **No `/dev/dsp` in any app jail, from now on.** *Recommend yes.* Without it every audio
   permission is decorative. The cost is that ported OSS programs are silent until the
   `cuse` shim lands.
3. **One system choragusd for the seat in v1**, as `_choragus`, rather than one per logged-in
   user. *Recommend yes.* Seat gating already gives each session its own audio. Per-session
   instances can come with the multi-human desktop work.
4. **An embedded player plays only if both it and its parent hold `audio`, and its sound
   counts as the parent's** (volume, mute, ducking, the list in Forum). *Recommend yes.*
   That is what the user sees on screen, and it stops a parent without `audio` from getting
   sound through an embed.
5. **The clock is a read-only shared page, not messages.** *Recommend yes.* A player reads
   it every video frame; a message round trip each time would cost more and be less exact.
6. **App wire in postcard frames like Limen**, replacing the fixed 8- and 12-byte frames
   (the lyrad control frame between the two daemons can stay fixed-width). *Recommend yes.*
   No app uses the old wire yet, so nothing needs to keep working.
