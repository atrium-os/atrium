# Atrium-ax — accessibility tree and assistive-tech service

Status: design sketch (pre-implementation).
Last updated: 2026-05-21.

**atrium-ax** is the system service that exposes the
composed accessibility tree across Insula apps to
assistive-tech apps (screen readers, voice control,
switch control, magnifiers).

This document expands `insula.md` §10.4 into
implementation detail. The locked decisions from §10.4
(AX tree = widget tree, custom-drawn UIs declare shadow
trees, AX as Aqueduct service, cross-jail composition,
publish-time gate, document viewer's role) frame this
spec — atrium-ax is the wire-format and protocol layer
that makes those decisions concrete.

## 0. Position

### 0.1 Why a separate service

The locked decision is **the widget tree IS the AX
tree** (`insula.md` §10.4). So why is there a separate
service?

Because:
- The widget tree lives inside each app's jail; no
  cross-app access by default.
- Assistive-tech apps need a *composed* view across
  every running app, plus Limen-embedded children,
  with the same handle-to-action mapping.
- The composed tree needs **subscription** semantics —
  screen readers react to focus changes, widget state
  updates, content mutations. Polling each app would
  be wrong.
- Inbound activation requests ("press this button on
  behalf of the user") need a uniform path.

`atrium-ax` is the **single subscriber-facing
endpoint**: Pergola publishes; atrium-ax composes;
assistive-tech subscribes.

### 0.2 What atrium-ax is

- The Aqueduct service that exposes the composed AX
  tree.
- The protocol layer between Pergola (which provides
  per-app trees) and assistive-tech apps (which
  consume the composed tree).
- The router for inbound activation requests.
- The throttler / coalescer for high-rate AX events.

### 0.3 What atrium-ax is not

- **Not the source of truth for tree structure.**
  Pergola is. atrium-ax composes and exposes.
- **Not the assistive-tech itself.** Screen readers,
  voice controllers, magnifiers are separate apps
  that *consume* the atrium-ax service.
- **Not a UI inspector.** The dev-tools inspector
  (`insula.md` §10.5) uses adjacent introspection
  surfaces (with broader scope: layout, paint, perf).
  AX is one slice of that information.

## 1. Architecture

```
App A (Pergola)         App B (Pergola)        atrium-doc (Pergola)
   │                       │                          │
   │ widget tree           │ widget tree              │ document AX
   │ + AX semantics        │ + AX semantics           │ tree from
   │                       │                          │ doc structure
   ▼                       ▼                          ▼
[Pergola AX publisher: typed Aqueduct stream per app]
   │                       │                          │
   └───────────────┬───────┴──────────────────────────┘
                   │
                   ▼
              atrium-ax-d
              composes across:
                - top-level windows
                - Limen-embedded children (via slot AX)
                - cross-jail composed surfaces
              throttles + coalesces updates
                   │
                   ▼
           subscriber stream (typed Aqueduct)
                   │
                   ▼
       Screen reader / voice / switch / magnifier
            (each an Insula app with the
             `accessibility-tech` capability)
```

`atrium-ax-d` is a system daemon, in the TCB
(`insula.md` §24.4) because incorrect composition can
cause assistive tech to mis-announce or fail to
activate.

## 2. The tree model

### 2.1 Node shape

Every node in the AX tree is:

```cddl
ax-node = {
  "id"        : ax-node-id,            ; opaque, stable within session
  "role"      : tstr,                  ; "button", "heading", "text-input", "list", ...
  "name"      : tstr,                  ; accessible name
  ? "description": tstr,
  "state"     : ax-state,              ; bit flags
  "children"  : [* ax-node-id],        ; ordered
  ? "value"   : tstr | uint | float,   ; for inputs / progress / sliders
  ? "extent"  : ax-rect,               ; geometric bounds on screen (visual)
  ? "actions" : [* tstr],              ; supported activations
  ? "live"    : ax-live,               ; live-region politeness
  ? "lang"    : tstr,                  ; BCP 47
  ? "level"   : uint,                  ; for headings, list items, etc.
}

ax-state = uint
  ; flags:
  ;   0x0001  FOCUSED
  ;   0x0002  SELECTED
  ;   0x0004  CHECKED
  ;   0x0008  EXPANDED
  ;   0x0010  DISABLED
  ;   0x0020  HIDDEN
  ;   0x0040  READONLY
  ;   0x0080  REQUIRED
  ;   0x0100  INVALID
  ;   0x0200  BUSY
  ;   ...

ax-live = {
  "politeness" : "off" | "polite" | "assertive",
  ? "atomic"   : bool,
  ? "relevant" : [* tstr],   ; "additions" | "removals" | "text"
}
```

### 2.2 Roles

A frozen vocabulary of role names — Pergola provides
the canonical set; vendor-specific roles allowed under
reverse-DNS namespaces.

Initial canonical roles:
- **Structural:** `document`, `region`, `landmark`,
  `group`, `list`, `list-item`, `table`, `row`,
  `cell`, `tree`, `tree-item`.
- **Headings & text:** `heading`, `paragraph`, `link`,
  `text`, `caption`.
- **Form:** `button`, `text-input`, `password-input`,
  `checkbox`, `radio`, `radio-group`, `combo-box`,
  `slider`, `progressbar`, `switch`.
- **Window chrome:** `window`, `dialog`, `menu`,
  `menu-item`, `tab-list`, `tab`, `toolbar`,
  `status-bar`.
- **Media:** `image`, `video`, `audio`.
- **Composition:** `embedded-content` (Limen slot
  node), `figure`.

ARIA mappings: roles are drawn from ARIA's vocabulary
where ARIA roles exist and have well-understood
semantics, so screen readers can use existing role-
specific announcement strategies.

### 2.3 Tree composition across jail boundaries

A Limen slot's AX node has role `embedded-content`;
its `children` are the child app's AX root + its
subtree.

The composition is **virtual** — atrium-ax doesn't
duplicate child nodes into the parent's tree; it
points at them via a (jail, ax-node-id) pair. Tree
walks transparently cross the boundary.

When a slot is hidden (`atrium_limen_set_visibility(
slot, false)`), its subtree is also hidden from AX
walks (state bit `HIDDEN`).

## 3. Subscriber protocol

### 3.1 Subscribing

Assistive-tech apps connect to atrium-ax via Aqueduct
with the `accessibility-tech` capability (declared in
their manifest; user grants explicitly at install for
this high-privilege capability).

```c
atrium_ax_subscribe(
    ATRIUM_AX_TREE_FULL,         /* snapshot + diffs */
    &subscription);
```

Other subscription modes:
- `ATRIUM_AX_TREE_FOCUSED_BRANCH` — only the focused
  branch + ancestors; cheaper.
- `ATRIUM_AX_FOCUS_ONLY` — just focus events.

### 3.2 Snapshot

On subscribe, atrium-ax sends a full snapshot:

```cbor
{
  "type": "snapshot",
  "session-id": uint,
  "root": ax-node-id,
  "nodes": [* ax-node]
}
```

Stale snapshots are tagged with a session id; if
session id changes, the subscriber discards local
state and re-snapshots.

### 3.3 Diff updates

After snapshot, updates arrive incrementally:

```cbor
{
  "type": "diff",
  "session-id": uint,
  "ops": [
    { "op": "create", "node": ax-node },
    { "op": "update", "id": ax-node-id, "fields": {...} },
    { "op": "remove", "id": ax-node-id },
    { "op": "reorder", "parent": ax-node-id, "children": [...] },
    { "op": "focus", "id": ax-node-id },
  ]
}
```

### 3.4 Throttling and coalescing

The daemon enforces a budget on event delivery to
prevent runaway streams from rapidly-mutating widgets:

- Per-app rate limit (default: 100 updates/s; high-
  rate apps must coalesce or batch).
- Per-property coalescing — if a field updates
  multiple times within a 10 ms window, only the last
  value is sent.
- Live-region semantics — `assertive` regions bypass
  coalescing for the most-recent state; `polite` is
  coalesced harder.

These choices are calibrated to prevent the
**"text-cursor that updates every keystroke flooding
the screen reader"** failure mode that browser AX
trees have historically suffered.

## 4. Inbound activations

Assistive tech can request actions:

```c
atrium_ax_invoke_action(
    ax_node_id,            /* the target node */
    "press",               /* action name */
    NULL, 0);              /* optional payload */
```

atrium-ax routes the action to the appropriate
Pergola app. Pergola translates into the widget's
activation handler — the same handler that a mouse
click would trigger. The action ID is part of the
node's `actions` field.

Supported standard actions: `press`, `set-value`,
`focus`, `scroll-into-view`, `expand`, `collapse`,
`select`, `show-context-menu`.

App-specific actions are allowed under reverse-DNS
names; assistive tech that doesn't understand them
simply doesn't expose them.

## 5. Coverage as a publish-time gate

`insula.md` §10.4 decision 5: app signing /
certification refuses bundles whose AX coverage falls
below a threshold.

### 5.1 Coverage metric

For each Pergola widget tree:

- **Total interactive nodes** = buttons, inputs,
  links, anything `actions`-bearing.
- **Named interactive nodes** = same, with a non-empty
  `name`.

Coverage = `named / total`. Threshold: ≥ 95% by
default; configurable per app type (games may have
custom thresholds with explicit user-visible
declaration of reduced accessibility).

### 5.2 Custom-drawn region coverage

A custom-drawn region (Pergola canvas widget) is a
single node by default. If the region contains
interactive elements (a custom-drawn chart with
clickable points, etc.), the app must publish a
**shadow AX tree** for the region. Otherwise the
region is treated as `unknown-interactive` — counted
against the app's coverage metric.

### 5.3 Cert pipeline integration

Opifex's verify step includes an atrium-ax-coverage
check. Apps below threshold cannot be signed by the
default registry without an explicit override (which
shows up loudly in the user's install consent).

### 5.4 Dev iteration

`portcullis dev` mode does *not* enforce coverage —
that would block iteration. It surfaces a warning in
the dev session UI ("AX coverage at 73% — production
build will be rejected at 95%").

## 6. atrium-doc and document AX

The document viewer (`insula.md` §10.6) is the place
where text-document semantics survive into AX:

- Heading levels → `heading` nodes with `level`.
- Lists → `list` + `list-item`.
- Tables → `table` + `row` + `cell` with header
  attribution.
- Links → `link` nodes with destination as
  description.
- Figures with captions → `figure` + `caption` pair.
- Live regions in dynamic documents → `live`
  attribute populated.

Document content is itself AX-tagged via the
authoring format (Markdown superset or HTML+CSS
subset); atrium-doc translates document AX into
Pergola AX which atrium-ax composes.

## 7. Inspector integration

Beyond assistive tech, the **dev-tools inspector**
(`insula.md` §10.5) uses atrium-ax to walk and display
the composed tree. The inspector is an Insula app
with both the `accessibility-tech` capability and
broader introspection capabilities (covered in
sibling spec — not this one).

Inspector use surfaces:
- "Which region of the screen lacks AX coverage?"
- "Why is this button not announced?"
- "What does a screen reader see when I focus here?"

## 8. API

### 8.1 Pergola → atrium-ax (publisher side)

Pergola publishes via per-app Aqueduct stream. The
ABI is internal to Pergola+atrium-ax; apps interact
with widgets, not with this stream directly.

### 8.2 Subscriber-side C ABI

```c
typedef struct atrium_ax_subscription_t atrium_ax_subscription_t;

typedef enum {
    ATRIUM_AX_TREE_FULL,
    ATRIUM_AX_TREE_FOCUSED_BRANCH,
    ATRIUM_AX_FOCUS_ONLY,
} atrium_ax_mode_t;

int atrium_ax_subscribe(atrium_ax_mode_t mode,
                        atrium_ax_subscription_t** out);

typedef struct {
    enum {
        AX_EVENT_SNAPSHOT,
        AX_EVENT_DIFF,
        AX_EVENT_SESSION_RESET,
    } kind;
    /* event-specific CBOR-decoded data */
} atrium_ax_event_t;

int atrium_ax_poll(atrium_ax_subscription_t* sub,
                   atrium_ax_event_t* out);

int atrium_ax_invoke_action(
    uint64_t node_id,
    const char* action,
    const uint8_t* payload, size_t len);

void atrium_ax_unsubscribe(atrium_ax_subscription_t* sub);
```

## 9. Performance and resource

| Metric | Target |
|---|---|
| Subscribe to first snapshot | <50 ms for a typical desktop with ~10 apps |
| Per-event delivery latency | <2 ms |
| Coalesced update throughput | ≤100 updates/s/app on the wire |
| Idle daemon RAM | <16 MB |
| CPU per assistive-tech connection at idle | ~0 |

## 10. Bring-up phases

### 10.1 Phase A — single-app AX

- Pergola exposes widget tree as AX nodes.
- `atrium-ax-d` daemon accepts subscribers.
- Snapshot + basic diff updates.
- A sample "list-and-button" Insula app and a sample
  screen-reader-shape consumer that announces focus.

Goal: prove the publisher → subscriber loop with
no composition.

### 10.2 Phase B — composition

- Limen `embedded-content` AX node implementation.
- Cross-jail subtree composition.
- atrium-doc's document-AX bridge.

### 10.3 Phase C — production polish

- Throttling + coalescing rules.
- Live-region semantics end-to-end.
- Inbound action routing.
- Inspector integration.

### 10.4 Phase D — cert gate

- Coverage metric calculation.
- Opifex integration: refusal of low-coverage apps in
  default registry.
- Dev-mode warnings without enforcement.

### 10.5 Phase E — assistive-tech ecosystem

- Reference screen-reader app (Atrium-flavored).
- Voice-control + switch-control reference apps.
- Magnifier app.

## 11. Open questions

- **Reference screen-reader implementation.** Who
  builds the default Atrium screen reader? Forking
  Orca / NVDA's vocabulary is reasonable; the
  rendering plumbing is Atrium-specific.
- **Internationalization of role announcements.**
  Role names are English; the locale layer for
  announcement strings (e.g., "button" → "bouton")
  belongs to the screen reader, not to atrium-ax —
  but atrium-ax should clarify this contract.
- **Live-region throttling specifics.** Exact
  millisecond budgets for `polite` vs `assertive`
  coalescing need empirical tuning with real screen
  readers.
- **Action vocabulary.** Standard actions
  (press, set-value, focus, …) covered. App-specific
  action discoverability: should atrium-ax surface
  these to subscribers in a structured way?
- **Custom-drawn region shadow trees.** How do app
  authors *generate* shadow trees efficiently for
  dynamic canvases? A Pergola helper API is needed,
  not yet designed.
- **Cert-gate threshold tuning.** 95% may be too
  strict or too lenient for some app categories;
  per-category thresholds + override workflow need
  detail.

## 12. References

- `docs/spec/insula.md` — parent; §10.4 is the design
  summary, §10.5 is the inspector pairing, §10.6 is
  the document-viewer pairing.
- `docs/spec/pergola.md` — toolkit; the publisher
  side of the AX stream.
- `docs/spec/limen.md` — cross-jail composition;
  embedded-content node semantics.
- `docs/spec/aqueduct.md` — transport for subscriber
  streams.
- `docs/NAMING.md` — (atrium-ax has no Latin name; it
  is a wire format / service, not a personifiable
  component; the descriptive name fits).
