# Forum status items (bar apps without windows)

Status: building (2026-11-12). Asked for by Shankar: apps that live in the
bar with no window (a VPN, a clipboard manager, a mixer). He chose
bar-drawn menus first, app-drawn panels later.

## 1. What it is

An app with the `status-item` capability puts one item in the bar: an
icon and a menu. **The bar draws both.** The app sends a description (a
menu model) and hears which item the user picked or where they moved a
slider. It never draws a pixel in the bar or its menu, so a status item
cannot pass for the system's own controls and cannot cover them
(fresco-popups.md §2).

- **Where it sits.** Status items have their own group in the bar, left
  of the system icons and set apart from them, in the order the apps
  added them. The system icons (controller, volume, appearance, lock) are
  the bar's own and never come from an app.
- **The icon** is a vector icon from the app's bundle (Shankar,
  2026-11-12: vector, not bitmap): stroke paths and circles in a 0..24
  viewBox, the dock's icon style, sent with each `Set` so it can follow
  the app's state (a VPN connected or not). At most 8 KiB of ASCII
  (forum-wm drops anything else); the bar parses it and draws it as
  strokes in its own colour, and refuses one that is too complex (64
  paths, 512 segments) or reaches far outside its box. Otherwise, and
  when there is none, the app's Phosphor glyph. Either way it is drawn in
  the status-item group only, so an icon that looks like a system icon
  still sits among apps.
- **The menu** is drawn as the bar's other menus are (Pergola
  `MenuButton::attached`), headed by the app's name from the launch
  registry, never a name the app supplies.
- **One item per app.** Setting it again replaces it; the app's
  connection closing removes it.

## 2. What hangs from the icon

The app chooses one of two, and can switch with its next `Set`:

- **A menu** (keyboard-driven, quick): up to 32 entries, each one of
  `Action { label, enabled }` and `Check` / `Radio { label, on, enabled }`
  (picked → `Picked { index }`, the app sends the new state back in its
  next `Set`), `Slider { label, value }` (0 to 1; moved → `Slid { index,
  value }`, while dragging and with Left/Right; the menu stays open) and
  `Separator`. Labels at most 64 characters. Drawn as Pergola `MenuItem`s,
  under a heading with the app's name.
- **A panel** (Shankar, 2026-11-12: "support everything including text,
  grouping, layout"): the app describes it, the bar lays it out and draws
  it with its own widgets, under the app's name. Nodes:
  - `Heading { text }`, `Text { text, style }` (Body, Secondary, Caption,
    Mono; wrapped to the width, `\n` breaks a line), `Separator`.
  - Layout: `Group { title, children }` (a framed section, untitled when
    the title is empty), `Row { children }` (side by side, equal widths),
    `Column { children }` (one under another, a row's cell).
  - Controls, each with an `id` the app picks: `Toggle` and `Check { id,
    label, on, enabled }` → `On(bool)`; `Slider { id, label, value,
    enabled }` → `Level(0..1)` while dragging; `Choice { id, options,
    selected, segmented, enabled }` (radio list or segmented) →
    `Chosen(i)`; `Button { id, label, primary, enabled }` → `Pressed`.
  - `Progress { label, value }`.
  - `TextField { id, label, placeholder, value, enabled }` → `Text(..)`
    when the user presses Enter. Never obscured: the bar's surfaces do
    not ask for passwords (fresco-popups.md §2), so there is no secret
    field. The field takes the app's `value` only when it changes, so an
    unrelated update does not wipe what the user is typing.

  What the user does comes back as `Changed { id, value }`; the app sends
  the new state in its next `Set` (a control moves at once in the bar, so
  a drag is not undone while the app answers, and takes the app's value
  when it comes). forum-wm passes on only a value that control takes,
  and only if it is enabled.

  Limits (forum-wm cuts to them): width 200 to 400 px; 64 nodes in all;
  4 levels of nesting; text 512 characters, labels 64; 8 options a choice;
  560 px tall (nodes past it are not drawn).

## 3. Wire

forum-wm serves `/atrium/sockets/forum-status/forum-status.sock` (postcard,
`forum_ctl` framing). Portcullis mounts it only into jails holding
`status-item`; forum-wm also resolves the peer through the launch registry
and refuses an unregistered one (development: `FORUM_STATUS_ANY=1`).

- App → WM: `Set { glyph: char, svg: Option<String>, tooltip: String, content: Menu(..) |
  Panel { width, nodes } }`, `Remove`.
- WM → App: `Picked { index }`, `Slid { index, value }`, `Changed { id,
  value }`, `Denied { message }`.

The bar reads the items with forum-ctl `ListStatusItems` (app id, glyph,
tooltip, content, generation; the name shown is the registry id's last
part) and reports the user's actions with `StatusPick` / `StatusSlide`
(menus) and `StatusInput { app_id, id, value }` (panels); forum-wm relays
them to the app's connection. Sample app: `forum-ctl/examples/status-item.rs`
(a clipboard history panel; `--menu` for the menu form). Code:
`forum-wm/src/status.rs`, `forum-bar/src/status_panel.rs`.

Checked live (2026-11-12, amd64 VM, headless frescod + forum-wm +
forum-bar + the sample): the panel opens from its icon, a toggle and a
button reach the app, the app's answer redraws the panel, a disabled
choice does not move. The bar's window had been 24 px tall while the bar
draws 38, so frescod refused every bar drop-down (anchor outside the
window); forum-wm now gives it 38.

## 4. Panels the app draws

An item whose content is `StatusContent::Drawn { width, height }` (width
200–400, height 40–560) draws its own panel, as a pop-up of its own that
hangs from its icon in the bar. The bar never draws it, and the app never
gets a window of the bar's.

1. The user presses the icon. The bar asks frescod for a grant,
   `OP_POPUP_GRANT { parent: bar window, anchor: the icon's column, the
   bar's full height, side below, align end, width, height }`. frescod
   checks it as it checks `OP_POPUP_CREATE` (the bar's own window, Chrome,
   just pressed) and answers a random 16-byte token.
2. The bar sends `Intent::StatusOpenPanel { app_id, token }` to forum-wm,
   which passes `FromStatus::OpenPanel { token, width, height }` to that
   app's status connection (only an item set as `Drawn` takes one).
3. The app claims the pop-up, `OP_POPUP_CLAIM { token, width, height }`
   (Pergola: `Window::open(&WindowDesc::hosted(token, w, h))`), and draws
   in it. frescod checks the grant again (one use, at most
   `POPUP_GRANT_TTL_MS` = 10 s old, the bar still pressed or its chain
   open) and cuts the size to the grant's.
4. frescod tells the bar `EV_POPUP_HOSTED { token, open }` when it opens
   and when it goes; the bar lights the icon while it is open.

The panel is the app's, not the shell's:

- It grabs (keys go to it; a press outside, or focus moving, dismisses
  it, and the press that dismisses it is swallowed, so pressing the icon
  again closes it).
- It is never placed again. Once dismissed the app destroys it, and the
  next press makes a new grant. Escape is the app's to handle (Pergola's
  example closes on it).
- It stacks above every app window and app pop-up but below the bar and
  the bar's own menus, and it is kept below frescod's strip.
- Pop-ups it opens (a submenu) are the app's too.
- In split mode frescod's trusted layer draws a tab under it naming the
  app from the launch registry (`uid-N` when unregistered), in the
  strip's colours and font. The app draws the panel but cannot draw who
  it is. Direct mode (development only) has no tab.

In split mode the bar's and the app's renderers each record their half
(`PopupGrant`, `PopupClaim` records) and frescod's broker checks both;
`PopupHosted` goes back to the bar's renderer.

Example: `pergola/examples/status_drawn.rs` (recent clips, a bar chart of
copies by hour, Clear). Live check: `scripts/vm-status-drawn-check.sh`.
