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
- **The icon** is a Phosphor glyph the app names. It is drawn in the
  status-item group only, so a glyph that looks like a system icon still
  sits among apps. (A bitmap icon from the app's bundle is a later step.)
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

  What the user does comes back as `Changed { id, value }`; the app sends
  the new state in its next `Set` (a control moves at once in the bar, so
  a drag is not undone while the app answers, and takes the app's value
  when it comes). forum-wm passes on only a value that control takes,
  and only if it is enabled.

  Limits (forum-wm cuts to them): width 200 to 400 px; 64 nodes in all;
  4 levels of nesting; text 512 characters, labels 64; 8 options a choice;
  560 px tall (nodes past it are not drawn). Text fields come later (a
  grabbing pop-up already takes keys).

## 3. Wire

forum-wm serves `/atrium/sockets/forum-status/forum-status.sock` (postcard,
`forum_ctl` framing). Portcullis mounts it only into jails holding
`status-item`; forum-wm also resolves the peer through the launch registry
and refuses an unregistered one (development: `FORUM_STATUS_ANY=1`).

- App → WM: `Set { glyph: char, tooltip: String, content: Menu(..) |
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

## 4. Later

App-drawn panels (Shankar's second step): the bar asks frescod to open a
pop-up for the app hanging from its icon; the app draws in it; frescod
labels it with the app's name, the icon's button lights in the app's
colour, and it stacks below the bar's own menus.
