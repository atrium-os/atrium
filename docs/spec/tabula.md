# Tabula — clipboard and drag-and-drop, and the gesture interface with frescod

Status: interface draft, 2026-11-06. Not built.

Placement decided with the user 2026-11-06 (pergola.md §1). Ownership agreed between threads: the
Pergola thread builds `tabulad`, `praecod`, the Pergola clients and widgets, Forum's toasts and
history, and the share sheet; the Fresco thread builds frescod's half: activation tokens (§2),
cross-window drag routing (§4), and the notification layer (§5).

## 1. The rule

An app touches the clipboard only on a real user gesture in one of its own windows. Copy, cut,
paste and drop each spend a one-shot **activation token** that frescod minted for that gesture.
Tabula never trusts what an app says about its own input: it checks every token with frescod.
So an app can neither read the clipboard in the background nor overwrite it unprompted
(clipboard hijacking), and a token leaked to another app is useless (§2.3).

Identity is the client's **uid**: Portcullis runs each app under a dedicated uid
(`portcullis-peer`), and both frescod and Tabula read it from the socket (`uid_gid`).

## 2. Activation tokens (frescod mints, Tabula consumes)

### 2.1 Minting

frescod mints a token for **every key press and pointer-button press it delivers to a window**,
not only for Ctrl-V: an app's own Edit menu, a Paste button or a right-click "Paste" are clicks
frescod cannot interpret, so every press is a possible gesture (like the web's transient user
activation). The token travels with the event:

```rust
// fresco-protocol: added to InputKeyEvent (pressed = true) and InputPointerButtonEvent (pressed = true).
pub activation: [u8; 16],   // random, from the kernel CSPRNG; all-zero on releases
```

frescod keeps, per live token:

| field | meaning |
|---|---|
| `token` | the 16 random bytes (the table key) |
| `window_id`, `uid` | the window the event went to and its client's uid |
| `kind` | `PRESS` (minted on input) or `DROP` (§4) |
| `minted` | monotonic ns |
| `counter` | per-frescod sequence number, for logs |

A token lives **1 s** or until consumed. The table is bounded by input rate (at most a few
hundred entries); frescod drops expired entries on each mint.

### 2.2 Consuming (the private channel)

Tabula, and only Tabula, may call frescod's `ACTIVATION_CONSUME` (on frescod's socket, gated to
Tabula's uid `_tabula` by peer credentials):

```rust
ActivationConsume { token: [u8; 16], presenter_uid: u32, purpose: u8 /* COPY | PASTE | DROP | DRAG */ }
→ ActivationResult { ok: bool, window_id: u32, kind: u8, minted_ns: u64, drag_id: u64 /* DROP only */ }
```

It succeeds once. It fails when the token is unknown, already consumed, older than 1 s, minted
for a different uid than `presenter_uid`, or of the wrong kind for the purpose (`DROP` tokens only
for `DROP`; `PRESS` tokens for the rest). On success frescod removes it.

### 2.3 What this buys

- A paste needs a press in the presenting app's own window within the last second.
- A token sent to another app fails the uid check.
- A replayed token fails: one use.
- An app holding focus but idle cannot read anything: no press, no token.
- Not covered, stated: an app can spend a token from an unrelated press (a click on its "Next"
  button) on a paste. The press was real and in its window, so this is the web's activation
  model, not a per-command proof. Narrowing it needs frescod to know command semantics (a
  system Edit menu); deferred.

## 3. Tabula's wire (`CLASS_CLIPBOARD` = 2, `/var/run/atrium/tabulad.sock`)

Reachable by apps whose manifest says `clipboard = true` (portcullis.md). Payloads are CAS blobs
(aqueduct.md §6.4): a large image copied once is one allocation, also in Tessera.

| op | request | response |
|---|---|---|
| `0x01 SET` | `token`, `formats: [(mime, blob)]` (≤ 16 formats) | `ok` |
| `0x02 OFFERS` | — | `formats: [mime]`, `generation: u64` |
| `0x03 GET` | `token`, `accept: [mime]` (preference order) | `mime, blob` or `none` |
| `0x04 CLEAR` | `token` | `ok` |
| `0x10 DRAG_BEGIN` | `token` (the press that started the drag), `formats` | `drag_id: u64` |
| `0x11 DROP_GET` | `drop token`, `accept` | `mime, blob` or `none` |
| `0x80 EV_CHANGED` | (pushed) | `generation` |

- `SET` and `CLEAR` consume a `COPY` purpose; `GET` a `PASTE`.
- `OFFERS` needs no token: it returns format names only, so an app can enable or disable its
  Paste item. Known leak: an app learns that *something* of a type was copied. Accepted, as on
  every desktop OS.
- One entry, no history in v1. A copy from a jail marked private (aqueduct.md §6.5 "privacy
  mode") is kept out of other apps' reach until the user pastes it there.
- Tabula's own policy (per-jail scoping, audit) lives in `tabulad`, not in the wire.

## 4. Drag and drop

1. The source app gets a press, starts a drag and calls Tabula `DRAG_BEGIN(token, formats)`,
   receiving `drag_id` (Tabula holds the payload; nothing leaves yet).
2. The source asks frescod to begin the drag with the same `drag_id` (frescod's op; Fresco
   thread). frescod owns the grab, the drag image, and hit testing.
3. frescod sends the window under the pointer `EV_DRAG_ENTER/OVER/LEAVE { window_id, drag_id,
   x, y, formats }` (formats from the source, so targets can show where they accept).
4. On release over a window, frescod mints a `DROP` token bound to that window's uid and the
   `drag_id`, and sends `EV_DROP { window_id, x, y, activation }`.
5. The target calls Tabula `DROP_GET(drop token, accept)`; Tabula consumes it (purpose `DROP`),
   checks the returned `drag_id` matches, and releases the payload once.

A drop nobody consumes within 1 s, or a cancelled drag, frees the payload.

## 5. Notifications (Praeco), for the same split

- `praecod` (`CLASS_NOTIFY` = 3) takes `POST` from apps with `notify = true`; the wire is
  `praeco-macos`'s v0 (urgency, title, body) plus an app icon blob and actions later.
  Tabellarius delivers remote pushes into it.
- Forum draws toasts and history as a Pergola app in the trusted shell. The posting app never
  draws its own notification.
- frescod keeps a **notification layer** above every app window, drawable only by the
  Forum/Praeco role and clamped to a screen band, as for the origin strip (Fresco thread).

## 6. Pergola's part

- `TextField` and `DocumentView`: Ctrl-C/X/V and their menu items spend the activation token of
  the event that triggered them (Pergola's `Event::Key` and pointer events carry it).
- A drag source and drop target API over §4.
- A share button and share receiving (the share sheet is a powerbox, insula.md §5.2).

## 7. Order of work

1. frescod: `activation` on key and button presses, the token table, `ACTIVATION_CONSUME`
   (Fresco thread). Pergola: carry the token on its events.
2. `tabulad` with `SET`/`OFFERS`/`GET`/`CLEAR`, tested against a fake frescod; then copy and
   paste in `TextField`.
3. Drag and drop (both halves).
4. `praecod` and Forum toasts; the notification layer.
