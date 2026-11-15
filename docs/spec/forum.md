# Forum — the Atrium window manager & shell (design)

Status: design, 2026-06-15. The per-session WM/shell. Companion to
`atrium-display-architecture.md` §12.5 (Forum's *role*: a privileged Insula app
holding `window-management`, the display analog of Choragus). Enforcement model:
`gpusim engine/src/forum.rs`. Launched per session by **ostiarius**; built in
**Pergola**; runs over **Fresco**.

Two defining choices were made up front: an **Atrium-native** WM model (not
floating/tiling-cloned), and a **decomposed, least-privilege** structure.

## 0. Thesis — apps have no ambient screen authority

Every mainstream WM lets an app place its own windows: it asks for a geometry, it
can move itself, raise itself, cover other windows. That ambient authority is the
root of overlay/clickjacking attacks, of focus-stealing, and of the WM being a
dumb geometry server. Atrium is a capability OS, so we invert it:

> **An app declares a *role* and *hints*; the WM places the surface. An app never
> positions, raises, covers, or focuses itself.**

That one inversion is the whole design. It buys three things no ambient WM can:

1. **Overlay/clickjacking are structurally impossible.** An app cannot draw over
   another app, over the password prompt, or over the capability dialog — it has
   no authority to place a surface anywhere but where Forum puts it (by role).
2. **Occlusion gates capabilities.** Because Forum owns placement *and* input
   routing, a surface that is not the focused/visible one simply does not receive
   input or screen capture — occlusion is a capability gate, not a z-order hint.
   The display sibling of the §9 audio-monitor property.
3. **Focus + visibility are policy signals, not pixel state.** Forum knows which
   surface the human uses and each surface's occlusion. It feeds that to the engine
   along a *gradient* (§2.5): the focused surface's render gets the **deadline lane**
   (frame-pacing, `atrium-scheduler-results.md`); partially-occluded/unfocused
   surfaces render best-effort and are composited clipped; only *fully* occluded
   ones are render-gated, so the **GPU power-gates** their idle work
   (`project_gpu_powergate`). The WM is the thing that connects *user intent* to
   *scheduling + energy*.

Role + intent + capability, the way Choragus is role + capability for sound:
*Lyra plays, the Choragus arranges who plays; apps draw, Forum arranges what's
seen.* And the arrangement itself is a **content-addressed, roamable Tessera
object** — "your workspace" is a first-class artifact you can snapshot, restore,
and roam across machines (Aqueduct/Stoa).

## 1. Constraints (already decided — we build on these)

- Forum is a **privileged Insula app** (signed manifest, libatrium, jail +
  dedicated uid, the Portcullis trust chain) holding the **`window-management`**
  capability — default-deny, granted only to the session shell. Normal apps get
  `graphics = "fresco"` (their own surfaces only).
- **Mechanism/policy split with Fresco**: Fresco composites + executes placement
  (the *mechanism*, the seat-shared engine); Forum decides cross-app *policy*
  (which surface is placed where, focus, occlusion). Fresco never decides layout;
  Forum never touches pixels.
- **Launched per session by ostiarius**, owned by the human, **seat-aware**: only
  the active session's Forum drives the engines (FUS keeps others detached).
- It launches user apps the way ostiarius launches it: **request → Portcullis →
  jaild**. Forum never execs.

## 2. The WM model (Atrium-native)

### 2.1 Surfaces are capabilities; layout is declarative

An app shares a **surface** with Fresco (a scene-graph subtree, the existing
retained-mode model). It holds a capability to *its own* surface — nothing more.
Forum holds, via `window-management`, the capability to **enumerate and arrange
all surfaces in its session**. Forum arranges by **declaring a layout** to Fresco
— "these surface ids, in these slots, this one focused" — not by issuing
imperative `move(x,y)`/`raise()` calls. Fresco composites the declared layout.
Layout is data, not a stream of commands; this is what makes a layout a
snapshot-able object (§2.4) and keeps placement atomic (no half-applied moves,
the §12.2 glitch-free-reconfig discipline).

### 2.2 Role-driven arrangement

A surface declares a **role** in its manifest (and may refine per-surface), like
audio roles drive Choragus:

| Role | Meaning | Default placement policy |
|---|---|---|
| `document` | the app's main content | the primary work area |
| `panel` | a palette/inspector attached to a document | docked beside its owner |
| `dialog` | modal/transient, owned by a surface | centered over its owner, grabs focus by policy |
| `hud` | transient overlay (volume, switcher) | Forum-reserved layer, app can't request it |
| `background` | wallpaper-class | the back layer |
| `chrome` | dock/statusbar/notifications (Forum's own shell apps) | reserved edges, §3 |

Apps give **hints** (preferred size, min/max, resizable, "I'm a tool not a
document") — never coordinates. Forum places by role + hints + the human's intent
(§2.3). An app asking for the `hud` layer, or to place over another app's surface,
is simply refused — it has no such capability.

### 2.3 Intent, not pixels — the user-facing model

Because apps don't self-place, the human's manipulations are **intents** Forum
resolves, not direct geometry edits:

- **Focus** a surface (it becomes the deadline-lane'd, input-receiving one).
- **Group / split** surfaces into a work area (document + its panels).
- **Zoom** a surface to fill, or back to the arrangement.
- **Snap** a surface to a region (the assist that makes this usable like
  float-with-snap without being floating).
- **Stash / summon** (send to a detached layer, bring back) — the switcher.

Forum applies sensible **defaults per role** (a new `document` opens in the work
area focused; a `dialog` centers over its owner) so it Just Works, and the human
reshapes by intent. This is neither floating nor tiling: it's **intent-managed
placement** — the WM owns geometry, the human owns intent, the app owns content.

### 2.4 Layouts are content-addressed objects

The current arrangement (surface roles + slots + focus, *not* the pixels) is a
small structured value. Forum stores it in **Tessera CAS** → a layout is
content-addressed, deduplicated, **snapshot-able** (save "my coding layout"),
**restorable**, and **roamable**: because Stoa/Aqueduct already roam sessions, a
layout can follow you to another machine. Reconnecting a roamed session
re-materializes the surfaces (re-requesting launches via Portcullis as needed) into
the saved layout. "Your desktop" becomes a portable artifact, not host state.

### 2.5 Visibility → scheduling + power (the engine tie)

This is the load-bearing distinctive piece. Forum is the only component that knows
each surface's **focus** and **visibility** (it declared the layout, so it knows
every surface's occlusion), so it is the natural policy input to the engine.
Visibility is a **gradient, not a binary**, and it drives the engine two separate
ways:

**(a) The compositor always clips to the visible region.** Fresco composites only
the unoccluded part of each surface — a 40%-covered surface costs ~60% of the
blend/sample/bandwidth. This is automatic from `WM_DECLARE_LAYOUT` (Fresco knows
each surface's visible rect); no app cooperation, and it handles *any* partial
occlusion. The compositor always saves proportional to coverage.

**(b) Render rate + scheduling priority scale with focus and visibility:**

| State | Render | Scheduling |
|---|---|---|
| focused, visible | full | **deadline lane** — vblank-perfect pacing |
| visible, unfocused (incl. *partially* occluded) | full, compositor-clipped | best-effort / content-rate, **not** the lane — may drop frames under load (you're not looking) |
| fully occluded / stashed | **render-gated** — not composited, last frame retained | the only hard gate → GPU idle blocks power-gate (`powergate.rs`) |

So **focus drives the *lane* (the pacing guarantee), visibility drives *whether/how
much* to render, and only *full* occlusion is a hard gate.** A partially-occluded
surface still renders (its visible part is visible) but loses the lane guarantee and
is composited clipped — graceful degradation, not gating. Power is saved *across*
the gradient: less compositing (clip), lower priority/rate (best-effort), and the
GPU power-gate reserved for the full-occlusion extreme.

Two honesties: (i) render-gating a fully-occluded surface gates only its *visual*
render — the app's non-visual CPU/audio work continues; it's a window-render
decision, not an app suspend, and the last frame is retained so un-occluding is
instant. (ii) An app that supports visible-region damage (occlusion-aware partial
render) can *additionally* skip the covered region — an opt-in win, never required.

"Coordinated not coupled": Forum publishes focus + per-surface visibility
(read-only); Fresco clips, the scheduler lanes, and the GPU driver gates, each
within its own budget. No mainstream WM drives the CPU deadline scheduler *and* GPU
power gating from visibility like this — only possible because Atrium owns the WM,
the scheduler, and the GPU power policy.

## 3. Decomposed, least-privilege structure

Only the **WM core** holds `window-management`. The visible chrome is *separate,
ordinary Insula apps* — they hold only `graphics` (their own surfaces) and talk to
the core over a small capability-gated protocol. Smallest powerful-cap surface.

```
            ┌───────────── window-management cap ─────────────┐
            │  forum-wm  (the WM core, privileged)            │
            │  - enumerate/arrange surfaces (declare layout)  │
            │  - focus + input routing                        │
            │  - publish focus/occlusion → scheduler + GPU    │
            │  - layout ↔ Tessera                             │
            └───────▲───────────────────────────▲────────────┘
   intent requests  │  (forum-ctl wire)         │  reserved chrome slots
        ┌───────────┴───────┐         ┌─────────┴──────────┐
        │ ordinary Insula apps (graphics only):             │
        │  forum-dock    app launcher (→ Portcullis)        │
        │  forum-bar     statusbar / indicators             │
        │  forum-shelf   notification shelf                 │
        │  forum-overview switcher / workspace overview     │
        └───────────────────────────────────────────────────┘
```

- **`forum-wm`** — the core. The *only* holder of `window-management`. Does §2.
- **`forum-dock`** — launches apps (it requests Portcullis → jaild; it does *not*
  hold `window-management` — it just shows icons and asks for launches, like any
  app). It's the app list + capability-prompt host (the §12.5 / pkg-install-ux UI).
- **`forum-bar`, `forum-shelf`, `forum-overview`** — ordinary apps drawing into
  `chrome`-role surfaces in Forum-reserved edges/layers. They can request *intents*
  of the core (e.g. the overview asks "focus surface X", the bar asks "stash") over
  **`forum-ctl`** — a capability-gated wire; the core authorizes each intent.

Benefit: a bug in the dock/shelf/overview cannot manipulate other apps' windows —
they never held the cap. The blast radius of the powerful capability is one small,
auditable WM core. (If the user later wants fewer processes, the "core + shell"
packaging is a config of the same protocol — but least-privilege is the default.)

## 4. The Fresco ↔ forum-wm protocol

Extends `CLASS_DISPLAY`. An app's ops over its *own* surface (the Pergola memory's
`WINDOW_CREATE/DESTROY/SET_TITLE/SET_HINTS/REQUEST_CLOSE` + the `WINDOW_RESIZED/
FOCUS_CHANGED/CLOSE_REQUESTED` events) stay app-scoped. forum-wm gets the
**cross-app** ops, gated by `window-management` (Fresco checks the peer's grant via
`portcullis-peer`, the same getpeereid path Choragus uses):

- `WM_ENUMERATE` → the session's surfaces (id, owner, role, hints, occlusion).
- `WM_DECLARE_LAYOUT { slots: [(surface_id, rect, layer)], focus: surface_id }` —
  the atomic declarative placement (§2.1); committed at a frame boundary.
- `WM_SET_RENDERING { surface_id, on }` — mark occluded surfaces non-rendering
  (§2.5).
- input routing: Forum names the focused surface; Fresco/atrium-input deliver
  input only to it (§5).

Fresco still owns the *mechanism* (compositing the declared layout, the
scanline-accurate scanout); forum-wm only declares policy.

## 5. Input routing

`atrium-input` (the BSD-native input path, HID usage codes) delivers events to
**forum-wm** (the focused-surface authority), which routes them to the focused
surface's owner. An app receives input **only** when Forum has focused it — an
unfocused/occluded/stashed surface gets nothing (the §2 occlusion-gates-capabilities
property, enforced at the routing layer, not by the app's good behavior). System
chords (the switcher hotkey, capability dialogs) are intercepted by forum-wm before
any app sees them (`atrium-input` §`input.system_keys` → Forum).

## 6. Launching apps

`forum-dock` shows the app list and, on the human's click, **requests** a launch
through Portcullis → jaild (the `atrium-launch` path ostiarius uses), owned by the
human. The new surface arrives at Fresco; forum-wm places it by role. The dock holds
no special capability — launching is a request to the TCB, authorized by the user's
grants, exactly like everywhere else.

### 6.1 The Applications view (built 2026-11-14)

Shankar (2026-11-14): apps are found in Forum, as a grid of every installed app,
not by browsing folders in Scrinium. Opening a *file* in an app is Scrinium's
(its file-opening spec: types, open-with, the file handed over as a descriptor);
this section is only about starting apps.

**What it is.** A full-screen grid of every app the catalog lists: each app's icon
and name, in name order, with a filter field on top. It belongs to `forum-dock`
(a second window of the same process), because the dock already holds the only
two things it needs: `app-launch` (the catalog and the launch request, both over
portcullisd's socket) and `forum-control` (the WM intents). Nothing new is
granted to anything.

**Opening and closing.**

| Opens | Closes |
|---|---|
| the Applications button at the left end of the dock | the same button, Escape, Super+A |
| Super+A (forum-wm sees the chord and tells the dock, below) | launching an app (once portcullisd accepted the launch) |
| | a press on the view's empty space, or the view losing focus |

The view is a `Dialog`-role window whose minimum size is the whole screen in
points, so forum-wm's existing dialog rule (grow about the centre within the work
area, take focus while it is up) gives it the work area: below the bar, above the
dock's reserved strip, with the dock and its lit Applications button still
showing. When it closes, focus goes back where forum-wm had it.

**Super+A.** forum-wm sees every key. The dock asks forum-wm, over the forum-ctl
socket, to tell it about the chord (`Intent::WaitAppsKey`); forum-wm keeps that
connection open and answers `Ack` when Super+A is pressed (once per press, all
waiting connections). The dock asks again after each answer. One intent per
connection still holds; this one just answers late.

**Filtering.** Typing goes into the filter field (it has focus while the view is
up). An app matches when every word typed appears in its name, id or description.
Matches are ranked: the name starts with what was typed; every word starts a word
of the name; every word is in the name; the rest. Each rank is in name order. The
first match is selected, so typing a few letters and Enter launches it.

**Keyboard.** Arrows move the selection (Left/Right by one, Up/Down by a row;
the filter field's caret is moved with the pointer, Home and End). Page Up and
Page Down turn pages. Enter launches the selection. Ctrl+D pins or unpins it.
Escape clears the filter first, then closes.

**Pages, not scrolling.** As many rows as fit the work area are shown; more apps
go on further pages, with page dots under the grid (pressable). The wheel and
Page Up/Down turn pages; moving the selection past the last row turns the page.

**Sizes** (resolution-independence.md §6). Tiles, gaps, the field and the dots are
`em` (they grow with the text size); the icon glyph is handed to Pergola in points
(32 pt, `icon-xl`, visual language §9) because the renderer applies the text size to
glyphs itself. The dock moves from Lucide to Phosphor with it: one icon family.

**Icons.** `[app] icon` in the manifest names an icon from the system set (the
Phosphor names, e.g. `"note-pencil"`, `"compass"`, plus a few short aliases:
`files`, `settings`, `terminal`, `browser`, `editor`). An app with no icon, or a
name not in the set, gets a tile with its name's first letter on a colour picked
from its id.

**Icons in the bundle** (built 2026-11-15). `icon` may instead be a relative path
to an SVG in the app's bundle (`"icons/app.svg"`: plain names, no `..`, no dot
files). portcullisd reads it when it builds the catalog (the jailed dock can't see
bundles), refusing a file that resolves outside the bundle (symlinks), isn't a
regular file, or is over 32 KB, and sends the text as `CatalogEntry::icon_svg`.
The dock parses it under the Navigator's strict SVG profile (navigator-svg: no
script, external references, text or filters; bounded size and work) into
filled paths, at most 64 shapes and 8192 points, and draws them as Fresco FILL
nodes filling the tile's square: the icon is the whole tile, plate included.
What the vector path can't say exactly (masks, patterns, group opacity over
several shapes) is refused and the app falls back to the system set or its
letter tile; no rasteriser runs in the dock. Bitmap icons are not taken.

**Launching.** Exactly the dock's path: `Launch { app_id }` to portcullisd, which
checks policy, picks the app's uid and jails it. The dock does this on a thread
per launch (the request stays open while the app runs). The view closes when
portcullisd answers `ReadyForFds` (started) or `AlreadyRunning` (the dock asks
forum-wm to focus that app's window, as before). If portcullisd says the app
needs the user's approval, or the launch failed, the view stays up and says so
under the grid, in the daemon's words.

**The dock row.** The dock now holds the Applications button, then the pinned
apps, then any running app that is not pinned, after a divider. A running app has
a dot under its tile. Pressing a tile launches the app or focuses its window.
In the Applications view, each tile has a pin button on hover ("Keep in Dock" /
"Remove from Dock").

- *Running* comes from forum-wm's surface list (`ListSurfaces`, every 2 s, as the
  bar polls it) matched by owner uid against the catalog, which now carries each
  app's uid for the person (`CatalogEntry::uid`, from the launch registry; `None`
  until it first ran). The catalog is fetched again when a surface with an
  unknown uid appears and each time the view opens.
- *Pinned* is a list of app ids, one per line, in `pinned` in the dock's state
  folder (below). With no file the first five apps of the catalog are pinned.
  `$FORUM_DOCK_PINS` overrides the place; outside a session it falls back to
  `$HOME/.config/forum-dock/pinned`. If the file can't be written, pins hold
  until logout.

**State folders for session components** (decided 2026-11-15). Session
components run as a per-session uid in a jail whose mounts are all read-only, so
they had nowhere to keep anything. When portcullisd launches one
(`LaunchSessionComponent`), it makes `/var/db/atrium/state/<person>/<component id>`
(the person's folder root's, 0755; the component's folder owned by the
component's session uid, 0700, re-owned at each launch in case the uid changed)
and mounts it read-write at `/atrium/state` in the jail, with
`ATRIUM_STATE_DIR=/atrium/state` in the environment. It is unmounted at teardown.
jaild's policy allows it as an `rw_subtrees` source. The session uid also gets a
nologin passwd entry, as an app's uid does: portcullisd refuses a peer it can't
name, so before this a session-launched dock could not fetch the catalog. A name that is not one
plain path segment gets no folder (the component still runs). The bar and the
others can use theirs the same way.

Forum's own components (the bar, the dock, the WM, the shelf, the overview,
Vestibulum) are not listed: they are started by the session, not by the person
(ids `org.atrium.forum-*` and `org.atrium.vestibulum`).

Code: `forum-dock/src/{apps.rs, apps_view.rs, launch.rs, pins.rs, icons.rs}`,
`forum-wm` (`WaitAppsKey`), `portcullis-ipc` (`CatalogEntry::uid`, `icon_svg`),
`portcullis-toml/src/icon.rs`, `forum-dock/src/bundle_icon.rs`, portcullisd
`prepare_state_dir`. Live checks: `scripts/vm-apps-grid-check.sh` (unjailed) and
`scripts/vm-apps-grid-jail-check.sh` (jailed, through portcullisd, then as a
session component with its state folder).

## 7. Seat / multi-session / FUS

Forum is per-session (ostiarius launches one `forum-wm` + chrome per human). The
**seat** selects the active session; only the active session's forum-wm drives
Fresco/atrium-input. On FUS, the prior session's forum-wm stays alive but detached
(its `WM_DECLARE_LAYOUT` is not the one Fresco scans out); switching re-binds. This
is already how the seat-aware engines work — Forum is just another seat-gated policy
layer, the display sibling of seat-aware Choragus.

## 8. Phasing

- **F0 — one surface, placed.** forum-wm launches, `WM_ENUMERATE` + a trivial
  `WM_DECLARE_LAYOUT` placing a single `document` surface full-area, focused;
  input routed to it. Proves the cross-app protocol + the cap gate end to end.
- **F1 — focus + the engine tie.** Two surfaces, focus switching, occluded surface
  render-gated → publish focus to the deadline lane + non-rendering to the GPU.
  The distinctive piece, measured (frame-pacing on focus; power on occlusion).
- **F2 — roles + intent.** Role-based default placement; group/split/zoom/snap/
  stash intents; the `hud`/`dialog` reserved layers (overlay-attack-proof).
- **F3 — the chrome apps.** forum-dock (launch via Portcullis) + forum-bar +
  forum-shelf + forum-overview over `forum-ctl`, least-privilege. *(Landed &
  proven in-VM: the `forum-ctl` wire (`Intent`/`Reply`, postcard, length-framed) +
  the core's `handle_intent`; `forum-overview` (list/focus surfaces); the
  `forum-control` capability gating forum-ctl (grant/deny verified); `forum-dock`
  (app catalog + unprivileged launch request to portcullisd); `forum-bar` (session
  status via ListSurfaces). forum-shelf (toasts from praecod, tabula.md §5.1; 2026-11-06,
  not yet run in-VM); the chrome apps DRAWING into their reserved-layer surfaces via
  Pergola, vs only driving intents/printing today.)*
- **F4 — layouts as objects.** Tessera-backed save/restore; Stoa-roamed layouts.

## 9. Out of scope (whose job it is)

- **Compositing, scanout, vblank timing, placement *mechanism*** → Fresco.
- **Pixel rasterization, widgets, theming** → Pergola + the SPIR-V bundle.
- **Audio policy** → Choragus (the sibling; Forum doesn't touch sound).
- **Launching / jails / capability grants** → Portcullis + jaild (Forum requests).
- **Authentication / session establishment** → ostiarius (Forum is *launched by*
  it, doesn't authenticate).

## 10. Open questions

- The concrete **intent gestures** (touch/keyboard/pointer bindings for focus/
  group/zoom/snap/stash) — UX work, F2.
- Whether `dialog` focus-grab is policy-fixed or app-requestable-within-its-own-
  surface-tree (overlay-safety must hold either way).
- ~~`forum-ctl` wire shape (likely Aqueduct class, capability-gated) — F3.~~
  **Decided (F3):** a small UDS, postcard `Intent`/`Reply` with u32 length-framing,
  one intent per connection. Gated today by same-session peer-uid; the principled
  gate is a `forum-control` capability via `portcullis-peer` (the
  `window-management` pattern), still TODO.
- Multi-monitor: each output a placement region forum-wm declares into; the §8
  output-topology crossbar is Fresco's, the region policy is Forum's. Deferred.
