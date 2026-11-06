# Tabula — clipboard and drag-and-drop, and the gesture interface with frescod

Status: interface draft, 2026-11-06. `tabulad` built (crate `tabula/`, §3) and tested against a
stand-in frescod; frescod's side (activation tokens) not yet. The token and consume types are
`fresco_protocol::activation` (ops 0x0900 `CONSUME`, 0x0901 `GRANT`), on frescod's socket
`/var/run/atrium/fresco-activation.sock`.

Placement decided with the user 2026-11-06 (pergola.md §1). Ownership agreed between threads: the
Pergola thread builds `tabulad`, `praecod`, the Pergola clients and widgets, Forum's toasts and
history, and the share sheet; the Fresco thread builds frescod's half: activation tokens (§2),
cross-window drag routing (§4), and the notification layer (§5).

## 0. One substrate, three services (asked by the user, 2026-11-06)

Clipboard, app-to-app transfer (drag, share) and notifications stay **three services**, because
their trust differs: Tabula holds content the user copied, the share broker routes it to an app
the user picked, and Praeco only displays. Merging them would put a display path next to
clipboard contents. What they share is unified instead, so each piece exists once:

| shared piece | one implementation | used by |
|---|---|---|
| Envelope, identity, capability | Aqueduct envelope; caller = socket peer uid (Portcullis's per-app uid); reachability = manifest `clipboard` / `notify` / `share` | all three |
| **Proof of a user gesture** | frescod's activation tokens (§2), consumable by an allowlist of brokers | Tabula (copy, paste, drop), the share broker (share), Praeco (a click on a notification's action), later Scrinium and Arca |
| **A typed item** | one `Offer`: up to 16 `(mime, blob)` formats, blobs in the CAS with proven possession (aqueduct.md §6.6) | clipboard entry, drag payload, shared item, notification image |
| **Delivery to an app**, launching it if needed | `CLASS_BROKER` (4) on the app's broker connection; Portcullis launches a stopped app | a share arriving, a notification action, a Tabellarius push |

So the share broker is Tabula's offer store plus a chooser drawn by Forum (trusted UI): a share
is a drop whose target the user picked in the chooser, and it spends the same tokens.

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

frescod mints a token for **every pointer-button press and every key press it delivers to a
window**: an app's own Edit menu, a Paste button or a right-click "Paste" are clicks frescod
cannot interpret, so a press is the gesture (like the web's transient user activation). Each
token records what the press was, and §2.2 limits what a key press may be spent on, so ordinary
typing never authorises a paste. Input that did not come from a local or Stoa-remote device (a
test client, an accessibility tool replaying input) mints no token. The token travels with the
event:

```rust
// fresco-protocol: added to InputKeyEvent (pressed = true) and InputPointerButtonEvent (pressed = true).
pub activation: [u8; 16],   // random, from the kernel CSPRNG; all-zero on releases
```

frescod keeps, per live token:

| field | meaning |
|---|---|
| `token` | the 16 random bytes (the table key) |
| `window_id`, `uid` | the window the event went to and its client's uid |
| `kind` | `BUTTON`, `KEY` (with the HID usage and modifiers), `DROP` (§4), or `GRANTED` (minted by trusted UI for an app, §2.4) |
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
for a different uid than `presenter_uid`, its window is no longer the focused one (a background
window cannot spend a token), or its kind does not fit the purpose:

| purpose | accepted kinds |
|---|---|
| `PASTE` | `BUTTON`; `KEY` only for the paste chords (Ctrl/Cmd-V, Shift-Insert); `GRANTED` for paste |
| `COPY` | `BUTTON`; `KEY` only with Ctrl/Cmd/Alt or Enter (Ctrl-C/X, menu accelerators) |
| `DROP` | `DROP` with a matching `drag_id` |
| `DRAG`, `SHARE`, `ACTION` | `BUTTON` (and `GRANTED` for `ACTION`) |

On success frescod removes it. The allowlist of callers is fixed in frescod's config: `_tabula`,
the share broker, `_praeco`; nothing else may consume. The channel is a frescod socket only
those uids can reach (Portcullis mounts it into their jails), checked again by peer uid.

### 2.4 Tokens granted by trusted UI

A click inside trusted UI on behalf of an app is a gesture for that app: the user clicked a
notification's "Reply", or picked an app in the share chooser. Forum, which drew the button,
asks frescod `ACTIVATION_GRANT { for_uid, purpose }` for the press it just received; frescod
checks that the press it is spending was in a Forum window and mints a `GRANTED` token bound to
`for_uid`. Only Forum's uid may grant.

### 2.3 What this buys

- A paste needs a press in the presenting app's own window within the last second.
- A token sent to another app fails the uid check.
- A replayed token fails: one use.
- An app holding focus but idle cannot read anything: no press, no token.
- Typing does not authorise a paste or a copy: only the chords in §2.2 do.
- Not covered, stated: an app can spend a click on an unrelated button (its "Next") on a paste.
  The click was real, in its focused window, within a second, so this is the web's activation
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

### 3.1 Security of the store

- **Tabula never parses content.** Blobs are stored and handed out as bytes; there is no format
  conversion in `tabulad` (an HTML-to-text conversion would parse attacker HTML in a trusted
  service). An app that wants another format asks for it and converts in its own jail; pasted
  HTML in `DocumentView` goes through the Navigator's normalizer in its jail, as any untrusted
  document does (pergola.md §10.2). The receiving app treats every paste as hostile input.
- **Proven possession.** `SET` uploads the bytes; it may not name a CAS hash it has not sent, or
  an app could probe whether content exists on the machine (aqueduct.md §6.6).
- **Limits.** Per format 16 MiB (one aqueduct upload), per entry 64 MiB and 16 formats, mime names ≤ 127 bytes of `[a-z0-9.+/-]`; per app
  one live entry and one pending drag; `tabulad` runs in its own jail as `_tabula` with no
  network.
- **Sensitive entries.** `SET` may mark an entry `sensitive` (Arca does, for a copied password):
  it is cleared after 60 s, never kept in history, and `OFFERS` shows it as `text/plain` only.
- **Privacy mode.** A jail marked private (aqueduct.md §6.5) gets its own clipboard; its copies
  reach another app only through a paste the user makes there with that jail's entry offered
  explicitly. Deferred past v1.
- **Audit.** `tabulad` logs who copied and who pasted (uids and times, never contents).

### 3.2 As built (2026-11-06)

`tabula/` (lib + `tabulad`): `SET`, `OFFERS`, `GET`, `CLEAR` and `EV_CHANGED`; replies are
`Reply::{Ok, Offers, Got, Empty, Refused{reason, detail}}` (postcard). A `SET` is validated
(formats, names, sizes, possession) before its token is spent, so a bad request doesn't waste a
gesture. Events go out on each client's own thread, between replies, so they never interleave
with one. Fails closed: if frescod's activation socket does not answer within 500 ms, the
request is refused. Tests (`tabula/tests/wire.rs`, over real sockets, two apps by uid): copy in
one app and paste in another; a made-up, stolen (other uid), reused, expired or wrong-kind token
is refused; no background overwrite or clear; naming a hash that wasn't uploaded is refused; a
secret must be text; change events; the audit log holds uids and outcomes only. Not yet: drag
(`DRAG_BEGIN`, `DROP_GET`), privacy mode, persistence across restarts (none planned for v1).

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
- **Against spoofing**: the app's name and icon come from its signed manifest, not from the
  post; title and body are plain text (no markup, no links), length-capped (64 / 512 bytes);
  images go through the Offer path and are drawn as images, never parsed as documents.
- **Against flooding**: a per-app rate limit (5 a minute, bursts of 3) and a queue cap; the user
  can mute an app from the toast.
- **Privacy**: on the lock screen (Vestibulum) only the app name shows, unless the user allows
  more per app.
- **Actions**: clicking an action is a `GRANTED` token (§2.4) delivered to the app over
  `CLASS_BROKER`; Portcullis launches the app if it is not running. A notification cannot run
  anything by itself.
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
