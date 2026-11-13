# Limen: the cross-jail embed broker

Status: v1 design, accepted 2026-11-06 (the user agreed to all §13 recommendations;
rewritten that day from the May 2026 sketch). Nothing is
built except the `credential` role's message types (`limen/`, e710f18) and Pergola's slot
plumbing (`pergola::slot`, `CredentialField`).

**Limen** (Latin: *threshold, doorway*) lets one app show part of another app inside its
window, without either one trusting the other. An email client shows an HTML mail body
rendered by the jailed document viewer. A sign-in form shows Arca's password field. An editor
hosts an extension. Each embedded app runs in its own jail, draws in its own window, and talks
to its host only through a typed **role** contract.

It is Atrium's answer to `<iframe>` and to platform intents (Android Intents, iOS App
Extensions), but built from pieces that already exist: frescod pins windows, portcullisd
launches jails, and Aqueduct carries the messages. Limen introduces the two sides and enforces
the contract between them.

Related specs:

- [insula.md](insula.md) §10.3: the summary and the side-channel posture.
- [arca-autofill.md](arca-autofill.md): the `credential` role and frescod's credential slots.
- [vestibulum.md](vestibulum.md) §4.3, §7: the `sign-in` role.
- [pergola.md](pergola.md) §10: the `doc-viewer` role's document contract.
- [artifex.md](artifex.md) §7: the `editor-extension` role.
- [tabula.md](tabula.md): drag and drop and the clipboard across the boundary.

## 0. Position

### 0.1 What Limen is

- The service that turns "show a role-X embed here" into a running jailed child whose window
  is pinned into a slot of the parent's window.
- The authority on roles: which messages a parent and child may exchange, and which apps may
  provide each role.
- The relay for those messages, checking size and rate and knowing who is on each end.
- The owner of an embed's lifecycle: attach, resize, hide, detach, crash.

### 0.2 What Limen is not

- **Not a window manager.** frescod composes and Forum manages top-level windows. Limen only
  asks frescod to pin a child window to a rect in its parent.
- **Not a UI library.** Limen has no widgets. Pergola has the parent's slot widget and the
  child's window.
- **Not a general app bus.** Aqueduct is the general IPC, and there is deliberately no
  app-to-app channel except a role's (pergola.md §1). Limen carries role messages and nothing
  else.
- **Not a pixel channel.** The parent never receives the child's pixels, and the child never
  receives the parent's.

## 1. Architecture

```
parent app (jail A)                         child app (jail B)
  Pergola window W                            Pergola window C
     │ request_embed(role, W, rect)              ▲ attach: role, channel
     ▼                                           │
   limend ── launch child (embed mode) ──► portcullisd
     │       or ask a system provider for a new instance
     │
     ├── bind C into W at rect ──────────► frescod (slot op, §5)
     │
     └── relay role messages A ⇄ B, lifecycle events to A
```

`limend` is a small trusted daemon, running as its own user `_limen` (fixed uid 65022, so frescod and choragusd can name it). It holds no secrets and
draws nothing. It knows four things: the role catalogue, who each connection is, which embeds
exist, and which slot each one is in.

**Identity is the connection, never a message** (as in [arca.md](arca.md) §4.1):

- A **parent** connects to limend's socket. limend reads the peer uid and resolves it to
  (user, app id) through the launch registry (`portcullis_peer::AppRegistry`).
- **Windows are named by frescod's embed tokens.** App window ids are local to each
  renderer in split mode, so frescod gives every window 16 random bytes and sends them only
  to its owner (`EV_WINDOW_EMBED_TOKEN`). Holding a token proves the window. frescod's
  answer to a bind names both windows' owner uids, and limend keeps the slot only if the
  parent window is the parent's and the child window is the child's. Otherwise it releases
  it and the parent gets `ChildLost`.
- A **child** is started by portcullisd at limend's request and inherits its end of the
  channel as a descriptor, so limend knows which embed it is without asking. A system
  provider that is already running (Arca, Vestibulum) instead connects to limend itself. It
  is recognised by its uid, which the catalogue names (§2.0). Arca's provider is `arca-ui`,
  a jailed system service running as `_arca-ui` (65021). It is separate from `arcad`, which
  parses nothing from outside. It is not a registry app, so limend recognises it by that
  fixed uid (`LIMEN_ARCA_UID`).

## 2. Roles

A role is:

1. a name (`doc-viewer`, or reverse-DNS for a vendor role);
2. a Rust module of message types, postcard-encoded, in the `limen` crate (for example
   `limen::credential`). Each side decodes with the role's types and refuses anything out of
   shape. limend relays the bytes without decoding them;
3. a provider rule: a fixed system service, or the user's default among apps whose manifest
   declares the role;
4. a slot kind (§5), and limits (message size and rate, lifetime).

Messages are versioned by adding enum variants at the end; a peer that gets a variant it
doesn't know drops the message. An incompatible change is a new role name (`doc-viewer.2`).

### 2.0 System roles and user roles

- **System roles** have exactly one provider, a system service named in limend's catalogue.
  The user never picks it, and no app can register for it. `credential` is Arca and
  `sign-in` is Vestibulum.
- **User roles** are provided by installed apps that declare them (§3), and the user picks a
  default.

| role | kind | provider | slot kind | lifetime |
|---|---|---|---|---|
| `credential` | system | Arca | `credential` | as long as the form |
| `sign-in` | system | Vestibulum | `credential` | one sign-in |
| `doc-viewer` | user | default: the Navigator's document worker | `plain` | as long as the parent wants |
| `picker` | user | default: Scrinium | `plain` | one pick |
| `share-target` | user | the app the user chose in the share sheet | `plain` | one share |
| `media-player` | user | default media app | `plain` | as long as the parent wants |
| `payment` | user | the user's payment app | `credential` | one payment |
| `map` | user | default map app | `plain` | as long as the parent wants |
| `editor-extension` | user | the extension the user installed | `plain` | as long as the parent wants |

### 2.1 `doc-viewer`

Renders a document that the parent doesn't trust (an HTML mail body, a downloaded help page,
a file preview) in a jailed child. The child runs the Navigator's document worker (layout,
zoom, big-document bands, replay) and scrolls with Pergola's `ScrollView` inside its own
window (pergola.md §10).

| direction | message | payload |
|---|---|---|
| parent → child | `Load` | `doc: DocRef` (a Tabula-style blob the parent uploaded, or a URL the child fetches through its own jailed fetcher), `theme` |
| parent → child | `SetTheme` | `theme` |
| parent → child | `ScrollTo` | `anchor: String` |
| parent → child | `Open` | the Navigator's page: `ticket: [u8; 16]` redeemed on navigatord's page socket, `dark`, `scroll: f32` (px from the top, at most 10⁷; a parked page coming back where it was) |
| parent → child | `Park` | the parent will close the child (navigator-ui.md §4.4); answered by `Parked` |
| child → parent | `Loaded` | `title: String` (at most 256 bytes), `anchors: Vec<String>` (at most 256 of them) |
| child → parent | `LinkActivated` | `url: String`. Only after a press in the child; the parent decides whether to open it |
| child → parent | `Error` | `code: u16` |
| child → parent | `Parked` | `scroll: f32`, how far down the page is (finite, 0 to 10⁷). Only a number: the page's state that comes back is its address, kept by navigatord, and this offset |
| child → parent | `Embeds` | `list: Vec<{ key: u64, rect: Option<[x, y, w, h]>, role: String, src: EmbedSrc }>`, every embed the child has in view, `rect` in the child's window px (`None`: scrolled partly or wholly out). At most 64. Sent when the list or a rect changes, after the frame that shows it |

The child never sends the parent document text.

**Embeds in a document** (fresco-video P3, 2026-11-06). frescod refuses a slot whose child
hosts slots (§5.1), so a video in a doc-viewer page is not the doc-viewer's own slot. It is a
**sibling**: the parent requests the `media-player` embed for its own window and binds it
above the doc-viewer slot: `Request.z` (limen `Parent::request_at`, Pergola `Embeds::request_at`)
is carried to frescod's `SlotBind.z` (ecc6d38), the doc-viewer at 0 and its videos at 1. The child, which owns the scroll position, reports each embed's
rect in its window; the parent adds the doc-viewer slot's origin, releases any rect that isn't
wholly inside the doc-viewer slot (§5.2's whole-or-hidden rule, and a lying child can place a
video nowhere it couldn't already draw), and places the rest. Keys are the document worker's
embed keys. The parent caps live media embeds per page (8) and starts one only on the user's
press or the page's autoplay policy. Until L6 the Navigator UI owns the `ScrollView` and needs
none of this: it emits a slot node at the embed's document box inside the scroll content
(`Ctx::slot`), and `pergola::slot::window_rect` does the mapping and the clipping. A selection leaves only through the
clipboard, on the user's Ctrl-C in the child (tabula.md).

### 2.2 `media-player`

Plays audio and video in a jailed native player. The parent never handles decoded frames:
the player demuxes, decodes, presents to frescod in its own window, and draws its own
controls. Because decoders are the riskiest code in media playback, they live only here.

There are two ways to feed it.

- **A whole resource** (`Load`): a file descriptor. The player's jail has no network
  (Shankar, 2026-11-06): for a plain video file at a URL, the Navigator fetches it and passes a
  descriptor, or feeds it as segments, so its cookies, CORS and LAN filter apply in one place.
  No engine runs at all.
- **A segment feed**, shaped like Media Source Extensions (MSE). The parent fetches the
  segments and the player decodes them. This lets a tier-4 engine run a site's player script,
  which is what YouTube needs to produce its stream URLs and tokens (legacy-web §8.1),
  while the engine's `<video>` and `MediaSource` are only a front end to this role. The
  engine appends bytes it fetched and never sees a frame.

| direction | message | payload |
|---|---|---|
| parent → child | `Load` | `source` (`Fd`: a descriptor passed with the message, see below; there is no URL source), `mime`, `growing: bool` (the file is still being written: wait at its end for more) |
| parent → child | `LoadDone` | none: a growing `Load`'s file is complete |
| parent → child | `OpenSource` | none: begins a segment feed |
| parent → child | `AddTrack` | `track: u8` (at most 8), `mime` (an MSE type such as `video/webm; codecs="av01.0.08M.08"`) |
| parent → child | `Append` | `track`, `bytes` (at most 60 KiB a message, inside limend's 64 KiB message cap; a parent splits a segment into several appends), `timestamp_offset: Option<f64>` |
| parent → child | `Remove` | `track`, `start`, `end` (seconds) |
| parent → child | `Abort` | `track`: drop a half-parsed segment |
| parent → child | `EndOfStream` | `error: Option<u8>` |
| parent → child | `SetDuration` | `secs: f64` |
| parent → child | `CanPlay` | `mime`: asks whether the player decodes this type |
| parent → child | `Play`, `Pause`, `Stop` | none |
| parent → child | `Seek` | `secs: f64` |
| parent → child | `SetVolume` | `volume: f32` (0 to 1) |
| parent → child | `SetRate` | `rate: f32` (0.25 to 4) |
| parent → child | `TextTrack` | `id: u8` (at most 16), `lang`, `label` (64 bytes each), with a WebVTT file as a regular-file descriptor: the page's `<track>` |
| parent → child | `ShowText` | `text`: `None`, `InBand` (the default) or `Page(id)` |
| parent → child | `MediaMetadata` | `title`, `artist`, `album` (128 bytes each): `navigator.mediaSession.metadata` |
| parent → child | `RequestPip`, `ExitPip` | none: `requestPictureInPicture()` during a user gesture, and `exitPictureInPicture()`; limend's `Pip` event says it happened |
| parent → child | `MediaActions` | `actions: u16`: the `media_action` bits the page handles (`NEXT_TRACK`, `PREVIOUS_TRACK`) |
| child → parent | `Ready` | `duration_secs: f64`, `width`, `height` |
| child → parent | `CanPlay` | `mime`, `supported: bool`: the reply, for `MediaSource.isTypeSupported` |
| child → parent | `Playing`, `Paused` | none: the play state changed, from the parent's message or the native controls |
| child → parent | `Buffered` | `track`, `ranges: Vec<(f64, f64)>` (at most 64), after each append or remove |
| child → parent | `Appended` | `track`: the append is parsed, so the next may go (`updateend`) |
| child → parent | `Time` | `secs: f64`, at most 4 a second |
| child → parent | `Waiting` | `secs: f64`: playback reached the end of what is buffered |
| child → parent | `Seeking` | `secs: f64`: the user seeked in the native controls; the parent fetches from there |
| child → parent | `Ended` | none |
| child → parent | `Volume` | `volume` (0 to 1), `muted`: the user changed it in the player (`volumechange`) |
| child → parent | `Action` | `action`: one `media_action` bit the page declared, asked for by the user |
| child → parent | `Error` | `code: u16` (`DECODE`, `UNSUPPORTED`, `TOO_BIG`, `STATE`, `NETWORK`), `track: Option<u8>` (an append to this track was refused and the feed goes on; `None`: the player failed) |

The types are `limen::media_player` (atrium-player.md M0).

**A file by descriptor.** `Load { source: Fd }` goes with `Parent::send_fd`, limend's
`SEND_FD` op. The descriptor travels in the same frame as the message (`SCM_RIGHTS`), so the
two can't be separated. Only a role whose catalogue entry says `takes_fd` may receive one,
and today that is `media-player` alone. For any other role limend refuses `MALFORMED` and
closes the descriptor. A message carries at most one descriptor. limend `fstat`s it and
refuses anything but a regular file or a pipe opened read-only with `BAD_FD`. That rules out
sockets, directories, devices and anything writable. FreeBSD's pipes are bidirectional, so a
pipe end is accepted there either way and made read-only by narrowing. On FreeBSD limend
narrows every passed descriptor with `cap_rights_limit` to read, seek and fstat before
forwarding it. The rights travel with the descriptor, so the child can't widen them. limend
never reads it. A descriptor counts against the message rate like any message, and is
closed with the event if the child is gone. Descriptors sent with any other op are closed.
The child receives `ChildEvent::MessageFd` and takes the descriptor with
`Child::take_fd()`. A pipe plays once, front to back. The player can't seek in a pipe or
restart a crashed decoder on one.

**Limits and back-pressure.** One `Append` is in flight per track: the next waits for
`Appended`. The player keeps at most 150 MB buffered per feed, or its jail's memory limit if
that is lower. Over that it answers `Error { TOO_BIG }`, which the engine reports to the
page as MSE's `QuotaExceededError`. The page then removes what it has played, as it does in
any browser.

**What the parent learns.** Only `Buffered`, `Time` and the state events, which are what MSE
and the video element already expose to the page. It gets no frames, no frame timing beyond
`Time`, and no decoded audio. Those are the same facts a page can read in any browser, so
running the player out of process adds no new channel.

**Codecs** are royalty-free: dav1d for AV1, libvpx for VP9, and Opus and Vorbis for audio.
OpenH264 is used for H.264 (as Firefox does for WebRTC; it decodes only Baseline and B-frame-free content correctly, Hospes H0), and AAC is decoded with a free decoder where
it is legally available. Hardware decode, when the GPU stack has it, goes here and nowhere
else.

**DRM.** A content decryption module (CDM) runs below the player with no network: in our
foreign-library host (own ELF loader + glibc shim, capability mode, own jail; no Linux ABI
in the kernel), optionally inside a bhyve micro-VM (legacy-web §8.1, threat model §2.9,
docs/reviews/2026-11-07-linux-compat-scoping.md). The player passes it encrypted
samples and gets frames back. The engine relays licence messages without seeing a key.
Messages for this (`LicenceRequest`, `LicenceResponse`) are added when a CDM is.

**One player, many hosts.** The Navigator, a mail app showing an attached video, and a
podcast app all embed the same role. So picture-in-picture, background audio, the
media keys and the lock-screen controls are built once, in the player, and work the same for
all of them.

### 2.3 `picker`

One-shot powerbox ([insula.md](insula.md) §5.2): the user picks items in the child's UI, and
only those items reach the parent.

| direction | message | payload |
|---|---|---|
| parent → child | `Open` | `filter: { kind, mime, multiple }` |
| child → parent | `Picked` | `items: Vec<Item>`; a file comes as a descriptor that limend passes with the message |
| child → parent | `Cancelled` | none |

The embed detaches on `Picked` or `Cancelled`.

### 2.4 `share-target`

The opposite of `picker`. The parent has content, and the share sheet (pergola.md §1) has the
user choose a target app, which receives it.

| direction | message | payload |
|---|---|---|
| parent → child | `Share` | `offer: Offer`, the same type as Tabula's clipboard entries (tabula.md §0) |
| child → parent | `Accepted` | none |
| child → parent | `Declined` | none |

### 2.5 `payment`

| direction | message | payload |
|---|---|---|
| parent → child | `Start` | `amount, currency, merchant, reference, methods` |
| child → parent | `Completed` | `receipt: { reference, method, at, signature }` |
| child → parent | `Cancelled` | none |
| child → parent | `Error` | `code: u16` |
| child → parent | `Parked` | `scroll: f32`, how far down the page is (finite, 0 to 10⁷). Only a number: the page's state that comes back is its address, kept by navigatord, and this offset |

Card numbers and account details stay between the user and the child. The parent gets a
receipt. The slot is a `credential` kind, so the press that pays has the same visibility
check as a password fill.

### 2.6 `map`

| direction | message | payload |
|---|---|---|
| parent → child | `SetView` | `lat, lng, zoom` |
| parent → child | `AddMarker` / `RemoveMarker` | `id, lat, lng, label` / `id` |
| child → parent | `ViewChanged` | `lat, lng, zoom` |
| child → parent | `MarkerActivated` | `id` |

### 2.7 `editor-extension`

Defined in [artifex.md](artifex.md) §7.2. This spec owns the role's mechanics; artifex.md owns
what Artifex does with it.

### 2.8 `credential`

Defined in [arca-autofill.md](arca-autofill.md) §8 and built as `limen::credential`. It has
`Configure` from parent to child, and `Filled`, `Handle` and `Cleared` from child to parent.
No message carries the secret.

### 2.9 `sign-in`

Defined in [vestibulum.md](vestibulum.md) §7. It has `Begin { service, hints }` from parent
to child, and `Done { result }`, `Cancelled` or `Error` from child to parent.

## 3. Providers of user roles

### 3.1 In the app manifest

```toml
[roles]
implements = ["doc-viewer", "share-target"]

[roles.share-target]
mime = ["text/*", "image/*"]
```

portcullisd checks this at install time, as it does every manifest. limend reads the
installed apps' declarations from the app tree (`/var/lib/atrium/apps/*/atrium.toml`) at
start and when an install changes it. A manifest can't declare a system role.

### 3.2 Choosing a default

- With one provider, it is used.
- With more than one, and no choice made yet, limend uses the platform default from its
  catalogue and remembers that the user hasn't chosen. Asking the user is Curia's job
  (default-app settings), which doesn't exist yet.
- The user's choice is stored per user in limend's own store until Curia exists.

### 3.3 No provider

The parent gets `NoProvider` and shows its own fallback (for a doc-viewer, the document as
plain text). The install prompt waits for Curia and the package UI.

## 4. Launch and attach

```
parent                    limend                 portcullisd       frescod        child
  │ RequestEmbed{role,      │                        │                │             │
  │  window, rect, slot}    │                        │                │             │
  ├────────────────────────►│ is window owned by     │                │             │
  │                         │ parent's uid? ─────────┼───────────────►│             │
  │                         │ role → provider        │                │             │
  │                         │ ExecInstance(embed)───►│ jail, exec ────┼────────────►│
  │                         │◄── child connects on inherited channel ─┼─────────────┤
  │                         │                        │                │  child opens │
  │                         │◄── ChildReady{window} ─┼────────────────┼─────────────┤
  │                         │ bind slot(child win,   │                │             │
  │                         │  parent win, rect,kind)┼───────────────►│             │
  │◄── Attached{embed} ─────┤                        │                │             │
  │ Send{embed, bytes} ────►│──── relay ─────────────┼────────────────┼────────────►│
```

- **User roles**: limend asks portcullisd for an `ExecInstance` of the provider app, as
  frescod does for its renderers. That's one jail per embed, discarded at exit, with the
  provider's own manifest capabilities. The child's stdin is its limend channel, its stdout
  goes nowhere, and its stderr is limend's. Hanging up the request connection ends the jail.
  The parent's identity goes to the child as data (`Attach { parent_app, parent_uid }`: the uid limend read off the
  parent's connection, which frescod's bind confirmed owns the slot's window), never as
  authority.
- **The child runs as its own user, never as `_limen`.** `ExecInstance` runs a worker as its
  caller unless the system policy has a worker rule for it, so every provider needs one:
  `[workers."<app>"] callers = ["_limen"], run_as = "<its user>"`. limend refuses a launch
  without one before asking portcullisd, and expects that uid to own the child's window. No
  portcullisd change was needed (`limen::launch`).
- **The child's only frescod connection is embed-only** (fresco-video.md §8 S1). Before
  sending anything else on the child's channel, limend opens a new connection to frescod's
  activation socket and sends `EMBED_CONNECT { owner_uid }` (0x0911), naming the child's
  uid. frescod refuses a uid that is itself on the slot allowlist. After `Ok`, frescod treats
  that stream as the child's client connection: every window it opens is forced
  `embed_pending` and shows only when bound. limend passes the descriptor to the child with
  `SCM_RIGHTS` as the first thing on the channel, and `limen::Child::embedded()` returns it.
  limend also refuses to launch a provider whose manifest asks for `graphics`, because
  `ExecInstance` would then mount the ordinary app socket into its jail. A launch fails
  closed if frescod can't give the connection.
- **System roles**: limend asks the provider, which is already running, for a new instance
  over the provider's own connection. The provider opens a new window for it.
- The child's window is not shown on its own until it is bound. frescod keeps a window
  hidden while it is waiting to be bound (the `embed_pending` window hint), so a child can't
  flash up as a top-level window.
- `Attached` reaches the parent only after frescod has bound the slot.
- **Descriptors after attach.** Besides the frescod connection at launch, limend forwards a
  descriptor to a child only with a `SEND_FD` message for a role that takes one (§2.2): it
  is checked, narrowed and never read.

## 5. Slots

### 5.1 A slot is a child window pinned into its parent

frescod already does this for credential slots ([arca-autofill.md](arca-autofill.md) §4.1),
and v1 generalises the same op:

- frescod places the child window at the parent's position plus the slot rect, gives it the
  rect's size (the child hears the resize), and keeps it directly above its parent in z-order;
- if either window goes away, frescod releases the slot and destroys nothing else.

This gives the guarantees with no new compositor machinery:

- **No pixel readback.** The child's pixels are another window's, and no client can read
  another window.
- **No drawing over the slot.** The parent draws only in its own window, which is below the
  child. A parent menu or popover over an embed has to be its own window, above both. Pergola's
  popups are separate windows for this reason.
- **Input** follows from z-order: the slot is the topmost window under the pointer, so
  frescod routes there (§6).

The slot op takes a **kind**:

- `plain`: pinned, clipped to the parent, and nothing else.
- `credential`: pinned, plus frescod's visibility record, `FILL` presses, and the frame that
  frescod draws (arca-autofill.md §4).

Only limend may bind slots. The op has its own allowlist (`_limen`) instead of the whole
window-management socket. `OP_WM_CREDENTIAL_SLOT` stays as the test stand-in until the general
op lands.

**As built in frescod (L2, 2026-11-06).** In split mode the window id an app knows is its
renderer's, not frescod's, so windows are named by an **embed token** instead of an id:

- frescod mints a random 16-byte token for every window it creates and sends it to the window's
  owner alone, once, as `EV_WINDOW_EMBED_TOKEN` (0x0588, `EmbedTokenEvent { window_id, token }`,
  with the app's own window id; fresco-client `Event::EmbedToken`). In split mode it travels as
  the split record `EMBED_TOKEN` (0x124). The parent app and the child each hand theirs to
  limend. A token dies with its window.
- The bind op is on frescod's activation socket (`/var/run/atrium/fresco-activation.sock`),
  which frescod serves itself in both modes: `fresco_protocol::slot::op::BIND` (0x0910),
  `SlotBind { parent, child: [u8; 16], kind, rect: Option<[x, y, w, h]> }`, `rect` in the
  parent's surface pixels and `None` to release. The answer is
  `SlotBindResult::Ok { parent_uid, child_uid }`, so limend can check both owners against its
  records, or `Refused { reason }`: `NOT_ALLOWED` (1, the caller's uid isn't in
  `FRESCOD_SLOT_UIDS`, default `_limen`), `UNKNOWN_WINDOW` (2), or `INVALID` (3: the same window
  twice, a parent that is itself a slot, a child that hosts slots, a notification, a child
  already in another parent, a bad kind or rect).
- `slot::kind::PLAIN` (0) and `CREDENTIAL` (1). Only `CREDENTIAL` gets the visibility record,
  `FILL` and the frame.
- `WindowHints.embed_pending` keeps a window off screen (not composed, hit or focused, and not
  listed to the WM) until it is bound. A child of either kind goes off screen again when it is
  released, or when its parent goes, so it never shows as a top-level window. A slot is clipped
  to its parent's edges; one clipped away entirely, or whose parent is off screen, is off screen
  too. The WM neither lists nor places slots.
- `OP_WM_CREDENTIAL_SLOT` remains as an alias (WM socket, frescod's ids, kind `CREDENTIAL`)
  until limend's binder moves to the token op.

Tested by unit tests in fresco-scene-server and by `atrium-activation-probe` in the VM, in
direct and split mode: an `embed_pending` window takes no click; once bound it takes the clicks
in its slot; a made-up token and a slot as a parent are refused; after a release, or after its
parent is destroyed, the click goes past it again; and a gone window's token is dead.

### 5.2 Placing, moving, hiding

The parent's Pergola window reports each slot's window rect after layout
(`pergola::slot::SlotBinder`, e710f18). It sends only changes, after the frame that makes
room for the slot is presented. limend forwards these to frescod.

- **Clipped means hidden.** A slot that a scroll view clips, even partly, is released until
  it is whole again, because a pinned window can't be partly clipped by its parent's scroll
  view.
- **Hidden** (`SetVisible(false)`, or clipped): frescod hides the child window, and the child
  gets `Suspended` and should stop drawing.
- **Each move restarts** a `credential` slot's 500 ms visibility run, so a field that is moving
  can't be filled.

### 5.3 Detach

The parent sends `Detach`, or closes its window, or exits. limend tells the child `Detaching`,
gives it 500 ms, then asks portcullisd to end the jail (user roles) or tells the provider the
instance is gone (system roles). The parent gets `Detached`.

## 6. Input, focus, clipboard, drag and drop

- **Pointer**: the slot is the topmost window at that point, so frescod sends the event to the
  child, in the child's coordinates. The parent never sees it.
- **Keyboard**: focus is per window. A click in the slot focuses the child's window, so keys
  go to the child. Tab at the end of the child's focus order and Shift-Tab at the start send
  `FocusOut { forward }` to limend, which tells the parent to focus its next widget. frescod
  focuses the parent window. System shortcuts go to Forum, as always.
- **Clipboard and drag and drop** are Tabula's, with the child as one more window: a paste in
  the child needs a press in the child (tabula.md §2), and a drop onto the slot goes to the
  child (tabula.md §4).
- **Scroll**: the child scrolls its own content. Handing a scroll on to the parent at the
  edge is not in v1.

## 7. Lifecycle and errors

```
RequestEmbed ─► LAUNCHING ─► ATTACHED ◄─► SUSPENDED
                    │            │
                    ▼            ▼
                 FAILED      CHILD_LOST ─► DETACHED
```

The parent receives:

- `Attached { embed }`;
- `Suspended` and `Resumed`;
- `Message { bytes }`;
- `ChildLost { reason }` (crashed, over its limits, or exited);
- `Detached`;
- `Refused { reason }` (`NoProvider`, `NotYourWindow`, `UnknownRole`, `TooMany`).

On a crash, frescod releases the slot and the parent's own placeholder shows (the
`CredentialField` well, for example). The parent decides whether to ask again.

**Limits** (per parent): at most 16 live embeds, at most 64 KiB per message, and at most 256
messages a second each way per embed. Over the rate, messages are dropped and the parent is
told once. Jail limits on the child (memory, CPU) are portcullisd's (`ExecInstance`'s
`memory_mb`, `cpu_pct`), set per role in the catalogue.

## 8. Capability and trust

- **The child's capabilities come from its own manifest**, never the parent's. The parent
  can't grant the child anything, smuggle capabilities through, or pin an old version. Neither
  side has authority over the other.
- **Choosing a default provider is the trust decision**: "I trust this app to show documents
  when another app asks". It is one decision per role, not one per site.
- **Messages are data.** limend relays bytes and enforces size and rate. Each side's role
  types refuse anything out of shape, and a side never acts on a message as if the user had
  asked. Anything that needs the user's intent (opening a link, paying, filling) needs a press
  in the child's window, which frescod vouches for.
- **Per-embed consent** (`payment`) is shown by the provider in its own slot, where the
  parent can't draw over it.

## 9. Accessibility

The accessibility tree should span the boundary: the slot is an `embedded-content` node in
the parent's tree, and atrium-ax grafts the child's tree there. That waits for atrium-ax and
is not in v1. Until then a screen reader reads the child's window on its own when it is
focused.

## 10. Side channels

| channel | what stops it |
|---|---|
| pixel readback | each side is a separate window; no client reads another window's pixels |
| drawing over the slot to mislead | the parent is below the child in z-order; for `credential` slots, frescod's visibility record and frame |
| frame timing | limend relays role messages only, with no frame events. Role events such as `Time` are rate-limited |
| GPU caches | per-window renderers in split mode (fresco-f4-split.md) |
| audio | the child and its parent must both hold `audio`; the sound counts as the parent's ([lyra-v1.md](lyra-v1.md) §4) |
| storage and network | none shared: each jail has its own |

Timing and hardware side channels (Spectre-class) are the kernel's and GPU driver's to
handle, not Limen's.

## 11. API (Rust)

```rust
// parent side, in pergola (with limen's role types)
let embed = limen::Parent::connect()?            // LIMEN_SOCKET or /var/run/atrium/limend.sock
    .request(Role::DocViewer, window_id, slot_key)?;
embed.send(&doc_viewer::ToChild::Load { .. })?;
while let Some(ev) = embed.poll()? { /* Attached, Message, ChildLost, ... */ }

// The Window's SlotBinder is limen::Parent: slot rects go to limend as they change.

// child side
let me = limen::Child::attach()?;                 // None when not started as an embed
let role = me.role();                             // and the parent's app id, as data
let window = Window::open(&me.window_desc())?;    // embed_pending until bound
me.ready(window.id())?;
```

The C SDK waits for an Insula app that needs it.

## 12. Build plan

| step | work | owner |
|---|---|---|
| L0 ✓ | `limen` crate: the role catalogue, `Parent` and `Child`, wire types for limend (`RequestEmbed`, `Attached`, `Send`, …) | Pergola thread |
| L1 ✓ | `limend`: identity from the peer uid and the registry, window ownership checked with frescod, embeds, relay, limits, lifecycle. Linux tests with a fake launcher and a fake frescod | Pergola thread |
| L2 | frescod: the general slot op with a kind, the `_limen` allowlist, and the `embed_pending` hint | Fresco thread |
| L3 ✓ (not run on FreeBSD yet) | portcullisd: `ExecInstance` in embed mode, with the channel inherited | Pergola thread, reviewed by portcullisd's owner |
| L4 | Pergola: `limen::Parent` as the `Window`'s `SlotBinder`, `CredentialField` on it, and the child side for Pergola apps | Pergola thread |
| L5 ✓ (with a stand-in Arca) | `credential` end to end against a stand-in Arca child, in the VM in split mode. The activation probe's covers are still refused | Pergola thread |
| L6 (built 2026-11-13, unconfined in the VM run) | `doc-viewer` with the Navigator's document worker as the child: `navigator-page`, `limen::doc_viewer` (`Load` with a descriptor, `Open` with a navigatord ticket); driven by `scripts/navigator-live-vm.sh` through `limend-dev` | Pergola thread, with the Navigator thread |

**As built (2026-11-06, d6f1401).** L0 and L1 are in `limen/`:
- `catalogue`, `proto` (CLASS_LIMEN 14), `daemon`, `client::{Parent, Child}` and
  `credential`;
- `limend`, which refuses binds and launches until L2 and L3 land;
- `tests/daemon.rs`, which covers the list below and the 16-embed cap.

**L5, 2026-11-06.** `limen-embed-probe` (limen/examples) runs limend's code against a live
frescod at 5062ebd, with a stand-in Arca window opened `embed_pending`. All 8 checks pass in
the VM in both direct and split mode:
- the child is attached only once frescod has bound it;
- frescod sizes it to the slot;
- hiding and re-placing suspends and resumes it;
- detach reaches the child;
- swapped windows are not attached.

The Pergola parent side (`Embeds`, `CredentialField`) is tested on Linux against limend, but
not yet in the VM.

**With the real Arca, 2026-11-06.** `scripts/limen-arca-vm-test.sh` runs a credential field end
to end in the VM, every part a separate process as its own user: headless frescod (direct),
limend as `_limen` (65022), arca-ui as `_arca-ui`, jailed arcad, and `limen-arca-probe` as the
app. As `org.example.mail` (an account saved), all 6 checks pass: the field attaches, nothing
fills on load, an injected click on the chip fills (the username, then a handle), typing clears
it, and Tab hands over a handle for what was typed. As `org.example.evil` (same publisher,
nothing saved), no click fills. Needs the current Portcullis stack (an older jaild drops
`allow.mlock` and arcad serves nothing).

**Tests for L1:**
- a parent can't embed into a window it doesn't own;
- an unknown role and a system role's impostor are refused;
- oversized and over-rate messages are dropped;
- a child crash reaches the parent as `ChildLost`;
- a parent's exit ends its children.

**Later:**
- default-app settings and the install prompt, in Curia;
- the accessibility bridge;
- scroll chaining;
- vendor roles;
- more than one slot per child;
- embeds whose child runs on another host;
- the C SDK.

## 13. Decisions, with recommendations

**Accepted by the user, 2026-11-06: all five recommendations below.**

1. **Slots are child windows pinned by frescod**, not regions of the parent's surface.
   *Recommend yes.* It reuses what frescod just built for credentials, and it makes "no
   readback" and "no drawing over" true by construction. The cost is that parent popovers
   over an embed must be their own windows.
2. **Role messages are postcard, typed per role in Rust**; CBOR/CDDL is dropped. *Recommend
   yes*, so every Atrium wire format is the same.
3. **System roles with fixed providers** (`credential` = Arca, `sign-in` = Vestibulum).
   *Recommend yes.* No app can register to receive passwords.
4. **Each user-role embed gets its own throwaway jail** (`ExecInstance`), not a shared instance
   of the provider. *Recommend yes.* One mail can't affect the next mail's viewer. It costs
   one jail start per embed, and portcullisd's pool keeps that cheap.
5. **Rust API first; the C SDK later.** *Recommend yes.*
