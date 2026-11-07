# Atrium input pipeline: Tactus

**Status:** design rev. 2, 2026-11-07; ACCEPTED by Shankar 2026-11-07 (all five §10 decisions, "proceed as recommended").
Rev. 1 (2026-05-08) planned a USB-only `uhid(4)` reader inside
frescod; what got built is several ad-hoc `hidraw(4)` readers in
frescod. Rev. 2 names the layer between the kernel and frescod
**Tactus** and gives it every input bus, not just USB.
**Owner:** Fresco display thread (Tactus, frescod routing) + Pergola.

How input devices (keyboards, mice, touchpads, touch screens,
gamepads, media keys, specialty HID) flow from the kernel to the
focused app, on every bus a FreeBSD machine has: USB, I2C, PS/2,
Bluetooth.

> **Position.** No evdev. FreeBSD ships an evdev compatibility layer
> (`/dev/input/event*`) for porting Linux input drivers and Xorg's
> libinput stack; it is exactly the Linux-shape shim our policy
> rejects (`feedback_no_linuxkpi`, `feedback_no_linux_keycodes`), and
> it would not save the work below anyway: evdev hands over Linux
> keycodes and raw axes, and libinput-class work (acceleration,
> touchpad gestures) still has to be written. Tactus is our own
> evdev-equivalent: one parsed, normalised event stream in **HID
> usages**, whatever the bus.

## 1. Why a named layer

Today frescod has four readers, each its own small parser:
`input_reader.rs` (keyboards, boot 8-byte layout only),
`pointer_reader.rs` (boot-protocol mice), `consumer.rs` (media keys,
the only one that parses report descriptors) and `injector_reader.rs`
(the dev harness). They see USB and I2C devices (through `hidbus` and
`hidraw`), nothing else. Adding touchpads, PS/2, Bluetooth and
gamepads to that would scatter the same descriptor parsing, hotplug
and normalisation across frescod. Three reasons to pull it out:

1. **Untrusted parsing.** Report descriptors and reports come from
   whatever is plugged in. A malicious USB device is a classic attack;
   its parser should not run in the compositor, which holds every
   window's pixels and mints activation tokens.
2. **More than one consumer.** frescod routes, but the lock screen,
   games (raw deltas, gamepads), tests (the span chaos and probe
   scripts) and later accessibility want the same normalised stream.
3. **One place for the hard parts:** pointer acceleration, touchpad
   contact tracking and gestures, key repeat, PS/2 scancode
   translation. These are libinput's job on Linux; here they have a
   name and an owner.

## 2. Architecture

```
 USB HID ─┐                        PS/2 kbd ── atkbd ─┐
 I2C HID ─┴─ hidbus ── /dev/hidraw*    PS/2 aux ── psm ── /dev/psm*
                 │                                    │
 Bluetooth ── Bluetooth service ── HID channels (§3.3)│
                 │                                    │
                 ▼                                    ▼
 ┌──────────────────────────────────────────────────────────────┐
 │ tactusd  (jailed, user _tactus, no network)                  │
 │   sources: hidraw, PS/2, Bluetooth HID, (dev) injector       │
 │   crate `tactus`: descriptor parser, report decoder,         │
 │     scancode→HID, pointer stage, touchpad, key repeat        │
 └──────────────────────────────┬───────────────────────────────┘
                                │ one socket, postcard (§4.0)
                                ▼
 ┌──────────────────────────────────────────────────────────────┐
 │ frescod: cursor position, hit-test, focus, pointer lock,     │
 │   activation tokens, WM-only keys, capability gating         │
 └──────────────────────────────┬───────────────────────────────┘
                                │ fresco protocol input events
                                ▼
                      Pergola in the app process
```

**Split of work.** Tactus knows devices and nothing about windows: it
turns reports into canonical events with a device id and a timestamp.
frescod knows windows and nothing about devices: it integrates
pointer motion into the cursor (it owns display geometry), hit-tests,
routes by focus and capability, and mints activation tokens. Kinetic
scrolling (flings) stays in Pergola's ScrollView (decision
2026-11-06); Tactus only marks where a finger scroll ends.

**Crate and service.** `tactus` is a pure library (no I/O): parsers,
decoders and the stages, all unit-testable on the host with recorded
descriptors and reports, like `consumer.rs` is today. `tactusd` is
the service: hotplug through `atrium-devevents`, the device fds, the
socket. It runs as `_tactus` in a jail whose devfs ruleset shows only
`hidraw*`, `psm*`, the PS/2 keyboard node (§3.2) and its socket
directory, with no network. Compromise of tactusd means forged input,
which a malicious device can do anyway, but not the compositor's
memory or other apps.

**Trust.** frescod accepts device events only from tactusd's socket
and mints activation tokens for their presses; tactusd accepts only
frescod's uid (and root). frescod has no input fallback: with tactusd
down it waits and gets no input, and its jail shows no input nodes.
The injector stays in frescod (its commands are in screen coordinates,
which are frescod's), for tests only, and stays synthetic (no tokens
unless `FRESCOD_INJECT_ACTIVATION=1`, tabula.md §2.1).

## 3. Sources

Every source produces the same thing: a device record (§4.0) and,
where it can, HID reports with a report descriptor. Non-HID buses are
translated to HID usages at the source, so everything after it sees
one vocabulary.

### 3.1 hidbus: USB and I2C

FreeBSD's `hidbus(4)` sits under USB HID (`usbhid`) and I2C HID
(`iichid`, which is how most laptop touchpads and touch screens
attach), and `hidraw(4)` exposes every top-level collection as
`/dev/hidraw<N>` with `HIDIOCGRDESC` for its descriptor. Reading
hidraw runs alongside the kernel's own drivers (`hkbd`, `hms`,
`hmt`), which keep feeding the console. This is the main source and
replaces rev. 1's `uhid(4)`, which is USB-only.

Tactus parses the descriptor in full (generalising `consumer.rs`:
collections, report ids, arrays and variables, logical/physical
ranges, units) and classifies each top-level collection (§3.5). It no
longer assumes boot-protocol layouts.

### 3.2 PS/2

Still common on laptops: the built-in keyboard and, on many, the
touchpad sit on the i8042 controller.

- **Keyboard (`atkbd`).** Scancodes (set 1 or 2), translated to HID
  page 0x07 usages with the published scancode-to-HID table (the one
  Microsoft documents for its keyboard stack), not Linux keycodes.
  Node to verify on hardware: the `atkbd` cdev (`KBD_INSTALL_CDEV`)
  detached from `kbdmux`, or a vt tty in `K_RAW` as Xorg's keyboard
  driver did. Either way the console keeps a path when tactusd is not
  running.
- **Pointer (`psm`).** `/dev/psm0` at `MOUSE_SETLEVEL 2` gives native
  packets. Plain PS/2 mice decode to buttons and deltas; Synaptics,
  Elantech and ALPS touchpads decode to finger contacts and feed the
  same touchpad stage (§5.3) as an I2C precision touchpad. Trackpoints
  arrive as a pass-through stream on the same port.

### 3.3 Bluetooth

Bluetooth carries more than input (A2DP and hands-free audio, file
transfer, LE sensors), so it is **not** part of Tactus. A system
Bluetooth service owns the controller (HCI, scanning, pairing, link
keys in Arca, which profiles each device may use) and hands each
profile's channel to the service that owns that kind of data: HID to
Tactus, audio to Lyra. Its design, including whether to extend
FreeBSD's netgraph stack (`ng_ubt`/`ng_h4`, HCI and L2CAP sockets;
Classic only, no LE above HCI) or build a new one, belongs to its own
spec (Bluetooth thread, 2026-11-07). Tactus assumes only that it gets
HID channels from that service.

For Tactus, a Bluetooth HID device is a source like hidraw: Classic
HID gives the descriptor from SDP and reports on the interrupt
channel; HID over GATT gives the Report Map and report
characteristics. Both are HID descriptors and reports, so §3.1's
parser handles them unchanged. Tactus gets the channels over a socket
from the Bluetooth service and never touches HCI. The hand-off is
[`bluetooth.md`](bluetooth.md) §10 (proposal): vinculumd passes one
hidraw-like SEQPACKET fd per device with its descriptor; packets
carry the report id separately, so tactusd prepends it before the
common decoder, and a `Dropped` packet releases held keys.

### 3.4 Virtual and test sources

- QEMU's `usb-kbd` / `usb-tablet` arrive through hidraw (the tablet is
  an absolute pointer, §5.2), which is what the VM tests use today.
- The injector line protocol (`MOVE`, `BTN`, `KEY`, `SCROLL`) stays in
  frescod (§2, Trust), tests only.
- A replay source plays recorded descriptor + report traces, so real
  devices' quirks become host tests.

### 3.5 Classification

Per top-level collection, as rev. 1:

| Usage Page | Usage | Class |
|---|---|---|
| `0x01` Generic Desktop | `0x02` Mouse | Pointer |
| `0x01` | `0x04` Joystick | Joystick |
| `0x01` | `0x05` Game Pad | Gamepad |
| `0x01` | `0x06` Keyboard | Keyboard |
| `0x01` | `0x07` Keypad | Keyboard |
| `0x01` | `0x80` System Control | System keys (power, sleep) |
| `0x0C` Consumer | `0x01` Consumer Control | Media keys |
| `0x0D` Digitizers | `0x04` Touch Screen | Touch |
| `0x0D` | `0x05` Touch Pad | Touchpad |
| `0x0D` | `0x02` Pen | Pen (later) |

Anything else is `Raw`: apps holding `input.raw_hid` can get its
reports (§4.6). One physical device commonly has several collections
(keyboard + media keys + system control); they share a device id and
report separately.

## 4. Wire format

### 4.0 tactusd to frescod

One stream socket, length-prefixed postcard messages (the Lyra
choice, lyra-v1.md), versioned by a hello. As built (T2,
`tactus::wire`) it carries `Hello`, `Device`, `Gone`, `Key`, `Button`,
`Motion`, `Absolute` (0..1 across the device) and `Scroll`; the rest
below lands with T3 and T5. Planned:

```
Device   { id, bus: Usb|I2c|Ps2|Bluetooth|Virtual, classes, vendor,
           product, name, physical_mm? }          // attach, or update
Gone     { id }
Key      { id, ts_ns, usage: u32 /* page<<16|usage */, down, repeat }
Motion   { id, ts_ns, dx, dy, raw_dx, raw_dy }    // accelerated + raw
Absolute { id, ts_ns, x, y, range_x, range_y }     // tablets, touch
Button   { id, ts_ns, usage, down }
Scroll   { id, ts_ns, axis, value_120, pixels?, phase: None|Begin|Update|End }
Gesture  { id, ts_ns, kind: Swipe|Pinch|Hold, fingers, phase, dx, dy, scale }
Contacts { id, ts_ns, contacts[] }                 // touch screens, raw touchpad
Axis     { id, ts_ns, usage, value: i16 }          // gamepads
Raw      { id, ts_ns, report }                     // Raw class only
```

`value_120` is the high-resolution wheel convention (120 per detent).
Usages are full 32-bit page:usage, so media keys (`0x000C_00CD`) need
no `0x0C00|` packing on this hop; frescod keeps the current packing
toward apps until CLASS_INPUT lands.

### 4.1 frescod to apps

As built, frescod sends `InputKey`, `InputPointerMotion`,
`InputPointerButton` and `InputPointerScroll` (fresco-protocol). The
plan below moves them to their own class:

New opcode class:

```
CLASS_INPUT = 7   (per aqueduct/src/classes.rs registry)
```

Reserves an opcode_class for input events distinct from
display/scenegraph (CLASS_DISPLAY = 1) — keeps the two evolving
independently and allows future input-only consumers (accessibility
narrators, automation tools, screen recorders).

#### Common shape

All input events carry:

```
device_id  : u32     // matches DeviceEvent.id
ts_ns      : u64     // host monotonic clock at HID-report time
window_id  : u32     // owning window (for focus context)
```

#### Keyboard ops

```
KEY_DOWN    device_id, ts_ns, window_id, hid_usage, modifiers
KEY_UP      device_id, ts_ns, window_id, hid_usage, modifiers
KEY_REPEAT  device_id, ts_ns, window_id, hid_usage, modifiers
            (synthesized by Tactus, per-user delay and rate)

modifiers : u16   bitfield: shift, ctrl, alt, super, fn, capslock
                  (extracted from current modifier-key state, NOT
                  passed through the wire from the device — modifier
                  state is frescod-side state)
```

Key codes are HID Usage Page 0x07 directly — no translation.
Modifier keys themselves emit `KEY_DOWN`/`KEY_UP` with their HID
usages (0xE0..0xE7) AND update frescod's modifier state used in
subsequent events' `modifiers` fields.

#### Pointer ops

```
POINTER_MOTION_ABS  device_id, ts_ns, window_id, x, y
                    (window-relative; for normal cursor-driven UI)
POINTER_MOTION_REL  device_id, ts_ns, window_id, dx, dy
                    (delta-only; emitted instead of ABS when device
                    is in lock mode)
POINTER_BUTTON      device_id, ts_ns, window_id, hid_usage, pressed
                    (HID Usage Page 0x09, Usage 1..N for buttons)
POINTER_SCROLL      device_id, ts_ns, window_id, axis, value
                    (axis: vertical | horizontal; value: lines or pixels)

POINTER_LOCK        window_id, mode    (client → server request)
                    mode: Unlock | Confine | Lock
POINTER_LOCK_LOST   window_id, reason  (server → client event)
                    reason: focus_lost | modal_popup | user_escape
```

**Lock modes:**

- `Unlock`: cursor moves freely; emit `POINTER_MOTION_ABS`.
- `Confine`: cursor stays inside window bounds (clamped),
  visible. Useful for drawing apps that don't want the cursor
  escaping during a stroke. Still `POINTER_MOTION_ABS`.
- `Lock`: cursor hidden, motion delivered as
  `POINTER_MOTION_REL`. Used for FPS camera, 3D modeling
  orbit, etc.

**Lock acquisition policy:**

- App calls `POINTER_LOCK Lock` after user interaction (typically
  pointer-down inside the window). Without that initial gesture,
  frescod refuses — prevents a hostile background app from grabbing
  the cursor.
- Lock survives until: (a) user presses Escape; (b) WM moves focus
  away; (c) a modal popup grabs focus; (d) app explicitly unlocks.
- On release frescod emits `POINTER_LOCK_LOST` with reason.
- Re-acquisition requires a fresh `POINTER_LOCK Lock` request after
  user gesture.

#### Gamepad ops

```
GAMEPAD_CONNECT     device_id, vendor, product, name,
                    button_count, axis_count, axes[]
                    (axes[]: usage, min, max, resolution per axis)
GAMEPAD_DISCONNECT  device_id
GAMEPAD_BUTTON      device_id, ts_ns, hid_usage, pressed
                    (button identified by HID usage, e.g. 0x09:0x01,
                    not by synthesized index)
GAMEPAD_AXIS        device_id, ts_ns, hid_usage, value
                    (value: i16 normalized -32768..32767;
                    Tactus scales the raw report to this range using
                    descriptor's logical_min/logical_max)
```

No `window_id` — gamepad events route to whoever has the gamepad
exclusively granted (see §6.3) or to the focused window if no
exclusive grant.

#### Touch ops (deferred)

```
TOUCH_BEGIN  device_id, ts_ns, window_id, slot, x, y, pressure
TOUCH_MOVE   device_id, ts_ns, window_id, slot, x, y, pressure
TOUCH_END    device_id, ts_ns, window_id, slot
```

Multi-touch slots per HID Usage Page 0x0D conventions. Not
gaming-blocking on desktop; punted.

#### Raw HID ops (specialty escape hatch)

```
RAW_HID_REPORT  device_id, ts_ns, report_bytes[]
                (only delivered to jails with input.raw_hid grant)
```

For apps that need direct HID access — flight-sim throttles, VR
trackers, MIDI-over-HID, etc. tactusd still owns the open file
descriptor; raw bytes flow through frescod to the app.

## 5. Per-device-class detail

### 5.1 Keyboards

Any descriptor layout, not just boot protocol (N-key rollover
keyboards report a bitmap). PS/2 scancodes are translated at the
source (§3.2). Key repeat is synthesised in Tactus from per-user delay
and rate, so every source repeats the same way; frescod drops repeats
of keys whose window lost focus. Modifier state stays frescod's (it is
per seat, across keyboards).

### 5.2 Pointers

Relative devices (mice, trackpoints) go through the **pointer stage**:
a per-user acceleration curve on deltas scaled by the device's
resolution when the descriptor gives one, so a 400 and a 3200 DPI
mouse feel alike. Both the accelerated and the raw delta are sent;
frescod uses raw under pointer lock (rev. 1 §4.3, unchanged).
Absolute devices (tablets, QEMU's usb-tablet, touch screens in mouse
mode) send `Absolute`, which frescod maps to the output they belong to.
Wheels: high-resolution wheel (`Resolution Multiplier`) when present,
else 120 per detent.

### 5.3 Touchpads

Two inputs, one stage: Windows Precision Touchpad reports over hidbus
(contacts with ids, confidence and the button) and PS/2 native packets
(§3.2). The stage tracks contacts and produces:

- pointer motion from one finger, with palm and thumb rejection
  (confidence bit, contact size, edge zones) and typing suppression
  shortly after a key press;
- tap-to-click, two- and three-finger taps, clickpad button areas
  or finger-count clicking (a user setting);
- two-finger scroll as `Scroll` with pixels and phases, so Pergola's
  ScrollView flings on `End`;
- `Gesture` swipe and pinch, which Forum uses for workspace and
  overview gestures and apps get for zoom.

### 5.4 Gamepads

USB gamepads / DualShock / Xbox controllers follow HID Usage Page
0x01 Usage 0x05 with vendor-defined collections. Most modern
controllers expose:

- Buttons via Usage Page 0x09
- Sticks via Usage Page 0x01 Usage 0x30..0x37 (X, Y, Z, Rx, Ry, Rz)
- Triggers via Usage Page 0x01 Usage 0x32, 0x35 (Z, Rz) typically
- D-pad via Usage Page 0x01 Usage 0x39 (Hat Switch)

Tactus decodes per the device's actual descriptor — it does
*not* maintain a per-vendor remap table. If the controller says
"button at usage 0x09:0x01 is `A`," that's the wire output. App-
side libraries (`pergola-gamepad`) may layer convenience mappings
("XInput-style: button 0 = A") on top, but the wire stays
HID-faithful.

### 5.5 Touch (deferred)

HID Usage Page 0x0D + the multi-touch report descriptor convention.
Not implemented in V1; spec'd here for forward compatibility.

## 6. Capability mediation

Unchanged from rev. 1, enforced by frescod:

```toml
[capabilities]
input.keyboard          = true   # most apps
input.pointer           = true   # most apps
input.pointer_lock      = true   # games / 3D modellers
input.gamepad           = true   # games
input.gamepad_exclusive = true   # exclusive controller grant
input.raw_hid           = false  # specialty hardware
input.system_keys       = false  # Cmd-Tab, Win etc.; WM owns them
```

Media keys go to the WM only (atrium-player-pip-keys.md §4.3), and
system-control keys (power, sleep) go to the session, never to apps.

## 7. Pergola integration

As rev. 1: `InputSignals` for ordinary UI, direct callbacks on
`SurfaceRenderer` for games, `pergola-gamepad` for XInput-style names
over HID usages, plus scroll phases and gestures for ScrollView and
zoomable views. The wire to apps stays HID-faithful.

## 8. Implementation order

1. **T1 `tactus` crate.** Full descriptor parser and report decoder
   (from `consumer.rs`), classification, keyboard and pointer decoding
   for any layout, key repeat, the pointer stage. Host tests from
   recorded descriptors (boot and NKRO keyboards, gaming mice,
   combined keyboard + consumer + system devices).
2. **T2 `tactusd`.** Hotplug, hidraw source, the socket to frescod,
   jail and `_tactus` user (commissioned like `_lyra`). frescod's
   four readers are replaced by one client.
   **Built 2026-11-07.** Crate `tactus/` (hid, decode, repeat,
   pointer, wire; 12 host tests) and `tactusd` (hidraw source,
   hotplug by devd or a 2 s rescan inside the jail, `--list`,
   `--watch`). frescod's `input_reader`, `pointer_reader` and
   `consumer` are gone; `tactus_client` maps events as they did
   (modifier keys as bits, media keys to the WM only, no tokens for
   them, repeats not yet passed to apps: the app wire has no repeat
   flag). Jail `atrium-tactusd` (services.d 38, `_tactus` 65018,
   devfs ruleset `atrium_input=26` pinned; `hidraw*` removed from
   rulesets 21 and 23), commissioned by `scripts/tactus-service.sh`.
   `scripts/vm-tactus-check.sh` (cloud amd64 VM with QEMU USB keyboard,
   tablet and mouse) passes with 0 failures: keys with shift, a token
   on each press and none on releases, one press for a held key, click
   with token, motion, scroll, a hot-plugged keyboard, and input again
   after tactusd restarts.
   Key repeats (2026-11-07): `InputKeyEvent.repeat` on the app wire
   (and fresco-split's KEY record); frescod sends a repeat only to the
   window that got the press while it keeps the keyboard, never with a
   token. Forum's toggles, snaps, workspace keys and media keys and the
   player's toggles ignore repeats; Pergola text input takes them as
   presses. VM check: a key held 1.5 s gives one press and ~30 repeats.
3. **T3 Touchpads.** Precision Touchpad over hidbus, contact tracking,
   taps, scroll phases, gestures. QEMU has no multi-touch pad, so the
   replay source carries the tests, plus a laptop run.
   **Built 2026-11-07** (host tests only so far: QEMU has no touchpad):
   the parser keeps each field's collection and physical units;
   `decode` turns Precision Touchpad reports (parallel and hybrid,
   contact count, confidence, button) into frames of contacts in mm;
   `touchpad` gives pointer motion with speed gain, one/two/three-finger
   taps (primary/secondary/middle), clickpad clicks (secondary with
   two fingers), palm (confidence) and typing rejection (fingers landing
   within 300 ms of a key press), two-finger scroll with Begin/Update/End,
   and recognises pinch and 3+ finger swipe. Scroll phases reach apps
   (`InputPointerScrollEvent.phase`, `fresco_protocol::scroll_phase`),
   content following the fingers, latched to the window the scroll
   began over. Not yet: routing swipes and pinches to Forum and apps
   (needs a gesture event on the app wire and a Forum design), setting
   a pad's Input Mode feature report ourselves (today `hmt(4)` does it
   when it attaches), the replay source, and a laptop run.
4. **T4 PS/2.** atkbd scancodes and psm packets (plain, Synaptics,
   Elantech, ALPS, trackpoint pass-through). QEMU's i8042 gives the
   keyboard and plain mouse path.
5. **T5 Gamepads.** Rev. 1 §4.4/§5.3, plus exclusive grants.
6. **T6 Bluetooth HID source**, once the Bluetooth service exists.
   *Built (tactusd side, 2026-11-07):* tactusd connects to vinculumd's
   `hid.sock` (`TACTUS_BLUETOOTH_SOCKET`, retry every 2 s) with the
   `vinculum-hid-wire` crate, takes each Attach's fd and descriptor
   (chunks included), prepends the report id and feeds the same decoder
   as hidraw; `Dropped` releases what the device holds; a bond without
   keyboard rights (bluetooth.md §7) gets no keyboard-page keys. Tested
   against a stand-in vinculumd (`tactus/tests/bluetooth.rs`); the
   end-to-end run waits on vinculumd's Classic HID (B3).
7. **Later:** touch screens and pens, CLASS_INPUT for the app wire,
   accessibility consumers (sticky keys, slow keys sit in Tactus).

## 9. Open questions

1. **PS/2 keyboard node.** atkbd cdev or vt `K_RAW` (§3.2): pick on
   real hardware; both keep the console working without tactusd.
2. **Multi-seat.** One seat for now; device records leave room for a
   seat id.
3. **Per-device settings.** Acceleration, tap-to-click and scroll
   direction per device or per class; stored with the user's display
   settings in Forum.
4. **Per-display pointer state** under lock across monitors
   (rev. 1 question 5), now that spanning exists (fresco-span.md).

## 10. Decisions

1. **Name and shape:** a named layer, Tactus, as a crate plus a
   jailed `tactusd`, not a thread inside frescod.
2. **Vocabulary:** HID usages on every bus, PS/2 translated at the
   source; no evdev, no Linux keycodes.
3. **Bluetooth:** not part of Tactus; a separate system service
   (own spec) hands HID channels to Tactus and audio to Lyra.
4. **Touchpad work in Tactus** (contacts, taps, scroll phases,
   gestures), flings in Pergola.
5. **Order:** T1-T2 first (no behaviour change, readers move out of
   frescod), then touchpads before PS/2.

## References

- [`spec/aqueduct.md`](aqueduct.md) — class registry; CLASS_INPUT
  joins as class 7.
- [`spec/pergola.md`](pergola.md) — toolkit-side surface integration.
- [`spec/fresco-surfaces.md`](fresco-surfaces.md) — game / GPU
  surface model that consumes pointer-lock + gamepad.
- `feedback_no_linuxkpi`, `feedback_no_linux_keycodes`,
  `feedback_bsd_devevents_shape` — substrate policy.
- FreeBSD `hidbus(4)`, `hidraw(4)`, `iichid(4)`, `psm(4)`, `atkbd(4)`,
  `ng_hci(4)`, `ng_l2cap(4)` man pages.
- [`spec/lyra-v1.md`](lyra-v1.md) — postcard wire; Bluetooth audio sink.
- [`spec/atrium-player-pip-keys.md`](atrium-player-pip-keys.md) — media keys.
- Microsoft "Keyboard scan code to HID usage" table; Windows Precision
  Touchpad HID spec.
- USB-IF HID Usage Tables — the canonical reference for usage page
  meanings.
