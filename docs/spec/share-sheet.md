# The share sheet: handing one item to an app the person picks

Status: built 2026-11-18; VM gate `scripts/vm-share-check.sh` 16/16 on headless frescod (amd64 cloud VM, Haswell guest), 2026-11-18. Crate `share-sheet/`
(the service and two demo apps), role messages `limen::share` and `limen::share_target`,
limend's `CHOOSE`. Assigned to the Pergola thread on 2026-11-06 (pergola.md §1), built by
the share-sheet thread.

Design it follows: [tabula.md](tabula.md) §0 (one item type, one gesture authority),
[insula.md](insula.md) §5.2 (the share sheet is a powerbox), [limen.md](limen.md) §2.4
(`share-target`), [app-composition.md](app-composition.md) §5.1 (`[roles] implements`).

## 1. What it is

An app has something (a note, a picture, a PDF) and the person wants it in another app. The
app presses Share; a sheet drawn by the system over the app's window lists the apps that take
that kind of thing; the person presses one; that app opens in the same place, gets the item,
and says whether it took it.

- **The app can't send to an app the person didn't choose.** `share-target` has no default
  provider. limend starts one only from the person's choice in a share sheet the same app
  opened, once, within 30 s.
- **The app can't open a sheet by itself.** The sheet spends the press that asked for it with
  frescod (purpose `SHARE`, which only `_share` may spend; tabula.md §2.2), so the press must
  be a fresh click in the app's own focused window.
- **The app learns nothing about what is installed.** It never sees the list, nor which app was
  chosen; only `Chosen`, `Cancelled` or `Failed`, and later the target's `Accepted` or
  `Declined`.
- **The sheet never sees the content.** It gets a title and the media types, nothing else.
- **The target treats the item as hostile input**, as any paste.

## 2. Parts

| part | runs as | does |
|---|---|---|
| the app (parent) | its own uid | embeds `share`, then `share-target`; sends the item |
| `share-sheet` (`org.atrium.share`) | `_share` (65045) | Limen's `share` provider: spends the press, lists targets, draws the sheet, tells limend the choice |
| limend | `_limen` | binds both slots; accepts `CHOOSE` only from the share sheet; starts the chosen target |
| the target | a per-instance uid of the app's person and persona (dev-tools-plan.md M6) | receives the item in a sheet slot, answers |
| frescod | `_frescod` | places both sheets over the app (slot kind `Sheet`), so the app can't draw over or read them; checks the press |

Why not Forum, as tabula.md §0 first sketched: the picker (scrinium.md §4.4) already showed that
a system sheet in a Limen `Sheet` slot is trusted UI frescod places and frames, with the app's
input held off it. The share sheet reuses that path instead of adding a second trusted drawer.
The chooser and the target are two embeds one after the other because frescod allows no slot
inside a slot (a sheet can't host the target) and one sheet per window.

## 3. The flow

```
app                         limend                  share-sheet (_share)      frescod      target
 │ click on Share (token T)    │                          │                       │            │
 │ REQUEST share ─────────────►│ Attach ─────────────────►│                       │            │
 │ Send Open{T,title,mimes} ──►│ ────────────────────────►│ CONSUME T (SHARE) ───►│            │
 │                             │                          │◄── ok (app's, fresh) ─┤            │
 │                             │◄── READY (sheet window) ─┤ list targets          │            │
 │◄── Attached ────────────────┤ bind Sheet slot ─────────┼──────────────────────►│            │
 │                             │          person presses "Notes" in the sheet     │            │
 │                             │◄── CHOOSE{embed, notes} ─┤                       │            │
 │◄── Message Chosen ──────────┤◄── Emit Chosen ──────────┤                       │            │
 │ DETACH sheet; REQUEST share-target ►│ the choice (once, ≤ 30 s) → ExecInstance(notes) ─────►│
 │ Send Share{item}; SEND_FD Part{i} ─►│ ──────────────────────────────────────────────────────►│
 │◄── Attached (Sheet slot) ───┤◄──────────────────────────────── READY ─────────────────────┤
 │◄── Message Accepted ────────┤◄──────────────────────────────── Emit Accepted ─────────────┤
 │ DETACH                      │                                                           │
```

limend's side (`limen::daemon`):

- `share` is a **system role** (`org.atrium.share`, slot `Sheet { 480, 400 }`); limend accepts
  `PROVIDE share` only from `LIMEN_SHARE_UID` (default 65045, `_share`). An app needs `share` in
  its manifest's `embeds` (platform bundles without `embeds` keep every role).
- `CHOOSE { embed, app }` (op `0x16`): only from the connection serving that `share` embed, only
  for an app whose installed manifest implements `share-target` and, for a dev build, only the
  parent's person's own (`Installed::may_target`); otherwise `NOT_A_TARGET` (16) or `NOT_YOURS`.
  The choice is stored for the embed's **parent connection**.
- `share-target` is `Provider::Chosen`: a request takes the parent's stored choice (removing it)
  if it is at most `CHOICE_MS` (30 s) old, else `NO_PROVIDER`. No `embeds` check for it: the
  choice can only come from a sheet the parent was allowed to open. The child is started with
  `launch_for(parent_uid)`, so it runs in the parent's person and persona; a share never crosses
  personas in v1 (personas.md §6 would need a crossing switch and a sheet saying so).
- `share-target` takes descriptors (`SEND_FD`): each is checked to be a read-only regular file or
  pipe and narrowed to read, seek and fstat.

## 4. Messages

### 4.1 `share` (app ↔ share sheet), `limen::share`

| direction | message | payload |
|---|---|---|
| app → sheet | `Open` | `activation: [u8; 16]` (the press), `title` (≤ 200 bytes, no control or bidi characters), `mimes` (1–16 media types, best first) |
| sheet → app | `Chosen` | none: embed `share-target` now |
| sheet → app | `Cancelled` | none |
| sheet → app | `Failed` | `why` (≤ 200 bytes, for the log): no fresh press, odd request |

The app detaches the sheet after any answer.

### 4.2 `share-target` (app ↔ target), `limen::share_target`

| direction | message | payload |
|---|---|---|
| app → target | `Share` | `item: { title, formats: [{ mime, body }] }`, ≤ 16 formats, one per type; `body` is `Inline(bytes)` (≤ 48 KiB all together) or `Fd { size }` (≤ 2 GiB) |
| app → target | `Part` | `index`: the bytes of format `index`, a descriptor in the same frame (`SEND_FD`), one per `Fd` format, after `Share` |
| target → app | `Accepted` | none |
| target → app | `Declined` | none |

`limen::share_target::Incoming` gathers a `Share` and its parts on the target's side and refuses
parts that weren't announced or come twice; `Received::bytes(i, max)` reads a format (never more
than its declared size); `Received::best(accept)` picks the first format the target takes.

### 4.3 What a target declares

```toml
[roles]
implements = ["share-target"]

[roles.share-target]
mime = ["text/*", "image/*"]      # 1–32: a type, a family, or "*/*"
```

portcullis-toml validates it (`is_mime_pattern`); without the section an app that implements
the role takes anything. The sheet lists an installed app when it takes at least one of the
item's types (`limen::share_target::accepts`), for the parent's person (her dev builds, nobody
else's), never the sharing app itself.

Providers still start with no capabilities and no `graphics` (M6): they draw through the
embed-only frescod connection limend hands them, and do their work there. A target that must
do more (Mail sending a message) needs the courier and broker paths its own app has; v1 targets
keep what they get (save it, show it, put it in a draft the person sends from the app).

## 5. For app developers

Sharing, with Pergola (`pergola::share`; `share-sheet/src/bin/share_demo.rs` uses it):

- `share::button(pressed)` is a Share button that keeps the press it was clicked with in a
  `Mutable<Option<Token>>` (or call `share::press()` in any click handler).
- `Sharing::begin(&embeds, window_token, press, item, files, (sheet_slot, target_slot))`
  brings the sheet up; `files` are the descriptors of the item's `Fd` formats, by index.
  It refuses an item out of shape or a missing descriptor before anything is asked.
- Draw `sharing.slot()` (the sheet's slot, then the target's; the system places both, so the
  rect doesn't matter) and feed it the window's Limen events with `sharing.event(&embeds,
  &ev)`, which answers `Some(Outcome::Shared | NotShared | Failed(why))` at the end, with both
  embeds detached.

Without Pergola the steps are the same by hand: in the click handler take the press;
`request("share")`, `send(Open { activation, title, mimes })`; on `Chosen` detach it,
`request("share-target")`, `send(Share(item))` and `send_fd(Part { index }, fd)` for each `Fd`
format; on `Accepted`, `Declined` or `ChildLost`, detach.

Receiving (`share-sheet/src/bin/share_target_demo.rs`, the shape of the SDK's `service`
template): `Child::embedded()`, wait for `Attach { role: "share-target" }`, open a window with
`embed_pending` on the frescod connection limend gave, `ready` with its token, feed messages to
`Incoming` until `take()` gives the item, then emit `Accepted` or `Declined`.

## 6. Tests and what is not done

Built and tested on Linux:

- `limen/tests/daemon.rs` `a_share_target_is_only_ever_the_app_the_person_chose_in_the_parents_share_sheet`:
  no choice no target; only `_share` provides; `CHOOSE` refused from anyone but the sheet's
  connection, for another embed, for a non-target or a path; the choice is the parent's only and
  spent once; the item and a read-only descriptor reach the target, a writable one is refused;
  both slots are `Sheet`s.
- `share-sheet/tests/sheet.rs` (the service against an in-process limend and frescod's rules):
  no press, another app's press, a key press and a spent press all give `Failed`; targets by
  type, by person (dev builds), never the app itself; only a shown app may be chosen, once;
  the chosen app is what portcullisd is asked to start; a cancel chooses nothing.
- `limen` unit tests for both message sets and `Incoming`; portcullis-toml for the manifest.

Not done yet:

- **The VM gate with real jails**: `scripts/vm-share-check.sh` (16/16) runs frescod's real
  activation table (only `_share` 65045 may spend `SHARE`), injected clicks (Share, Scraps,
  Keep, Cancel) and `pergola::share`, but limend is `limend-dev` (targets start unconfined as
  `sharetgt`) and nothing runs in a jail or under Forum yet. It also checks: no press gives
  `Failed`; Photos and Scraps are listed but not Maps nor the app itself; a press beside the
  sheet does nothing; the picture arrives by descriptor; Cancel starts nothing.
- Escape and keyboard choice in the sheet; app icons in the list (names only now).
- Sharing across personas (refused by construction: targets run in the parent's persona).
- Curia: the person's ordering or hiding of targets.
- Notification actions over `CLASS_BROKER` (tabula.md §5) are a separate path, still not built.
