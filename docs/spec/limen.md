# Limen: the cross-jail embed broker

Status: v1 design, for review (rewritten 2026-11-06 from the May 2026 sketch). Nothing is
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

`limend` is a small trusted daemon, running as its own user `_limen`. It holds no secrets and
draws nothing. It knows four things: the role catalogue, who each connection is, which embeds
exist, and which slot each one is in.

**Identity is the connection, never a message** (as in [arca.md](arca.md) §4.1):

- A **parent** connects to limend's socket. limend reads the peer uid and resolves it to
  (user, app id) through the launch registry (`portcullis_peer::AppRegistry`). A parent can
  embed only into a window it owns: limend checks the window's `owner_uid` with frescod.
- A **child** is started by portcullisd at limend's request and inherits its end of the
  channel as a descriptor, so limend knows which embed it is without asking. A system
  provider that is already running (Arca, Vestibulum) instead connects to limend itself. It
  is recognised by its uid, which the catalogue names (§2.0).

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
| child → parent | `Loaded` | `title: String` (at most 256 bytes), `anchors: Vec<String>` (at most 256 of them) |
| child → parent | `LinkActivated` | `url: String`. Only after a press in the child; the parent decides whether to open it |
| child → parent | `Error` | `code: u16` |

The child never sends the parent document text. A selection leaves only through the
clipboard, on the user's Ctrl-C in the child (tabula.md).

### 2.2 `media-player`

| direction | message | payload |
|---|---|---|
| parent → child | `Load` | `media: DocRef`, `mime` |
| parent → child | `Play`, `Pause`, `Stop` | none |
| parent → child | `Seek` | `secs: f64` |
| parent → child | `SetVolume` | `volume: f32` (0 to 1) |
| child → parent | `Ready` | `duration_secs: f64` |
| child → parent | `Time` | `secs: f64`, at most 4 a second |
| child → parent | `Ended` | none |
| child → parent | `Error` | `code: u16` |

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

- **User roles**: limend asks portcullisd for an `ExecInstance` of the provider app in embed
  mode. That's one jail per embed, discarded at exit, with the provider's own manifest
  capabilities. The parent's identity goes to the child as data (`Attach { parent_app }`),
  never as authority.
- **System roles**: limend asks the provider, which is already running, for a new instance
  over the provider's own connection. The provider opens a new window for it.
- The child's window is not shown on its own until it is bound. frescod keeps a window
  hidden while it is waiting to be bound (the `embed_pending` window hint), so a child can't
  flash up as a top-level window.
- `Attached` reaches the parent only after frescod has bound the slot.

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
| audio | needs the child's own `media-output` capability |
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
| L0 | `limen` crate: the role catalogue, `Parent` and `Child`, wire types for limend (`RequestEmbed`, `Attached`, `Send`, …) | Pergola thread |
| L1 | `limend`: identity from the peer uid and the registry, window ownership checked with frescod, embeds, relay, limits, lifecycle. Linux tests with a fake launcher and a fake frescod | Pergola thread |
| L2 | frescod: the general slot op with a kind, the `_limen` allowlist, and the `embed_pending` hint | Fresco thread |
| L3 | portcullisd: `ExecInstance` in embed mode, with the channel inherited | Pergola thread, reviewed by portcullisd's owner |
| L4 | Pergola: `limen::Parent` as the `Window`'s `SlotBinder`, `CredentialField` on it, and the child side for Pergola apps | Pergola thread |
| L5 | `credential` end to end against a stand-in Arca child, in the VM in split mode. The activation probe's covers are still refused | Pergola thread |
| L6 | `doc-viewer` with the Navigator's document worker as the child | Pergola thread, with the Navigator thread |

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
