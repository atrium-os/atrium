# Atrium input pipeline

**Status:** design, 2026-05-08.
**Owner:** atrium-devevents + fresco-server input thread + Pergola.

How HID input devices (keyboards, mice, gamepads, joysticks,
trackpads) flow from the kernel to the focused jail's app, with
the per-device-class semantics each app category needs.

> **Position.** Atrium reads input from `/dev/uhid<N>` directly.
> Not from `/dev/input/event*`. FreeBSD ships an evdev compatibility
> layer for porting Linux input drivers; that layer is exactly the
> kind of Linux-shape shim our policy rejects (`feedback_no_linuxkpi`,
> `feedback_no_linux_keycodes`). Pure FreeBSD primitives — `uhid(4)`
> + `libusbhid(3)` — for everything.

## 1. Architecture

```
┌────────────────────────────────────────────────────────────────┐
│  USB HID device (kbd, mouse, gamepad, …)                       │
└──────────────────────────┬─────────────────────────────────────┘
                           │ USB
                           ▼
┌────────────────────────────────────────────────────────────────┐
│  FreeBSD kernel: uhid(4), exposes /dev/uhid<N>                 │
│    - raw HID reports                                           │
│    - HID descriptor via USB_GET_REPORT_DESC ioctl              │
└──────────────────────────┬─────────────────────────────────────┘
                           │ devd / kqueue
                           ▼
┌────────────────────────────────────────────────────────────────┐
│  atrium-devevents (per BSD; tiny parser, not a daemon)         │
│    - watches /dev/uhid<N> appear / disappear                   │
│    - on appear: read descriptor via libusbhid                  │
│    - classify (HID Usage Page + Usage → device class)          │
│    - publish "device-attached" event                           │
└──────────────────────────┬─────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────┐
│  atrium-input-router (today: thread in fresco-server;          │
│                       future: dedicated daemon)                │
│    - opens /dev/uhid<N>, reads raw HID reports                 │
│    - decodes per parsed descriptor                             │
│    - emits canonical aqueduct events (CLASS_INPUT = 7)         │
│    - routes per WM focus + per-jail capability                 │
└──────────────────────────┬─────────────────────────────────────┘
                           │ aqueduct
                           ▼
┌────────────────────────────────────────────────────────────────┐
│  Pergola in app process                                        │
│    - subscribes to input events                                │
│    - delivers via SurfaceRenderer callbacks + signals          │
└────────────────────────────────────────────────────────────────┘
```

## 2. Device discovery

`atrium-devevents` is the per-BSD device-hotplug parser library
(`feedback_bsd_devevents_shape`). On FreeBSD it watches `devd`
events for HID device attach/detach and parses the resulting
state.

For each new `/dev/uhid<N>`:

1. `open(2)` for descriptor query (no exclusive grab — multiple
   readers OK at this stage).
2. `ioctl(USB_GET_REPORT_DESC)` retrieves the raw descriptor bytes.
3. `libusbhid` parses the descriptor into a structured form
   (collections, fields, usages, value ranges).
4. The top-level collection's Usage Page + Usage classifies the
   device:

| Usage Page | Usage | Class |
|---|---|---|
| `0x01` (Generic Desktop) | `0x02` (Mouse) | Pointer |
| `0x01` | `0x04` (Joystick) | Joystick |
| `0x01` | `0x05` (Game Pad) | Gamepad |
| `0x01` | `0x06` (Keyboard) | Keyboard |
| `0x01` | `0x07` (Keypad) | Keyboard (numeric) |
| `0x0D` (Digitizers) | `0x04` (Touch Screen) | Touch |
| `0x0D` | `0x05` (Touch Pad) | Touchpad |

5. Devices that don't match a known class are still discovered
   but tagged `Class::Raw` — apps with the right capability can
   open and parse them themselves.

`atrium-devevents` publishes a typed event:

```rust
pub enum DeviceEvent {
    Attached  { id: DeviceId, class: DeviceClass, descriptor: HidDescriptor,
                vendor: u16, product: u16, name: String },
    Detached  { id: DeviceId },
}
```

`DeviceId` is a stable identifier surviving suspend/resume (mapped
via vendor+product+serial when serial is available, else session-
local).

## 3. Routing

The atrium-input-router holds the open file descriptors for all
discovered devices and drives the read loops. Today it lives as a
thread inside fresco-server; future split into a dedicated daemon
is an internal refactor with no wire-format impact.

**Focus-driven routing.** For each input event, the router looks up
the WM's currently-focused window, identifies the owning jail,
and emits the canonical input event on that jail's aqueduct
connection. Unfocused jails see nothing.

**Exclusive routing for locked devices.** When an app holds a
pointer-lock or gamepad-exclusive grant, the router routes those
specific devices to that app even if focus moves elsewhere — until
the grant is released. Modal popups break exclusivity (see §6.2).

**Capability gating.** Per-jail manifest declares which device
classes it may receive:

```toml
[capabilities]
input.keyboard = true       # most jails
input.pointer  = true       # most jails
input.gamepad  = true       # games + couch-input apps
input.raw_hid  = false      # rare; specialty hardware apps only
```

Capability checked at subscription time; without it, the router
silently does not deliver events of that class to the jail's
aqueduct connection. Default-grant for keyboard+pointer
(everything is a UI), default-deny for gamepad and raw.

## 4. Wire format

New opcode class:

```
CLASS_INPUT = 7   (per aqueduct/src/classes.rs registry)
```

Reserves an opcode_class for input events distinct from
display/scenegraph (CLASS_DISPLAY = 1) — keeps the two evolving
independently and allows future input-only consumers (accessibility
narrators, automation tools, screen recorders).

### 4.1 Common shape

All input events carry:

```
device_id  : u32     // matches DeviceEvent.id
ts_ns      : u64     // host monotonic clock at HID-report time
window_id  : u32     // owning window (for focus context)
```

### 4.2 Keyboard ops

```
KEY_DOWN    device_id, ts_ns, window_id, hid_usage, modifiers
KEY_UP      device_id, ts_ns, window_id, hid_usage, modifiers
KEY_REPEAT  device_id, ts_ns, window_id, hid_usage, modifiers
            (synthesized by router from kernel auto-repeat)

modifiers : u16   bitfield: shift, ctrl, alt, super, fn, capslock
                  (extracted from current modifier-key state, NOT
                  passed through the wire from the device — modifier
                  state is router-side state)
```

Key codes are HID Usage Page 0x07 directly — no translation.
Modifier keys themselves emit `KEY_DOWN`/`KEY_UP` with their HID
usages (0xE0..0xE7) AND update the router's modifier state used in
subsequent events' `modifiers` fields.

### 4.3 Pointer ops

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
  router refuses — prevents a hostile background app from grabbing
  the cursor.
- Lock survives until: (a) user presses Escape; (b) WM moves focus
  away; (c) a modal popup grabs focus; (d) app explicitly unlocks.
- On release the router emits `POINTER_LOCK_LOST` with reason.
- Re-acquisition requires a fresh `POINTER_LOCK Lock` request after
  user gesture.

### 4.4 Gamepad ops

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
                    router scales raw report to this range using
                    descriptor's logical_min/logical_max)
```

No `window_id` — gamepad events route to whoever has the gamepad
exclusively granted (see §6.3) or to the focused window if no
exclusive grant.

### 4.5 Touch ops (deferred to V1)

```
TOUCH_BEGIN  device_id, ts_ns, window_id, slot, x, y, pressure
TOUCH_MOVE   device_id, ts_ns, window_id, slot, x, y, pressure
TOUCH_END    device_id, ts_ns, window_id, slot
```

Multi-touch slots per HID Usage Page 0x0D conventions. Not
gaming-blocking on desktop; punted.

### 4.6 Raw HID ops (specialty escape hatch)

```
RAW_HID_REPORT  device_id, ts_ns, report_bytes[]
                (only delivered to jails with input.raw_hid grant)
```

For apps that need direct HID access — flight-sim throttles, VR
trackers, MIDI-over-HID, etc. The router still owns the open file
descriptor (kept in one process for atomicity); raw bytes flow
through the wire.

## 5. Per-device-class detail

### 5.1 Keyboards

Already largely shipped (D1 step 2c.14), but with a substrate gap:
the existing `input_reader` thread reads `/dev/input/eventN` (the
evdev shim). Migration path:

1. Add a `/dev/uhid<N>` reader path alongside the existing evdev
   path; both feed the same canonical-event emitter.
2. Verify HID-direct path produces equivalent events on the same
   physical hardware.
3. Cut over: remove the evdev path; keep the canonical-event
   emitter unchanged.

Wire format doesn't change — we already emit HID Usage Page 0x07
codes; only the input substrate changes.

### 5.2 Pointers

Mouse motion comes from HID Usage Page 0x01 Usage 0x30/0x31 (X, Y)
in the descriptor's "relative" data field. The router maintains an
internal absolute-position cursor by accumulating deltas and
clamping to the current display geometry; emits `POINTER_MOTION_ABS`
with the resulting position when in `Unlock`/`Confine` modes, and
emits `POINTER_MOTION_REL` directly with the raw deltas when in
`Lock` mode.

Wheel scrolling: HID Usage 0x38 (Wheel) → `POINTER_SCROLL`.
Buttons: HID Usage Page 0x09 → `POINTER_BUTTON`.

DPI / acceleration: the router applies *no* OS-side acceleration
in `Lock` mode — apps get raw deltas straight from the device.
In `Unlock`/`Confine` mode it may apply a configurable
acceleration curve (operator policy), so the cursor feels right
at varying device DPI.

### 5.3 Gamepads

USB gamepads / DualShock / Xbox controllers follow HID Usage Page
0x01 Usage 0x05 with vendor-defined collections. Most modern
controllers expose:

- Buttons via Usage Page 0x09
- Sticks via Usage Page 0x01 Usage 0x30..0x37 (X, Y, Z, Rx, Ry, Rz)
- Triggers via Usage Page 0x01 Usage 0x32, 0x35 (Z, Rz) typically
- D-pad via Usage Page 0x01 Usage 0x39 (Hat Switch)

The router decodes per the device's actual descriptor — it does
*not* maintain a per-vendor remap table. If the controller says
"button at usage 0x09:0x01 is `A`," that's the wire output. App-
side libraries (`pergola-gamepad`) may layer convenience mappings
("XInput-style: button 0 = A") on top, but the wire stays
HID-faithful.

### 5.4 Touch (deferred)

HID Usage Page 0x0D + the multi-touch report descriptor convention.
Not implemented in V1; spec'd here for forward compatibility.

## 6. Capability mediation

Per-jail `[capabilities]` controls:

```toml
[capabilities]
input.keyboard         = true           # most apps
input.pointer          = true           # most apps
input.pointer_lock     = true           # games / 3D modelers
input.gamepad          = true           # games
input.gamepad_exclusive = true          # require exclusive grant
                                         # (refuses if another app
                                         # already holds a controller)
input.raw_hid          = false          # rare; specialty hardware
input.system_keys      = false          # Cmd-Tab, Win, etc.
                                         # V1 default-deny
```

The router enforces per-class delivery:

- Without `input.keyboard`, no `KEY_*` ops reach the jail.
- Without `input.gamepad`, gamepad events skip the jail even if
  it's focused.
- `input.pointer_lock` is a separate grant from `input.pointer`:
  many apps want pointer events but should never lock the cursor
  (a notification-display app, a settings panel).
- `input.system_keys` controls whether the WM forwards system
  shortcuts (Cmd-Tab, Win key, etc.) instead of consuming them.
  V1: default-deny; the WM owns these unconditionally. V2+: the
  capability-prompt UI (Forum / D3) can grant per-app.

Capabilities are checked at subscription time and on each grant
upgrade (e.g. `POINTER_LOCK Lock` request).

## 7. Pergola integration

The toolkit subscribes to input events on the app's aqueduct
connection and surfaces them as:

**Signals** for normal UI (most apps):

```rust
fn root(s: &State, input: &InputSignals) -> impl View {
    let cursor = input.pointer_pos.get();
    let pressed = input.keys_down.get();        // Set<HidUsage>

    column((
        text(format!("Mouse at {:?}", cursor)),
        if_signal(pressed.contains(0x07_29), || text("ESC pressed")),
    ))
}
```

`InputSignals` exposes high-level reactive state for the app's
focused window: `pointer_pos: Signal<(f32,f32)>`, `keys_down:
Signal<Set<HidUsage>>`, `modifiers: Signal<Modifiers>`. Mostly used
for the kind of UI logic that doesn't need event timestamps.

**Direct callbacks** on `SurfaceRenderer` for games / GPU surfaces:

```rust
impl SurfaceRenderer for MyGame {
    fn on_pointer_motion_rel(&mut self, dx: f32, dy: f32, ts: u64) {
        self.camera.yaw   += dx * self.sens;
        self.camera.pitch += dy * self.sens;
    }
    fn on_key(&mut self, usage: HidUsage, pressed: bool, ts: u64) { ... }
    fn on_gamepad_button(&mut self, dev: DeviceId, usage: HidUsage, pressed: bool) { ... }
    fn on_gamepad_axis(&mut self, dev: DeviceId, usage: HidUsage, value: i16) { ... }
}
```

Surface-renderer callbacks run synchronously, with full event
timestamps. Games typically work entirely off these and ignore the
signal layer (input-loop state is the renderer's, not Pergola's).

**`pergola-gamepad`** (separate crate, like `pergola-wgpu`) layers
XInput-style convenience over the HID-native events:

```rust
let pad = gamepad::wait_for_connection().await;
match pad.button(Button::A) {
    ButtonState::Pressed => ...,
    ...
}
```

Convenience mappings (`Button::A`, `Stick::Left`, `Trigger::Right`)
are app-side translations of HID usages; the wire stays
HID-faithful so per-vendor quirks don't leak into the platform
layer.

## 8. Implementation order

1. **`/dev/uhid<N>` reader + libusbhid descriptor parser**
   — foundation; replaces the current evdev path. ~3 days.
2. **CLASS_INPUT = 7 + ops; `KEY_*`, `POINTER_MOTION_ABS/REL`,
   `POINTER_BUTTON`, `POINTER_SCROLL`** — wire format + router
   delivery. ~2 days. Existing keyboard demos verify equivalence.
3. **`POINTER_LOCK` + `POINTER_LOCK_LOST`** — cursor lock
   semantics. Enables FPS-class games. ~2 days. Verify with a
   Bevy mouse-look demo running through `pergola-wgpu`.
4. **Gamepad classification + ops** — `GAMEPAD_*` family,
   `pergola-gamepad` adapter. ~4 days. Verify with a USB Xbox
   controller.
5. **Existing keyboard cutover** — remove evdev reader, route all
   keyboard input through `/dev/uhid<N>`. ~1 day; bounded refactor.
6. **(Future) Touch ops, capability prompt UI for
   `input.system_keys`** — D3 / Forum work.

## 9. Open questions

1. **`atrium-input-router` as a dedicated daemon?** Today the
   logic lives in fresco-server's input thread. Splitting it out
   would let the wm-vs-input concern be cleaner and allow other
   consumers (recording, automation) to read independently. Worth
   doing once the input pipeline stabilizes; probably D5 territory.

2. **Hot-pluggable USB devices during a session.** atrium-devevents
   handles attach/detach events; the router needs to gracefully
   open/close fds in response. Largely a small implementation
   detail, but worth a smoke test on a real laptop with USB
   ports + receivers.

3. **Bluetooth HID.** uhid handles USB; Bluetooth HID lands at a
   different kernel path (`/dev/bthidev*`). For V1 we punt — wired
   first. The wire format and router design accommodate it
   trivially (just another `/dev/<X>` source); that's a libusbhid-
   adjacent integration when prioritized.

4. **MIDI / specialty input devices** — typically use HID classes
   not in the table above. `Class::Raw` covers them; apps with
   `input.raw_hid` get raw report bytes. No first-class wire
   support beyond that for V1.

5. **Per-display pointer state on multi-monitor.** When the cursor
   is locked on display A and the app's window straddles a display
   boundary, what happens? Probably: lock confines to the originating
   display's window region. Worth a real test once multi-monitor
   lands.

## References

- [`spec/aqueduct.md`](aqueduct.md) — class registry; CLASS_INPUT
  joins as class 7.
- [`spec/pergola.md`](pergola.md) — toolkit-side surface integration.
- [`spec/fresco-surfaces.md`](fresco-surfaces.md) — game / GPU
  surface model that consumes pointer-lock + gamepad.
- `feedback_no_linuxkpi`, `feedback_no_linux_keycodes`,
  `feedback_bsd_devevents_shape` — substrate policy.
- FreeBSD `uhid(4)`, `libusbhid(3)` man pages.
- USB-IF HID Usage Tables — the canonical reference for usage page
  meanings.
