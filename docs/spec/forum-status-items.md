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

## 2. Menu model

Up to 32 entries, each one of:

- `Action { label, enabled }`: picked → `Picked { index }`.
- `Check { label, on, enabled }`: picked → `Picked { index }` (the app
  sends the new state back in its next `Set`).
- `Radio { label, on, enabled }`: as `Check`, drawn as a choice.
- `Slider { label, value }` (0 to 1): moved → `Slid { index, value }`,
  while dragging and with Left/Right; the menu stays open.
- `Separator`.

Labels are at most 64 characters (cut), the tooltip 64. These map onto
Pergola's `MenuItem` (`checked`, `radio`, `slider`).

## 3. Wire

forum-wm serves `/atrium/sockets/forum-status/forum-status.sock` (postcard,
`forum_ctl` framing). Portcullis mounts it only into jails holding
`status-item`; forum-wm also resolves the peer through the launch registry
and refuses an unregistered one (development: `FORUM_STATUS_ANY=1`).

- App → WM: `Set { glyph: char, tooltip: String, menu: Vec<Entry> }`,
  `Remove`.
- WM → App: `Picked { index }`, `Slid { index, value }`.

The bar reads the items with forum-ctl `ListStatusItems` (app id, app
name, glyph, tooltip, menu, generation) and reports the user's actions
with `StatusPick { app_id, index }` / `StatusSlide { app_id, index,
value }`; forum-wm relays them to the app's connection.

## 4. Later

App-drawn panels (Shankar's second step): the bar asks frescod to open a
pop-up for the app hanging from its icon; the app draws in it; frescod
labels it with the app's name, the icon's button lights in the app's
colour, and it stacks below the bar's own menus.
