# Tabula — clipboard and drag-and-drop, and the gesture interface with frescod

Status: interface draft, 2026-11-06. `tabulad` built (crate `tabula/`, §3) and tested against a
stand-in frescod; frescod's side of the activation tokens built (§2.5). The token and consume types are
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

As built (2026-11-18, [share-sheet.md](share-sheet.md)): the chooser is the share sheet, a
Limen system role drawn by `share-sheet` as `_share` in a frescod `Sheet` slot (the picker's
trusted path), which spends the app's press as `SHARE`; the item goes straight from the app to
the chosen app over Limen (`share-target`), not through a store, so nothing is kept.

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

### 2.5 As built in frescod (2026-11-06)

`frescod/src/activation.rs`. The HID readers mark each press; the event fan-out mints its token
with the uid of the window's app (the connection's peer uid, or, for a window a renderer opened
in split mode, the uid of the app behind it). Only the window's owner gets the token: the WM sees
the same press with an all-zero `activation`. The injector (`FRESCOD_INPUT_SOCK`) mints nothing,
unless `FRESCOD_INJECT_ACTIVATION=1`, which is for tests and logs a warning. The table holds at
most 4096 live tokens (the oldest goes first) and drops expired ones on each mint.

The socket is `FRESCOD_ACTIVATION_SOCK` (default `/var/run/atrium/fresco-activation.sock`).
frescod checks each connection's peer uid. Each consumer may spend only its own purposes. The
defaults are root (`OPEN`, `CONSOLE`, `APPROVE`, `LINK`: portcullisd; `LINK` is a link opened in
another app, opening-links.md §5, and a press in a guarded window needs the guard seen as for
`PROMPT`), `_tabula` (`COPY`, `PASTE`, `DRAG`, `DROP`),
`_share` (`SHARE`), `_praeco` (`ACTION`), `_arca-ui` (`FILL`) and `_sigillum-ui` (`IDENTITY`),
for the users that exist. `PROMPT` is self-spent: any uid may spend a press on its own window
(`presenter_uid` equal to its own) for it, and no consumer is named for it; the person's own
navigatord answers the Navigator's prompt row (navigator-ui.md §6.4), and frescod's checks
(the guard seen whole and still, focus, the window's owner) are the same. `FRESCOD_ACTIVATION_UIDS` replaces them:
comma-separated `uid=purpose+purpose` entries, purposes by name or number (`0=open+console`,
`1001=prompt`); a bare uid may spend any purpose and is logged as a warning (tests only). Forum is
`FRESCOD_FORUM_UID`, or else `_forum`'s uid. Other uids are not served. A consumer may only
`CONSUME`, and only for its purposes, and Forum may only `GRANT`; anything else gets
`Refused { NOT_ALLOWED }`, and a refused consume leaves the token unspent. Replies carry `IS_RESPONSE` and the request's op.

- "Focused" means the window keys go to: the focused one, else the topmost. `GRANTED` tokens
  skip the focus check.
- A refused consume does not spend the token, so a wrong presenter cannot burn the real app's
  token. tabula's stand-in `Fake` behaves the same way.
- An expired token answers `EXPIRED`, then `UNKNOWN` once it has been dropped.
- `GRANT` spends a `BUTTON` press in one of Forum's own windows. It mints a `GRANTED` token for
  `for_uid` that can be spent on that one purpose (`PASTE`, `SHARE` or `ACTION`) within 1 s.

Tests are the table's unit tests in frescod and `atrium-activation-probe` (atrium-test-client).
The probe runs end to end over the real sockets, in direct and split mode, and checks:

- the app's press carries a token, and the WM's copy carries none;
- a token is refused for another uid, spent once, and unknown after;
- typing 'a' does not paste; Ctrl-C copies;
- a click expires after 1 s;
- Forum grants, and the granted token works only for its purpose.

Not yet built:

- `DROP` tokens (§4);
- the system users `_tabula`, `_share`, `_praeco` and `_forum`;
- Portcullis mounting the socket's directory into those jails.

### 2.6 A press in an island, spent by its parent (2026-11-25)

A live page in Navigator is an island: a slot in the Navigator window that the engine's own
window fills (navigator-islands.md §7, I6.4). Keys and clicks on it go to the engine, so the
token is minted for the engine's uid, and the engine has no Tabula access. The Navigator UI,
which owns the window the island is a slot of, writes and reads the clipboard for it.

- When frescod mints a token for a press in an island slot, it also records the parent window
  and that window's owner's uid (`island_parent` in `socket_server.rs`, `Entry.parent_window`
  and `parent_uid` in `activation.rs`).
- `CONSUME` with `presenter_uid` equal to that parent uid is taken for `COPY` and `PASTE`
  only. Every other purpose still needs the island's own uid. The chord rule (§2.2) is
  unchanged: Ctrl-C for a copy, Ctrl-V for a paste.
- The focus check passes when the island or its parent window is focused.
- Tabula is unchanged: it still passes its caller's uid as `presenter_uid`.
- The engine forwards each Ctrl-C (with the selected text, at most 16 KiB) and each Ctrl-V
  with its token to navigatord, which hands them to the UI. The UI spends the token through
  Pergola's `copy_text_with` and `paste_text_with`, and only for the live page on screen.

Test: `an_islands_parent_spends_a_press_in_the_island_only_on_a_copy_or_a_paste` in
`activation.rs`.

## 3. Tabula's wire (`CLASS_CLIPBOARD` = 2, `/atrium/sockets/clipboard/clipboard.sock`)

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
secret must be text; change events; the audit log holds uids and outcomes only. Drag (§4) is
built on Tabula's side: `DRAG_BEGIN` (a `BUTTON` press, purpose `DRAG`) holds the payload and
returns `Reply::Dragging { drag_id }`; `DROP_GET` spends the `DROP` token and takes the payload
of the drag frescod bound it to, once; `DRAG_CANCEL { drag_id }` (op `0x12`, source only) frees
it; one pending drag per source, 60 s at most. Tested: a drop releases once; a plain press or a
drop token minted for another app takes nothing; only the source cancels; a drag needs a button
press. Not yet: privacy mode, persistence across restarts (none planned for v1).

### 3.3 Personas, as built (2026-11-18)

Each entry and each pending drag is in a persona (personas.md §6): the one the launch registry
gives the copying uid (a person's own login uid is their Personal; other unregistered uids are
system services, outside every persona). `GET` and `DROP_GET` take an entry only within its
persona, or across with the person's switch for `clipboard` crossings in their `policy.toml`
(`Policy::may_cross`, read afresh each time); otherwise `Refused { reason: OTHER_PERSONA (10) }`
and tabulad logs which persona the entry was copied in, never what. `Set.for_uid` copies for
another app, in that app's persona: only from the uids in `TABULA_FOR_OTHERS` (default
`_monstrator`, for Monstrator's Copy address), refused for anyone else. tabulad listens on
`/atrium/sockets/clipboard/clipboard.sock` (the directory the `clipboard` capability mounts;
Pergola's and the Terminal's clients default to it) and runs as `_tabula` (65039), the uid
frescod lets spend copy and paste presses. Tested in `tabula/tests/wire.rs` (Work to Personal
and back, a drag, the switch one way only, Copy address and a stranger's `for_uid`) and in the
VM by `scripts/navigator-confined-vm.sh`. A paste or drop refused this way asks frescod for the
strip note "paste refused: copied in <persona>" over the pressed window (frescod's `NOTE`, only
for the press tabulad just spent; personas.md §6, built 2026-11-18). Not yet: tabulad in a jail of its own (the person's `policy.toml` must stay readable
to it).

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

### 4.1 As built in frescod (2026-11-06)

The wire is in `fresco_protocol::control`:

- `OP_DRAG_BEGIN` (0x0710, `DragBegin { window_id, drag_id, activation, formats }`) has no
  reply. A refusal comes back as `EV_DRAG_END { dropped: false }`.
- `EV_DRAG_ENTER`, `EV_DRAG_OVER` and `EV_DRAG_LEAVE` (0x0711 to 0x0713) carry
  `DragEvent { window_id, drag_id, x, y, formats }`. `formats` is filled on `ENTER` only.
- `EV_DROP` (0x0714) carries `DropEvent { window_id, x, y, activation }`.
- `EV_DRAG_END` (0x0715) goes to the source.
- fresco-client has `drag_begin` and the matching `Event`s.

A begin is refused unless all of these hold:

- `activation` is the button press still held (frescod remembers the last tokened press until
  its release, and does not spend it);
- the press went to `window_id`, a window of the caller's that has focus;
- no other drag is on;
- `drag_id` is non-zero;
- the formats are at most 8 printable-ASCII MIME types of at most 64 bytes.

While the button is held, frescod hit-tests the pointer and sends `ENTER`, `OVER` and `LEAVE`
to the window under it. Only that window's owner gets these.

On release over a window, frescod mints a `DROP` token for that window's app uid, bound to the
`drag_id`, and sends `EV_DROP`. The source gets `EV_DRAG_END { dropped }`. Consuming a `DROP`
token returns its `drag_id` and needs no focus, since the drop target is often a background
window. A destroyed source window ends the drag.

In split mode the same events travel as fresco-split records:

- renderer to frescod: `DRAG_FORMAT`, then `DRAG_BEGIN`;
- frescod to renderer: `DRAG_OFFER`, `DRAG`, `DROP` and `DRAG_END`.

Tested by a frescod unit test, and by `atrium-activation-probe` in the VM in both modes. The
probe drags from one app's window to another's: a begin without the held press is refused, the
target gets `ENTER` with the formats, then `OVER` and `EV_DROP`, and the source gets
`dropped: true`. The drop token is refused as a paste and spends as that drag's `DROP`.

Not yet built: a drag image (the cursor only), and Escape to cancel.

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

### 5.1 As built (2026-11-06)

Crate `praeco/` (lib + `praecod`), the production service beside the `praeco-macos` adapter, same
v0 `POST` wire (libatrium's `atrium_notify` unchanged; tested byte for byte). The app is named by
the Portcullis launch registry from the peer uid (an unlaunched process gets `NOT_AN_APP`);
title ≤ 64 and body ≤ 512 bytes, cut on a character boundary; control, bidi-override and
zero-width characters dropped (so "invoice‮txt.exe" cannot read as a different name); markup is
kept as literal text; 3 a burst, 5 a minute per app; `WITHDRAW` only one's own; `SUBSCRIBE` only
for the uids in `PRAECO_SUBSCRIBERS` (Forum's), which get the history (500) then a stream.
Each notification carries the registry's user, and Forum shows only its own user's. System
services that are not apps post as a name praecod is configured with, never one they send:
`PRAECO_SYSTEM_POSTERS` lists `uid:name` pairs, and their posts carry the user `*`
(`proto::SYSTEM_USER`), which every user's shelf shows. Forum posts this way (forum-wm, as
`Forum`) to name a monitor a span holds out for flapping (fresco-span.md §6);
`praeco::client::post` is the poster's side. Not yet:
images, per-app mute. Actions: §5.5.

`forum-shelf/` draws the toasts (Pergola, `Notification` role, §5.2): it subscribes to praecod, shows its own
user's only, three at a time from the top right, low 3 s, normal 5 s, high until dismissed and
first in the queue; the first line of a toast is the registry's app id, so a title claiming to
be the system reads as a title under the app's real name. A click dismisses. It asks for a
band-wide window (420 px) and draws at whatever size frescod gives it, following resizes. Its uid
must be listed in both `PRAECO_SUBSCRIBERS` and `FRESCOD_NOTIFY_UIDS`. Not run on frescod yet.

### 5.2 The notification layer, as built in frescod (2026-11-06)

`WmRole::Notification` is appended to fresco-protocol's roles. In split mode it is `Role::Notification`
(5). frescod grants it only to the uids in `FRESCOD_NOTIFY_UIDS`, and the default is none.
Forum's shelf runs under its app's own uid, so the session lists that uid there, as it does for
`PRAECO_SUBSCRIBERS`. Any other client asking for the role gets `Document`. Every frame, before
anything is drawn or sent to renderers, frescod applies two rules:

- **Order:** notification windows sit above every other window, but stay below frescod's own
  strip. The same holds after a WM layout, a raise or a new window. Hit testing follows the same
  order.
- **Placement:** each one stays inside `fresco_protocol::notification_band(width, height,
  reserved_top)`. That is the top right of the screen, 420 px wide, below the strip, and half of
  the rest of the screen high. A larger window is shrunk, and its client hears the new size. A
  window placed outside is moved in.

Keys never fall through to a notification. With nothing focused, they go to the topmost other
window (`Compositor::keyboard_window`). forum-wm places the role in the band, shows it on every
workspace and never gives it focus.

Tests:

- a compositor unit test covers order, clamping and the keyboard rule;
- `atrium-activation-probe`, run in the VM in both modes, opens a 2000x2000 toast at (0, 0). It
  lands at the band, (860, 24, 420, 348) on 1280x720 in split mode, and keys still reach the app.

forum-shelf asks for `Notification` and a band-sized window (Pergola thread, 2026-11-06).

### 5.3 No storms: freshness, catch-up and bursts (Shankar 2026-11-16, system-wide)

Proposed in [mail.md](mail.md) §12.1 and made a rule for every app by Shankar
(2026-11-16: "Yes, system wide"). Not built.

- **Event time.** A post carries `at`, when the thing it reports happened (mail received,
  message sent, reminder due), and optionally `kind = reminder { starts, ends }`. A v0 post
  without `at` counts as now.
- **Freshness.** A post older than its app's freshness window (default 10 minutes; an app
  may ask for less, not more) when praecod gets it is **folded**: never shown as its own
  toast, never sounds. Folded posts make one **catch-up summary per app** ("Mail: while you
  were away, 23 new, 4 from people"), silent, updated in place rather than stacked, built
  from the posts' titles by praecod (an app may supply the summary line through a
  `summary` field, still plain text and capped).
- **Bursts.** More than 2 posts from one app within 30 s become one grouped toast that
  updates in place ("5 new"), listing the latest titles. Today's limit (3 a burst, 5 a
  minute) refuses the rest; with grouping they are folded into the group instead of lost.
- **Sounds**: at most one a minute per app, whatever arrives.
- **Reminders that are late**: a `reminder` whose `starts` is more than 5 minutes past, or
  whose `ends` has passed, is history only, never a toast. Several late reminders are one
  silent summary.
- **Withdraw on read**: an app may withdraw its own posts (exists); Mail does so when a
  message is read elsewhere.
- **Quiet hours** (per persona, mail.md §12) and Do Not Disturb hold everything for the
  next summary.

### 5.4 Readable toasts, private when they must be (Shankar 2026-11-16)

Shankar: "the email popup arrives as a notification but it is not enough to read even the
title fully so have to click it to go to the email app to find out. Of course security
matters so don't show if locked or logged out." Not built.

- **Longer, still plain text.** Title up to 256 bytes (was 64), body up to 2 KiB (was
  512), an optional `subtitle` line (Mail: the sender and verified domain) of 128 bytes.
  Same rules as before: plain text, controls, bidi overrides and zero-width characters
  dropped, no markup, no links, nothing parsed.
- **Toasts wrap, they don't cut.** The title wraps to up to 3 lines and the body shows up
  to 4 lines; only past that is it ended with "…". The app's name from its manifest stays
  the first line (§5.1), so long text can't push it off.
- **Expand in place.** A chevron on the toast (or Space while the shelf is the target of
  a press, since keys never reach notifications by themselves) grows it to the whole body,
  up to the band's height, scrollable, and stops its timeout. Expanded, it still shows only
  the plain text the app posted: reading a whole short mail there needs no app switch, and
  pressing the body opens it in the app as before. Actions (Archive, Reply) sit under it
  (§5.5).
- **History** in the shelf shows the same full text.
- **When it shows less:**
  - **Locked** (the lock screen, lock-screen.md): app name and a count only ("Mail · 3
    new"), no title, sender or body, unless the person allows titles for that app; bodies
    never on the lock screen.
  - **Logged out, at the login screen, or another person's session on the seat**: nothing
    of this person's; Forum shows only its own user's notifications (§5.1), and the login
    screen has no shelf.
  - **Screen being shared or recorded** (once frescod has capture): titles and bodies
    hidden, app name and count only, so a preview never appears in a meeting.
  - **Per app**: the person can choose *full*, *title only* or *name only* per app (and per
    persona); apps may also mark a post `private` (Mail does for encrypted mail: "Encrypted
    message from Priya"), which shows name only.
- **What Mail puts in a post**: subject as title, sender + verified mark as subtitle, the
  first 2 KiB of the new text (quoted text and signature removed, from the parse jail's
  plain-text summary, never HTML) as body.

### 5.5 Actions, as built (2026-11-18)

A notification can carry up to three buttons ("Archive", "Reply"). The press on one reaches
the posting app only through praecod and only with frescod's word that a person pressed it in
Forum (§2.4). Built in `praeco/`, `forum-shelf/` and frescod's `GRANT`.

- **Posting.** `POST_ACTIONS` (op `0x02`) takes a postcard `Post { urgency, title, body,
  actions }`. Each `Action { id, label }` needs an id of 1 to 32 characters from
  `[a-z0-9._-]`, unique on that post. Its label is cleaned like a title, at most 24 bytes, and
  can't be empty. A post with more than 3 actions, or with any bad one, gets `MALFORMED`. A
  system poster (`SYSTEM_USER`) can't attach actions, since there is no app to start. The
  `Notification` Forum receives also carries the actions, the posting app's `uid` and its
  `persona` (from the registry). SDK: `praeco::client::post_with_actions(sock, urgency, title,
  body, &[("archive", "Archive"), ("reply", "Reply")])`.
- **The press.** The shelf draws the labels as buttons under the toast. A click takes the press
  frescod minted for the shelf's window and asks frescod to `GRANT` it to the posting app's uid
  for `ACTION`. Only `FRESCOD_FORUM_UID` may do this, and frescod checks the press was in that
  uid's window and is fresh. The shelf then sends praecod `ACT { id, action, token }` (op
  `0x03`, subscribers only, on a connection of its own) and dismisses the toast.
- **praecod** finds the notification and checks that the action is one of its own, then spends
  the token with frescod as `ACTION` for the posting app's uid (praecod is the `ACTION`
  consumer, `_praeco`). A token frescod never minted, or one granted to another app, gets
  `NO_PRESS` (7). A non-subscriber gets `NOT_ALLOWED`. A spent press can't be used twice.
- **Delivery.** An app opens `praeco::client::Listener::connect(&praeco::client::socket())`
  when it starts (op `LISTEN`, `0x11`) and reads `Acted { id, action }` (pushed as op `0x81`)
  with `next(timeout)`. Only listeners of the posting app's own uid hear it. When none is
  listening, praecod holds the action for 30 s (`HOLD_MS`) and answers `NOT_RUNNING` (6). The
  shelf then asks portcullisd to start the app (`app-launch`, as the dock does) in the
  notification's persona, and the app gets the held action as soon as it listens. A listener
  whose app went stops counting within 0.1 s.
- **Its window forward** (2026-11-18). A Reply that opens a compose window must be able to
  bring it to the front. When praecod spends an action's press, frescod gives praecod one
  `RAISE` of that app's top window for two minutes, as it does for portcullisd's `LINK`.
  After hearing the action, the app calls `praeco::client::raise(&socket(), id)` (op `RAISE`,
  `0x12`, `RaiseMe { id }`). praecod accepts this only from the posting app's uid, once per
  press, within 110 s (`RAISE_MS`), and then asks frescod to raise it. Asked before a press,
  or a second time, it answers `NOT_ALLOWED`. frescod sends the WM a `WindowActivate`, and
  forum-wm focuses the window.
- **Where apps find praecod**: `praeco::client::socket()` returns `PRAECO_SOCKET` if set.
  Otherwise, in a jail with `notify = true`, it returns `/atrium/sockets/notify/notify.sock`,
  else `/var/run/atrium/praecod.sock`. praecod's `PRAECO_FRESCOD_ACTIVATION` is frescod's
  activation socket; the shelf's is `SHELF_FRESCOD_ACTIVATION`.
- **Tests.** `praeco/tests/wire.rs` checks the limits and that only Forum may act. It also
  checks that only a press granted to that app counts, that only its own listener hears the
  action, that a press counts once, that an action is held and handed over when the app
  starts, that a gone listener doesn't count, and that a raise works once, for the posting app
  only, after a press. `forum-shelf` checks that a toast's actions are its buttons.
  `scripts/vm-notify-actions-check.sh` passes 19/19 in the amd64 cloud VM on headless frescod
  with forum-wm and injected clicks. The shelf itself runs in its jail as its app uid, with
  `fresco`, `fresco-activation`, `notify` and `portcullis` mounted. Three signed test apps (`praeco-demo`, in
  `forum-shelf/src/bin`) run in their jails. Chat opens a window and posts with Archive and
  Reply, and a raise asked before any press is refused. Cover's window then covers Chat's. A
  click on Reply reaches Chat through frescod's grant and praecod's spend, and Chat's raise
  makes forum-wm bring its window forward. Reminder posts and exits; a click on Archive makes
  the shelf start it through portcullisd, and it hears "archive" from the hold. A forged
  action from root gets status 5 (only the subscriber may act), and one from the shelf's uid
  without a real press gets status 7. frescod's `GRANT` now uses the same token lifetime as `CONSUME`.
- **The shelf in its jail (2026-11-18).** Its manifest asks for `notification-shelf = true`, a
  system-only permission that mounts frescod's activation socket (`fresco-activation/`) so the
  shelf can hand a press to the app that posted. Because the shelf runs as its app uid,
  praecod tells a subscriber whose person it is (`Event::Person { user }`, from the persona
  registry) and sends it only that person's notifications.

## 6. Pergola's part

- `TextField` and `DocumentView`: Ctrl-C/X/V and their menu items spend the activation token of
  the event that triggered them (Pergola's `Event::Key` and pointer events carry it).
- A drag source and drop target API over §4.
- A share button and share receiving (the share sheet is a powerbox, insula.md §5.2).

### 6.1 As built (2026-11-06)

`pergola::clipboard`: `copy_text` / `paste_text` spend the activation of the input event being
dispatched (`App::handle_event_activated` scopes it; outside a dispatch there is none and nothing
is sent), over a `Backend` that is Tabula's socket in production (`TABULA_SOCKET`, else
`/atrium/sockets/clipboard.sock`). `TextField`: Ctrl/Cmd chords are commands and are never typed
(they used to append the letter). Ctrl-C/X copy or cut the selection. Ctrl-V replaces the
selection or inserts at the caret. A `secret` field never copies or cuts.
Rich fields additionally offer `application/vnd.atrium.pergola.inline-text` alongside `text/plain`,
up to 32 KiB of source text. Version 1 is a version byte, UTF-8 text, and one emphasis byte per
UTF-8 byte. The decoder checks that emphasis is consistent over each character. Ctrl-Shift-V
requests plain text. A rich paste writes text and emphasis in one editor undo step.
`copy_formats` publishes alternative MIME representations in one SET. `paste_format` submits an
ordered list of accepted types in one GET and Tabula returns the first available type. Fresco
supplies and checks the gesture token. Tabula negotiates the MIME type and serves its bytes.
Tested in `text_field.rs` and against the real server code in `pergola/tests/clipboard_tabula.rs`.
`Window` passes the `activation` of fresco-client's key and button presses (04c295d) and of
`Event::Drop`. Drops: fresco-client's `Drop` becomes `pergola::Event::Drop { at }`; the App
hit-tests it to the nearest `Ctx::on_drop` node, whose handler calls `clipboard::drop_text`
(`DROP_GET` with the drop token). `TextField` inserts drops as plain text. Not built: starting a drag
from Pergola (`DRAG_BEGIN` + `Connection::drag_begin`), and a drop highlight on
`DragEnter`/`DragLeave`.

## 7. Order of work

1. frescod: `activation` on key and button presses, the token table, `ACTIVATION_CONSUME`
   (Fresco thread). Pergola: carry the token on its events.
2. `tabulad` with `SET`/`OFFERS`/`GET`/`CLEAR`, tested against a fake frescod; then copy and
   paste in `TextField`.
3. Drag and drop (both halves).
4. `praecod` and Forum toasts; the notification layer.
