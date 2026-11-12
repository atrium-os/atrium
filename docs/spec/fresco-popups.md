# Fresco pop-up windows

Status: steps 1 and 2 built (2026-11-12), VM-checked direct and split
(`scripts/vm-popup-check.sh`, 19/19 each); step 3 with the Pergola thread. Asked for by Shankar so that drop-downs and
menus can reach past their window (the Forum bar's controller menu first).

## 1. What a pop-up is

A pop-up is a window of the app's own, opened next to a spot in another of
its windows (the *parent*), that frescod places and stacks, not the WM.
Menus, drop-down lists, combo boxes, tooltips and submenus use it. Until
now Pergola drew these in the window's overlay layer, so a menu near the
window's edge was clipped (pergola `widgets/popup.rs`).

- **frescod places it.** The client says where it hangs from (an *anchor*
  rectangle in the parent's coordinates), which side it opens to, how it
  lines up along that side (`align`: start, centre, end), and the size it
  wants. frescod puts it there, flips it to the other side if it
  would leave the monitor, slides it along to stay on it, and shrinks it
  to the monitor if it is larger. The client hears where it landed.
- **It is not the WM's.** `OP_WM_ENUMERATE` leaves pop-ups out, as it
  leaves out slots. A pop-up moves with its parent.
- **It stacks above everything but notifications**, its parent's chain in
  order (a submenu above its menu).
- **Focus stays on the parent's top window**, so that window still looks
  active. While a *grabbing* pop-up is open, keys go to the topmost
  grabbing pop-up of the focused window's chain, so a menu gets the arrow
  keys and Escape.
- **It is dismissed by frescod** when the user presses outside the chain
  (anywhere not one of the chain's pop-ups: the parent, another window,
  the background), when focus moves to another window, or when the parent
  hides or goes. Dismissal hides the whole chain and tells the client;
  the press that dismissed it is not delivered (as on macOS and GNOME,
  so a click outside a menu does not also act). The client destroys the
  windows or opens them again.
- A pop-up that does not grab (a tooltip) takes no keys and is not
  dismissed by presses; it goes when its parent loses focus or goes.

## 2. Limits (the threat model's overlay rules)

A pop-up could be used to draw over another app. So:

- The parent must be one of the client's own windows, on screen, and the
  focused window (or a pop-up of the focused window's chain), or the window
  the user last pressed (the bar, which never takes focus), when the pop-up
  opens: no pop-ups from a background app.
- The anchor must lie inside the parent; the pop-up opens touching it, so
  it always sits against the app's own window.
- At most 8 pop-ups deep per chain, and a pop-up is never larger than the
  monitor the anchor is on.
- A pop-up cannot be a notification, a slot, or an embed, and cannot have
  slots.
- **The bar stays trusted** (asked by Shankar 2026-11-12: the bar's
  drop-downs reach into app area). An app's pop-up is kept below the
  trusted strip (`reserved_top`). The shell's chrome windows (the Forum
  bar, role `Chrome`) and their pop-ups stack above every app's pop-up:
  app windows, app pop-ups, chrome, chrome pop-ups, notifications. While
  a grabbing pop-up is open, only its chain may open more, so an app that
  stays focused under the bar's menu cannot cover it. No app gets the `Chrome`
  role (Shankar, 2026-11-12). It goes to the Forum bar and dock started as
  declared session components: portcullisd records those in the launch
  registry under their session.d `app_id`, marked `session` (`<uid>
  <user> org.atrium.forum-bar session`), which an app installed under the same id never is, and an app
  launch never reuses a session uid. `FRESCOD_CHROME_UIDS` adds uids for
  tests. Everyone else gets `Document`.
- An app can still draw a fake drop-down in its own window under the bar.
  The tell is the bar: while its menu is open the button is drawn pressed
  and the menu attached to it (atrium-visual-language.md §14.2, Pergola
  `MenuButton::attached`), which no app can draw. Bar menus never ask for
  secrets; those go through Arca's credential slots.

## 3. Wire

- `OP_POPUP_CREATE` 0x0507 (`PopupCreatePayload { parent, anchor:
  [f32; 4], side, align, width, height, grab }`) → the new window id, as
  `OP_WINDOW_CREATE` answers. `side`: `popup_side::BELOW`, `ABOVE`,
  `RIGHT`, `LEFT` (submenus open right). Refused (an error reply) when a
  §2 rule fails.
- `OP_POPUP_PLACE` 0x0508 (`PopupPlacePayload { window_id, anchor, side,
  align, width, height }`): move or resize an open pop-up, or show a dismissed
  one again (same rules).
- `EV_POPUP_PLACED` 0x058b (`PopupPlacedEvent { window_id, x, y, width,
  height, side }`): where it landed, in its parent's coordinates, and the
  side it opened to after any flip. Sent on open, on every re-place, and
  when the parent moves.
- `EV_POPUP_DISMISSED` 0x058c (`PopupDismissedEvent { window_id, reason
  }`), once per pop-up of the chain, topmost first. `reason`:
  `popup_dismiss::PRESS_OUTSIDE`, `FOCUS`, `PARENT`.

Everything else is an ordinary window: scene ops, present, destroy,
input.

In split mode the renderer's compositor only records pop-ups
(`popups_upstream`). It declares a pop-up with `WindowCreate` role
`Popup` (the broker makes it hidden) and then `PopupPlace` (parent,
anchor, side, align, size, grab); it sends `PopupPlace` again when the
client re-places it or shows it after a dismissal. The broker, which holds
the real window table, checks, places and dismisses, and answers with
`PopupPlaced` and `PopupDismissed` records (a refused `PopupPlace` is a
`PopupDismissed`, reason `PARENT`).

## 4. Pergola (the Pergola thread's part)

Built 2026-11-12 (`pergola::popup`). A view asks with `Ctx::popup(rect,
spec, on_dismiss, draw)` from an overlay: a free top-level stack laid out
where the overlay would draw, with an anchor, side, align and grab. The
tree stays one tree, so hit-testing, focus and keys work as for an
overlay. `Window` keeps a window per pop-up root (`popup::Popups`): it
opens, re-places (a new anchor or size, or showing it again after a
dismissal) and destroys them to match the tree, sends the root's subtree
to the pop-up (the root at its origin) and the rest to the main window,
moves a pop-up's pointer input back into tree coordinates, and on
`EV_POPUP_DISMISSED` runs the root's `on_dismiss`. `FrescoSurface::select`
switches the scene target and keeps slots per window (the glyph atlas is
re-sent to a window that missed rows). A `Menu` with an anchor
(`Menu::anchored`; a `Dropdown`'s list) opens this way; without a `Window`
(tests) it draws as an overlay. The anchor includes the 4 px gap, trimmed
to the window when the control sits at its edge.

Not yet: a list frescod shrank does not scroll (`EV_POPUP_PLACED` is
not used yet), and tooltips and submenus as pop-ups.

`Menu` and `Dropdown` open as pop-ups when the app's window allows
(`Window` holds the connection); the overlay path stays for tests and for
content that fits. `EV_POPUP_DISMISSED` closes the menu as Escape does.
The Forum bar's controller menu becomes a drop-down from its button.

## 5. Build order

1. Protocol, compositor (`Window::popup`, placement, stacking, keyboard,
   dismissal), frontend ops, frescod's press path, split records, unit
   tests.
2. fresco-client `popup_create` / `popup_place` and events; a probe for
   the VM check (open, flip at the screen edge, dismiss by an outside
   press, keys to the pop-up).
3. Pergola Menu/Dropdown (Pergola thread), then the Forum bar.
