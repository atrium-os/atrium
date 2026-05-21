# Limen — cross-jail embed broker

Status: design sketch (pre-implementation).
Last updated: 2026-05-21.

**Limen** (Latin: *threshold, doorway*) is Insula's embed
broker — the system service that mediates **cross-jail
Fresco surface composition with typed message channels**.
It is Insula's answer to `<iframe>` and to platform "intent"
systems (Android Intents, iOS App Extensions), done as a
first-class capability of Aqueduct + Fresco rather than as
a retrofit.

This document expands `insula.md` §10.3 into implementation
detail. The relationship to other specs:

- **`insula.md` §10.3** — the design summary, public framing,
  pixel-readback guarantees, side-channel posture.
- **`artifex.md` §7** — Artifex's use of Limen for its
  `editor-extension` role and Stoa terminal embeds.
- **`fresco-surfaces.md`** — surface-level compositor
  contract that Limen rides on.

## 0. Position

### 0.1 What Limen is

- The platform service that turns "I want to embed an app
  of role X" into a running jailed child whose Fresco
  surface is composed into the parent's window.
- The wire-format authority for *embed roles* — typed
  contracts defining what messages a parent and child
  exchange.
- The mediator of capability transfer at the embed
  boundary (no transitive caps; the child's caps come from
  its own manifest).
- The router for input events between the compositor and
  the embedded child.

### 0.2 What Limen is not

- **Not a window manager.** Forum + Fresco compose surfaces
  at the top level; Limen only manages the embedded-child
  case (child surface owned by a different jail, rendered
  into a slot inside the parent's surface).
- **Not a UI library.** Limen has no widgets. It allocates
  slots; what gets rendered in them is the child's affair.
- **Not a generic IPC.** Aqueduct is the generic IPC.
  Limen specifically wraps the (slot allocation + role
  contract + lifecycle + capability boundary) bundle.

## 1. Architecture

```
Parent app (Insula app in jail A)
   │
   │ atrium_limen_request_embed("doc-viewer", rect, options)
   │
   ▼
limend ────────────────► Portcullis ────────► Child app
   │                       (launch in jail B,    (Insula app
   │                        embed mode)            in jail B)
   │                                                 │
   │ ◄──── slot id + child connection ───────────────┘
   │
   ▼
Compositor (Fresco)
   │
   │  knows: parent surface has slot @ (rect)
   │         slot's content owned by jail B
   │
   ▼
Composed frame to display

         + parallel typed-message channel
         between parent and child via Aqueduct
```

`limend` is the system daemon. It is *trusted* (in the
TCB per `insula.md` §24.4) and runs as part of the Atrium
core platform. Its surface is small: launch coordination,
role lookup, capability checks, lifecycle events.

The actual surface composition is the compositor's job;
the actual message transport is Aqueduct's job. Limen is
the *broker* — it does the introducing.

## 2. Role catalogue

A **role** is a typed contract between parent and child.
Initial roles, expanded from `insula.md` §10.3.2:

### 2.1 `doc-viewer`

Renders a document URL into the slot. Read-only by
default.

| Direction | Message | Payload |
|---|---|---|
| Parent → Child | `load` | `url: string, theme?: string` |
| Parent → Child | `set_theme` | `theme: string` |
| Parent → Child | `scroll_to` | `anchor: string` |
| Child → Parent | `loaded` | `title: string, anchors: [string]` |
| Child → Parent | `error` | `code: int, message: string` |
| Child → Parent | `link_clicked` | `url: string, modifiers: ...` |
| Child → Parent | `selection` | `text: string, range: ...` |

Used by: Artifex (Markdown preview), email clients
(rendering HTML mail bodies), help systems, RSS readers.

### 2.2 `media-player`

Plays an audio/video stream.

| Direction | Message | Payload |
|---|---|---|
| Parent → Child | `load` | `url: string, mime: string` |
| Parent → Child | `play` / `pause` / `stop` | — |
| Parent → Child | `seek` | `time: float (seconds)` |
| Parent → Child | `set_volume` | `volume: float (0-1)` |
| Child → Parent | `ready` | `duration: float, codecs: [string]` |
| Child → Parent | `time_update` | `time: float` (rate-limited) |
| Child → Parent | `ended` | — |
| Child → Parent | `error` | `code: int, message: string` |

### 2.3 `picker`

One-shot resource picker (file, contact, photo, …).
Powerbox pattern (`insula.md` §5.2): user picks one item,
child returns just that item, vanishes.

| Direction | Message | Payload |
|---|---|---|
| Parent → Child | `open` | `filter: { kind, mime?, multiple? }` |
| Child → Parent | `picked` | `items: [{ fd?, uri?, metadata }]` |
| Child → Parent | `cancelled` | — |

Always short-lived. Slot dismisses on `picked` or
`cancelled`.

### 2.4 `share-target`

Inverse of picker — parent has content to hand off;
child receives.

| Direction | Message | Payload |
|---|---|---|
| Parent → Child | `share` | `items: [{ fd?, text?, uri?, mime }]` |
| Child → Parent | `accepted` | `summary: string` |
| Child → Parent | `declined` | `reason?: string` |

### 2.5 `payment`

Handles a payment flow. Specifically:

| Direction | Message | Payload |
|---|---|---|
| Parent → Child | `start` | `amount, currency, merchant, ref, methods: [string]` |
| Child → Parent | `completed` | `receipt: { ref, method, ts, signature }` |
| Child → Parent | `cancelled` | — |
| Child → Parent | `error` | `code, message` |

Payment data flows *only* between user and child (which
runs the trusted payment app's UI). Parent receives a
receipt, never card numbers or account details. Strictly
stronger than `<input type=card>`.

### 2.6 `map`

Embedded map widget with markers and navigation.

| Direction | Message | Payload |
|---|---|---|
| Parent → Child | `set_view` | `lat, lng, zoom` |
| Parent → Child | `add_marker` | `id, lat, lng, label?` |
| Parent → Child | `remove_marker` | `id` |
| Child → Parent | `view_changed` | `lat, lng, zoom, bounds` |
| Child → Parent | `marker_clicked` | `id` |
| Child → Parent | `clicked` | `lat, lng` |

### 2.7 `editor-extension`

Detailed in `artifex.md` §7.2. Wire-format ownership:
`limen.md` defines the *role*; `artifex.md` defines what
Artifex specifically does with it.

### 2.8 Adding new roles

A role is defined by:
1. A registered name (string, `category.kind` form;
   reverse-DNS allowed for vendor-specific roles).
2. A typed message schema (CBOR, versioned).
3. A capability profile — what the child app's manifest
   may declare to implement the role.
4. Defaults and lifecycle policy.

Platform-blessed roles live in the canonical Limen
catalogue and ship with the platform. Vendor-specific
roles can be added under reverse-DNS namespaces with
their own schemas; the platform does not enforce
correctness of vendor roles but Limen still mediates
launch, capability, and slot mechanics.

## 3. Role implementation registration

### 3.1 In the app manifest

An app declares which roles it implements:

```toml
[role.implements]
"doc-viewer" = { schema = "1.x" }
"share-target" = { schema = "1.x", mime = ["text/*", "image/*"] }
```

Limen indexes this at install time; the index lives in
Curia (system settings) as a (role, schema-version) →
[apps] map.

### 3.2 Default-app selection

When multiple apps implement the same role:

1. **First install wins** transiently — until user picks.
2. **User explicitly picks** in Curia → "Default apps"
   the first time a role is invoked with no default set.
3. Subsequent invocations use the user's pick until
   re-set.
4. Per-role schema versioning means an app implementing
   an old schema can coexist with one implementing a new
   schema; Limen picks the highest schema both endpoints
   support.

### 3.3 No implementation installed

If a role is requested and no implementing app exists:

1. Limen prompts the user via a system dialog: "App X
   wants a `<role>`. Install a `<role>` provider?"
2. The dialog suggests platform-default candidates from
   the configured Opifex registry.
3. If the user declines, the parent gets
   `EMBED_NO_PROVIDER` and handles gracefully.

## 4. Launch protocol

Concrete sequence for `request_embed("doc-viewer", rect, opts)`:

```
Parent                       limend                      Compositor          Child
  │  request_embed             │                             │                 │
  ├───────────────────────────►│                             │                 │
  │                            │ resolve role "doc-viewer"   │                 │
  │                            │ → user's default = atrium-doc│                │
  │                            │                             │                 │
  │                            │ ask Portcullis to launch    │                 │
  │                            │ atrium-doc in embed mode    │                 │
  │                            │                             │                 │
  │                            │                             │     execve      │
  │                            │ ───────────────────────────►│ ───────────────►│
  │                            │                             │                 │
  │                            │ allocate slot in parent's   │                 │
  │                            │ surface                     │                 │
  │                            ├────────────────────────────►│                 │
  │                            │                             │                 │
  │                            │ wire slot ↔ child surface   │                 │
  │                            ├────────────────────────────►│                 │
  │                            │                             │                 │
  │                            │ open typed Aqueduct channel │                 │
  │                            │ parent ↔ child for "doc-viewer" role         │
  │                            ├──────────────────────────────────────────────►│
  │                            │                             │                 │
  │  attached(slot_id, channel)│                             │                 │
  │◄───────────────────────────┤                             │                 │
  │                                                          │  child renders  │
  │  load("atrium-doc://...")                                │                 │
  ├──────────────────────────────────────────────────────────┼────────────────►│
```

Latency budget for cold launch:
- Limen role lookup: <1 ms.
- Portcullis fork+exec+jail-setup: ~500 µs from pool
  (`insula.md` §8).
- Slot allocation + wiring: ~100 µs.
- Channel open: ~100 µs.
- First message delivery: ~50 µs.

**Cold attached event in <1 ms typical.** First user-
perceivable content depends on the child's startup (file
load, render, etc.).

## 5. Surface slot mechanics

### 5.1 Slot allocation

When Limen allocates a slot, it tells the compositor:

```
slot_id      : opaque handle
parent_surface : (jail, surface)
slot_rect    : (x, y, w, h) in parent-surface coords
child_surface_owner: jail B (just launched)
input_policy : { keyboard, pointer, scroll, drop }
audio_policy : NONE | OWN_CONTEXT
transparency : OPAQUE | ALPHA_BLEND
z_policy     : ABOVE_PARENT_CONTENT
```

The compositor enforces:
- The child's surface is composed *into* the parent's
  surface at `slot_rect`.
- The parent's process can render *above* the slot
  (overlays in parent surface) but **cannot read** the
  child's pixels.
- The child's process renders *only* into the slot's
  bounds; clipping enforced by compositor.

### 5.2 Resize

Parent can resize the slot:

```c
atrium_limen_resize_slot(slot_id, new_rect);
```

Compositor notifies child of new viewport; child re-lays-
out. Standard surface-resize semantics.

### 5.3 Visibility

Parent can hide/show:

```c
atrium_limen_set_visibility(slot_id, visible);
```

Hidden slots: child is paused (no input, no frame
updates). Re-shown: child resumes. This is how panel-
based UIs in Artifex hide their sidebar extensions
without tearing them down.

### 5.4 Detach

Parent can detach the slot (terminate the child):

```c
atrium_limen_detach(slot_id);
```

Child gets SIGTERM with grace period; slot is freed;
parent gets `detached` event.

## 6. Input routing

### 6.1 Pointer events

Compositor hit-tests the cursor:
- Inside slot rect → routed to child (in child's
  coordinate space).
- Outside slot → parent.
- Multi-touch / gesture events broken across slot
  boundaries → routed by primary touch point.

The parent never sees pointer events that land inside a
slot, even for forwarding purposes. (If the parent
explicitly requests `input_policy.pointer = NONE`, the
slot is decorative and events pass through to parent.)

### 6.2 Keyboard

Routed based on focus. Pergola's focus model:
- Each surface has a *focused widget*.
- When a slot is the focused widget, keyboard events go
  to the child.
- When the child's internal focus is on a non-text
  widget (e.g., a button), keyboard events may bubble
  back via Limen-mediated focus transfer.

Modifier-only system shortcuts (Cmd-Tab, Cmd-Q, etc.) go
to the WM, never to the child or parent.

### 6.3 Scroll

Within slot rect → child. Outside → parent. Scroll
chaining (when child reaches scroll boundary, parent
scrolls) is opt-in per role; default off.

### 6.4 Drag-and-drop across the boundary

Mediated by the system DnD service:
- User initiates drag in parent.
- Cursor crosses into slot rect → system shows drop-
  zone indicator; child gets drag-enter event.
- Drop → system mints a one-shot capability for the
  dropped content (file fd, text, URI list); delivers to
  child.

Neither side fakes the drag content; the system is the
trust anchor for DnD across jail boundaries.

## 7. Lifecycle and error handling

### 7.1 States observable from the parent

```
                request_embed
                     │
                     ▼
                 LAUNCHING
                     │
                     ▼
                  ATTACHED
                  /      \
                 /        \
                ▼          ▼
              SUSPENDED   CHILD_LOST
              (hidden)      │
                │           ▼
                ▼        DETACHED
              ATTACHED
```

Parent receives messages:
- `attached(slot_id, channel)` — embed live.
- `suspended` / `resumed` — visibility changes.
- `child_lost(reason)` — child crashed, exceeded
  resource limits, exited unexpectedly.
- `detached(reason)` — orderly shutdown.

### 7.2 Child crash handling

A crashed child:
1. Compositor sees disconnect, holds last frame as a
   placeholder.
2. limend detects, sends `child_lost(reason)` to parent.
3. Parent decides: retry (request_embed again), show
   "extension stopped" UI, or detach permanently.

The editor in Artifex shows "Extension X stopped
responding [Restart]" in its place; the user clicks
Restart, parent re-invokes `request_embed`.

### 7.3 Resource exhaustion

`rctl` limits on the child's jail are enforced by the
kernel. When exceeded:
- CPU: child is throttled but stays alive.
- RSS: child is SIGKILL'd; `child_lost("rss_exceeded")`.
- Wall time: SIGKILL'd; `child_lost("wall_exceeded")`.

Defaults are conservative; manifests can request higher
limits (and the user reviews them at install).

## 8. Capability and consent

### 8.1 Child's capabilities are its own

This is load-bearing and worth restating: **the child's
capabilities come from the child's own manifest, not the
parent's.** The parent cannot grant the child anything.
The parent cannot smuggle elevated capabilities. The
parent cannot pin a vulnerable old version.

Trust is symmetric — parent and child each see what they
see, neither has authority over the other.

### 8.2 The user's choice of default app is the trust act

When the user picks (e.g.) atrium-doc as their default
`doc-viewer`, they are *trusting that app* for embeds of
that role across the platform. The trust statement is
not "I trust app X" but "I trust app Y to render
document content when app X asks for it."

This is a more honest trust model than the web's same-
origin policy — the user makes one capability decision
per role, not zero.

### 8.3 Per-embed consent (rare)

For high-stakes roles (payment, identity-claim), Limen
may surface a per-invocation consent UI even if the
default app is set. The role's metadata declares whether
per-embed consent is required. Default-no; opt-in by
role.

## 9. Accessibility composition

The AX tree spans the boundary (`insula.md` §10.4
decision 4). Concretely:

- The compositor's slot has an AX node with
  `role = embedded-content`.
- atrium-ax (the AX service) subscribes to the child's
  AX subtree.
- The subtree is grafted into the parent's tree at the
  slot's AX position.
- Screen readers traversing the parent see one
  continuous tree.

When the slot is hidden, its AX subtree is hidden too.
When the child sends an AX-relevant update, Limen
propagates it to atrium-ax which propagates it to
subscribers.

## 10. Side channels and mitigations

Restating `insula.md` §10.3.6 in implementation terms:

| Channel | Mitigation |
|---|---|
| Pixel readback | Compositor architecturally never gives parent the child's pixel buffer. The shared GPU memory the parent has access to does not include the child's allocations. |
| Render-completion timing | Limen does not surface frame-precise rendering events to the parent. Role-level events (`loaded`, `time_update`) are coalesced. |
| Shared GPU caches | Per-jail GPU contexts at the driver layer (Atrium GPU ABI requirement). Cross-jail GPU work cannot share cache lines. |
| Audio capture | Audio output requires the `media-output` capability; embed `audio_policy` controls whether the child can play audio at all. |
| Storage / network sharing | None — each jail has its own; the embed boundary does not connect them. |

The damaging-but-residual concerns (timing attacks,
hardware side channels, Spectre-class) are addressed at
the Atrium kernel + GPU driver layer, not at Limen.
Limen surfaces what is enforceable architecturally and
relies on the lower layers for the rest.

## 11. API

### 11.1 Parent-side (`libatrium_limen.h`)

```c
typedef struct atrium_limen_t atrium_limen_t;

typedef struct {
    enum atrium_limen_input_t input;     // FULL | NONE | DECORATIVE
    enum atrium_limen_audio_t audio;     // NONE | OWN_CONTEXT
    enum atrium_limen_alpha_t alpha;     // OPAQUE | ALPHA
} atrium_limen_options_t;

atrium_limen_t* atrium_limen_request_embed(
    atrium_window_t* window,
    atrium_rect_t   rect,
    const char*     role,
    const atrium_limen_options_t* opts);

int atrium_limen_send(atrium_limen_t* slot,
                      const char* msg_name,
                      const uint8_t* payload, size_t len);

typedef struct {
    enum {
        EMBED_EVENT_ATTACHED,
        EMBED_EVENT_MESSAGE,
        EMBED_EVENT_SUSPENDED,
        EMBED_EVENT_RESUMED,
        EMBED_EVENT_CHILD_LOST,
        EMBED_EVENT_DETACHED,
    } kind;
    /* event-specific payload */
} atrium_limen_event_t;

int atrium_limen_poll(atrium_limen_t* slot,
                      atrium_limen_event_t* out);

void atrium_limen_resize(atrium_limen_t* slot,
                         atrium_rect_t new_rect);
void atrium_limen_set_visibility(atrium_limen_t* slot,
                                 bool visible);
void atrium_limen_detach(atrium_limen_t* slot);
```

### 11.2 Child-side (`libatrium_limen_self.h`)

```c
typedef struct atrium_limen_self_t atrium_limen_self_t;

/* Called by an Insula app when launched in embed mode.
   Returns NULL if not in embed mode. */
atrium_limen_self_t* atrium_limen_self_attach(void);

const char* atrium_limen_self_role(atrium_limen_self_t*);

typedef struct {
    const char* msg_name;
    const uint8_t* payload;
    size_t len;
} atrium_limen_self_msg_t;

int atrium_limen_self_poll(atrium_limen_self_t*,
                           atrium_limen_self_msg_t* out);

int atrium_limen_self_emit(atrium_limen_self_t*,
                           const char* msg_name,
                           const uint8_t* payload, size_t len);

atrium_window_t* atrium_limen_self_window(atrium_limen_self_t*);
/* The window the embed is rendering into; the child uses
   this with normal Pergola/Fresco calls. */
```

### 11.3 Rust SDK

Thin wrapper above the C ABI; idiomatic Rust types
(`Result<T, EmbedError>`, async `poll_event`, typed
message structs derived from the role schema).

## 12. Performance targets

| Metric | Target |
|---|---|
| Role lookup | <1 ms |
| Cold launch via Portcullis from jail pool | ~500 µs |
| Slot allocation + wiring | ~100 µs |
| Channel open | ~100 µs |
| **`attached` event (cold)** | **<1 ms** total |
| Per-message latency on warm channel | ~5–20 µs (Aqueduct local) |
| Resize / visibility change | <100 µs |
| Detach (orderly) | ~500 µs grace + cleanup |

## 13. Bring-up phases

### 13.1 Phase A — core mechanism

- `limend` daemon stub: role lookup against an in-memory
  catalogue; Portcullis launch coordination; slot
  allocation against Fresco; Aqueduct channel wiring.
- Two roles implemented end-to-end: `picker` (the simplest
  short-lived case) and `editor-extension` (Artifex's
  load-bearing case).
- Parent-side and child-side C SDKs at the level of §11.

Goal: Artifex can run extensions; Scrinium can be invoked
as a picker via Limen.

### 13.2 Phase B — full initial role catalogue

- `doc-viewer`, `media-player`, `share-target`, `payment`,
  `map` schemas + a reference child app for each.
- Default-app management UI in Curia.
- Per-embed consent flow for `payment`.

### 13.3 Phase C — A11y bridge

- Composition with atrium-ax (sibling spec, planned).
- AX subtree subscription, slot AX node propagation.
- Inspector app integration: an inspector can walk the
  composed tree across jail boundaries.

### 13.4 Phase D — production hardening

- Resource exhaustion handling with sensible defaults
  per role.
- Crash-and-restart polish.
- Drag-and-drop across boundary, integrated with the
  system DnD service.
- Performance optimization to meet §12 targets.

## 14. Open questions

- **CBOR schema language.** Role schemas should be
  formally specified; CBOR-via-CDDL is the obvious
  candidate but tooling and Rust ergonomics need
  evaluation.
- **Role versioning policy.** Schema bumps within a
  major version are backward-compatible additions;
  major bumps are coordinated. Detailed policy TBD.
- **Vendor role mediation.** Platform mediates launch /
  capability / slot for vendor roles, but does not
  enforce wire correctness — is that the right line?
  An "official" vs. "experimental" tier for roles may
  be warranted.
- **Multiple slots from the same child.** Today's model is
  one slot per embed instance. A child app implementing
  a role might want to render into multiple slots (e.g.,
  a sidebar + a status-bar segment). Two-slot extension
  shape needs design.
- **Long-running vs. short-lived embeds.** Pickers detach
  immediately; terminals last for hours; assist
  extensions may persist across multiple workspaces. The
  default lifecycle policy per role is part of role
  metadata; specifics TBD.
- **Cross-host embedding.** Can a Limen slot's child run
  on a remote Atrium host (combining `insula.md` §20.2
  with embed)? Architecturally yes (Aqueduct + Fresco
  are network-transparent); operationally, latency
  budgets need tightening. Defer to v2.

## 15. References

- `docs/spec/insula.md` — parent spec; §10.3 is the
  design summary.
- `docs/spec/artifex.md` — §7 is the IDE's use of Limen.
- `docs/spec/fresco-surfaces.md` — surface-level
  compositor contract.
- `docs/spec/aqueduct.md` — IPC substrate; the typed
  message channel.
- `docs/spec/portcullis.md` — jail launcher; how Limen
  asks for embedded-mode child launches.
- `docs/NAMING.md` — naming reference (Limen entry).
- Future sibling: `docs/spec/atrium-ax.md` — AX tree
  composition referenced from §9.
