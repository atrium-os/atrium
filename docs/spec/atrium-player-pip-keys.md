# atrium-player: picture-in-picture, media keys and now-playing

Status: **accepted** by Shankar, 2026-11-06 (all four §7 recommendations). Owner: the media player thread (P1, atrium-player.md §7).
It needs work from Forum (forum-wm, forum-ctl), limend, frescod and choragusd. §7 lists the
decisions for Shankar.

## 1. What the user gets

- **Picture-in-picture (PiP).** A small video window that floats above other windows. It
  keeps playing when the user switches tabs or apps, and goes back into the page when
  closed or when the page asks. Every host gets it, because it lives in the player
  (limen.md §2.2, "one player, many hosts").
- **Media keys.** Play/Pause, Next, Previous and Stop on a keyboard or headset reach "the
  thing that is playing", whatever has focus.
- **Now playing.** Forum's bar and the lock screen show what is playing and can control it.

All three need one idea the system lacks today: a **media session**. That is a player
instance with a state (playing or paused), a few lines of metadata, and the actions it
accepts.

## 2. What exists

- The player is a **pinned child window** in a Limen slot (`fresco-protocol` `slot::BIND`).
  limend holds the slot. The video is a Fresco layer under the player's window, so it
  moves with the window.
- **Forum** places every surface by role (`document`, `panel`, `dialog`, `hud`,
  `background`, `chrome`) through `WM_DECLARE_LAYOUT`. Apps never give coordinates. The
  chrome apps ask the WM core for *intents* over `forum-ctl`. There is no floating layer
  and no PiP role.
- **Input**: atrium-input delivers to forum-wm, which routes to the focused surface. System
  chords are taken by forum-wm first. Media keys (HID consumer page: Play/Pause 0xCD, Next
  0xB5, Previous 0xB6, Stop 0xB7) have no route today.
- **choragusd** already knows every Media-role stream, and which app it counts for. An
  embedded player's stream is attributed to its parent by limend's audio ticket (Lyra A7),
  so choragusd sees "Navigator, youtube.com", not "the media player". It is seat-aware.
- The player already handles a few keys when focused (space, left, right).

## 3. Picture-in-picture

### 3.1 Flow

1. The user presses the player's PiP button. Or the page calls
   `requestPictureInPicture()` *during a user gesture*, and the parent sends
   `ToChild::RequestPip`.
2. The player asks limend for PiP (a child-to-limend request, not a message to the parent).
3. limend checks that a press reached the player's window within the last second. frescod
   routed that press, so it can answer; the player can't fake one. limend then asks
   forum-wm, over `forum-ctl`, `Intent::Pip { surface }`. limend gets a narrow
   `forum-pip` grant for this, not `window-management`.
4. forum-wm takes the player's window out of the slot. frescod unbinds it the way
   `BIND { rect: None }` does, but keeps it shown. forum-wm places it in a new **`pip`
   layer**: above `document` and `panel`, below `dialog`, `hud` and `chrome`, in a corner
   the user last chose.
5. The parent hears `ToParent::Pip { on: true }` and draws a placeholder in the slot,
   such as "Playing in picture-in-picture". The page gets `enterpictureinpicture`.
6. Back: the user closes PiP, presses "back to tab", or the page calls
   `exitPictureInPicture()`. forum-wm drops the window, limend binds it into the slot
   again, and the parent hears `Pip { on: false }`.

### 3.2 Rules

- **Size and place** belong to forum-wm, as for every role. The player's hints give only
  the aspect ratio and a minimum of 160×90. The window is at most a quarter of the output
  area. The user moves it with `snap` intents (a drag in PiP is a snap to the nearest
  corner) and resizes it within those bounds.
- **One PiP per session.** A second request replaces the first, as browsers do.
- **Lifetime.** PiP doesn't outlive its parent: closing the tab, or detaching the embed,
  ends it. While in PiP, limend doesn't send `Suspended` when the parent's slot is hidden.
  That is the point of PiP.
- **Overlay safety.** The `pip` layer is under `dialog` and `hud`, so it can never cover a
  capability prompt or a credential slot. Limen's unobscured-input rule (threat model
  §2.9) already stops a page's islands from taking input while PiP covers them. The PiP
  window gets pointer input for its own controls but never keyboard focus unless the user
  clicks it.
- **No PiP without video**, and none while the player is hidden or the embed is
  suspended without play.

### 3.3 Work by owner

| Owner | Work |
|---|---|
| player | PiP button, `RequestPip` and `ExitPip` handling, controls laid out for small sizes (play, close, back) |
| limen | messages `RequestPip`, `ExitPip`, `ToParent::Pip`; limend's gesture check; the slot unbind and rebind around PiP; no `Suspended` while in PiP |
| frescod | "press seen on this window within N ms" for limend; keep a slot child shown when it is unbound for PiP |
| forum-wm | the `pip` layer and role, `Intent::Pip` and `Intent::PipEnd`, placement and snap, and the `forum-pip` grant |

## 4. Media keys and now-playing

### 4.1 Where the session lives

The media-session registry goes in **choragusd** (recommended, §7 Q1):

- It already has the attribution, through the ticket. Now-playing shows "youtube.com in
  Navigator", the same name the volume list shows.
- It is seat-aware, so after fast user switching only the active seat's sessions get keys.
- Each player already holds a connection to it.

The other place is forum-wm. It knows focus but not sound or attribution, and it would
need a new connection from every player.

### 4.2 The wire (lyra-protocol, app side)

| Direction | Message | Fields |
|---|---|---|
| app → | `Session` | `state` (playing, paused, none), `title`, `artist` (at most 128 bytes each, cleaned), `actions` (a bit set: play, pause, stop, next, previous, seek backward/forward, seek to), `duration` and `position` (at most 1 a second) |
| → app | `SessionAction` | one action, with `seek_offset` or `seek_time` where it applies |
| Forum, lock screen → | `NowPlaying` | → the active session: app label, title, artist, state, actions |
| Forum, lock screen → | `Control` | an action for the active session |

The *active session* is the one that most recently went from paused to playing. A session
that stops playing stays active until another starts, so Play/Pause resumes what you just
paused. Only a session that has played since its app last had focus can be active, so a
background tab can't grab the keys by starting silent audio.

### 4.3 The key route

atrium-input sends consumer-page keys to forum-wm as system keys. forum-wm never forwards
them to the focused app. It sends `Control` to choragusd, which sends `SessionAction` to
the active session's player. The player acts on it, and tells its parent through Limen
(`Playing`, `Paused`, or a new `ToParent::Action { NextTrack | PreviousTrack | … }` for the
page's `navigator.mediaSession.setActionHandler`).

Two exceptions:

- When the focused app has the `input.media_keys` grant (a DAW, a DJ app), forum-wm gives
  it the keys instead.
- Volume keys go straight to choragusd's master volume, as today.

### 4.4 Limen messages for the page's media session

- `ToChild::MediaMetadata { title, artist, album }`: from `navigator.mediaSession.metadata`.
- `ToChild::MediaActions { actions }`: the page's action handlers, as a bit set.
- `ToParent::Action { action, seek: Option<f64> }`: the user (keys, Forum, lock screen,
  the player's own buttons) asked for an action the page handles. Play, pause and seek the
  player does itself and reports as usual. Next and previous are only ever the page's.

With no metadata from the page, the player names its session from the host's label, or
the file name in the standalone player.

### 4.5 Lock screen

ostiarius or Vestibulum draws the lock screen, and with this it asks choragusd for
`NowPlaying`. The lock screen shows **the app label and state only**. Title and artist
appear only if the user turns that on, because a page's title can be private and the lock
screen is public. Controls work without unlocking: play, pause, next, previous.

### 4.6 As built in choragusd (2026-11-06)

- **Wire** (`lyra_protocol::wire`): the app sends `ToChoragus::Session { state, title,
  artist, actions, duration, position }` and gets `ToApp::SessionAction { action,
  seek_offset, seek_time }`. `SessionState` is `None | Paused | Playing`; `None` ends the
  session. `actions` is a bit set (`wire::action::PLAY`, `PAUSE`, `STOP`, `NEXT`,
  `PREVIOUS`, `SEEK_BACKWARD`, `SEEK_FORWARD`, `SEEK_TO`). Client: `Client::session(..)`.
- **Controllers** connect to `/atrium/sockets/media-control/media-control.sock` (the
  `media-control` capability; forum-bar and forum-wm hold it) and speak `ToMedia`:
  `Hello { version, lock_screen }`, `NowPlaying`, `Control { action, seek_offset,
  seek_time }`, `Focused { app_id }` (forum-wm, on each focus change) and
  `SetLockTitles { on }` (the user's setting; refused from a lock-screen connection).
  Replies are `FromMedia::{Welcome, NowPlaying(Option<NowPlaying>), Denied}`;
  `NowPlaying` is also sent unasked whenever what that connection would see changes.
  Client: `lyra_protocol::client::Media`.
- **Actions** are `MediaAction::{Play, Pause, PlayPause, Stop, Next, Previous,
  SeekBackward, SeekForward, SeekTo}`. The first six map one to one onto the consumer
  keys. choragusd turns `PlayPause` into `Play` or `Pause` by the session's state, drops
  actions the session doesn't offer, gives seeks a 10 s default step, and clamps `SeekTo`
  to the duration.
- **Active session:** the most recent session to go to playing while eligible. Eligible
  means its app had the focus when the session began or has gained it since, judged from
  forum-wm's `Focused`. With no `Focused` reports for that user (development), every
  session is eligible. A paused session stays active. When the active session ends,
  the most recent other session that had been active takes over.
- **Seat:** a controller sees and steers only its own user's sessions, and only while
  that user's session is the seat's; on a switch it hears `NowPlaying(None)`.
- **Lock screen:** `lock_screen: true` connections get app and state only, unless the
  user's `lock_titles` (kept in `/var/db/choragus/<user>.toml`) is on.
- **Wire hardening:** `wire::recv` now refuses a frame with trailing bytes, so a message
  of one dictionary can't be half-read as another's on the wrong socket.

## 5. Security notes

- A session's metadata is untrusted text from a page. It is length-capped, has control
  characters removed, and Forum draws it as plain text.
- No app can read another app's session. Only the active session is ever shown, and only
  to Forum and the lock screen. Both need a `media-control` grant.
- PiP can't be started without a real press: limend's check uses frescod's routing, not
  the player's word.
- Keys reach a player only through choragusd, which knows which connection the player is.
  A page can't send itself `SessionAction`s.

## 6. Order of work

1. **Player, now** (needs no one; **built**, see atrium-player.md M7): keyboard shortcuts in the focused player (space and k,
   j and l for ±10 s, arrows ±5 s, up and down for volume, m to mute, c for subtitles,
   0 to 9 to jump), the Limen media-session messages (§4.4) and the player keeping them.
2. **choragusd** (**built**, §4.6): `Session`, `NowPlaying` and `Control`, the
   active-session rule, and a `media-control` grant. Tested with a fake controller
   (choragus e2e).
3. **forum-wm**: consumer-page keys to `Control`, and the `input.media_keys` exception.
   forum-bar shows now-playing.
4. **PiP**: frescod's press record and slot unbind, then forum-wm's `pip` layer and
   intents, then limend's request path, then the player's button and small layout.
5. **Lock screen** controls.

## 6.1 As built: limend and Forum (2026-11-06)

- **limend** (4a60909, 876e9aa): the child asks with op `PIP` (`Pip { embed, on,
  aspect }`, `limen::client::Child::pip`). Going in needs the `media-player` role, a
  bound (shown) slot, a sane aspect, and a press frescod routed to the child's *or the
  parent's* window within `catalogue::PIP_GESTURE_MS` (1 s; frescod `slot::op::PRESSED`).
  limend then unbinds with `keep_shown` and tells both sides with
  `ParentEvent::Pip` / `ChildEvent::Pip` (from limend, so the parent can trust it). One
  PiP at a time: a new one sends the last back. While in PiP the parent's `Place` and
  `SetVisible` are kept but change nothing, so no `Suspended`. Going back rebinds at
  the parent's last rect without a `Resumed`, or suspends if the slot is hidden by
  then. Detach, a closed tab or a crashed child ends PiP with the embed.
- **No forum-pip grant or `Intent::Pip`** (changed from §3.1 with the Fresco thread):
  frescod reports a `keep_shown` child as `WmRole::Pip` in WM_ENUMERATE while it is
  out of its slot (d5d73c2), so forum-wm places it without being told and limend's
  unbind is the only authority (limend has no Forum link at all; the `aspect` in
  `op::PIP` is checked but the window's own size sets the PiP's). forum-wm puts it in the pip layer (above documents and
  panels, below chrome, HUD and dialogs) at the user's corner, a quarter of the work
  area wide at its own aspect, at least 160×90, over zoom and snap too.
  `Intent::PipCorner { corner }` moves it (a drag that ends near a corner asks for
  that one, `nearest_corner`). A click on it gives it keyboard focus, nothing else
  does.
- **Media keys** (forum-wm `media`): consumer-page usages arrive as key events with
  `media::CONSUMER` (0x0C00) set on the usage. Play/Pause, Play, Pause, Stop, Next and
  Previous go to choragusd as `ToMedia::Control` and never to the focused app.
  forum-wm sends `ToMedia::Focused { app_id }` whenever focus moves to another app,
  and again after choragusd restarts.
- **Volume keys** (0x0CE9 up, 0x0CEA down, 0x0CE2 mute) go to choragusd's master
  level on `audio-control.sock` (forum-wm holds `audio-control` for this, beside the
  bar): up and down move it a sixteenth, snapped to the sixteenths, and up unmutes;
  mute toggles. forum-wm follows the `Apps` events for the current level and steps
  ahead of choragusd's echo, so quick presses add up. No level overlay yet.
- **forum-bar** shows now-playing: a note glyph, "title · app" (at most 36
  characters), and play/pause when the session offers it. The `input.media_keys`
  exception (a DAW gets the keys) is not built yet: it needs frescod to hand those
  keys to the focused window when forum-wm says so.

## 7. Decisions (accepted 2026-11-06: all four as recommended)

1. **Where the media-session registry lives.** *Recommend choragusd* (§4.1). The other
   choice is forum-wm.
2. **A `pip` layer in Forum**, between `panel` and `dialog`, placed by the WM.
   *Recommend yes.*
3. **Media keys go to the active session, not the focused app**, except for apps granted
   `input.media_keys`. *Recommend yes.*
4. **Lock screen shows the app and state only**; title and artist are opt-in.
   *Recommend yes.*
