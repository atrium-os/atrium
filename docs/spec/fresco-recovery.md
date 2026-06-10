# Fresco — crash recovery & server-side CAS budgets

> Status: design settled 2026-06-10 (architecture review thread #4).
> Normative for the post-M2 frescod and for fresco-rs/libfresco/Pergola.
> Companion edits: wire-format.md §6.4/§6.5/§8/§10,
> fresco-production-rollout.md M2.5, aqueduct.md §6.6.

## 0. Position

Two problems, one mechanism:

1. **frescod is a singleton.** A crash today takes the whole
   desktop with it — every client dies with the transport. That
   is the standard fate of display servers (Wayland compositor
   crash kills every app; X11 session is gone), and Atrium can do
   structurally better.
2. **frescod's in-memory CAS is an unbounded shared resource.** A
   hostile or buggy client uploading unique blobs can exhaust
   server memory. GPU *time* fairness has an entire controller
   theory (atrium-gpu-scheduler.md); CAS *memory* fairness has
   nothing.

Both are resolved by taking two existing protocol rules to their
logical conclusion:

- **Retained-mode + content-addressed ⇒ the client is the source
  of truth for everything it put on the server.** Scene state is
  declarative and client-knowable; recovery is replay.
- **Sender-must-serve (aqueduct.md §6.2) ⇒ the server CAS is a
  cache, not a store.** Any blob the server can re-acquire via
  `FETCH_REQUEST` may be dropped. Eviction is safe; crash
  recovery is the degenerate case where everything was evicted.

## 1. What state lives where

| State | Owner of truth | Recoverable by |
|---|---|---|
| CAS blobs (atlases, textures, paths, subtrees) | client (immutable, hash-keyed; client must hold the bytes anyway — sender-must-serve) | client re-upload / server re-fetch |
| Slot graph (slot_id → xform / content hash / children / flags) | client (it must know current values to mutate them) | client replay |
| Windows (existence, client-side properties) | client (lifecycle ops `0x0500`+) | client replay |
| WM state (geometry, stacking, focus, workspace) | **server** — the user dragged the window; the app never knew | WM checkpoint (§4) |
| Negotiated version + extensions | both | re-handshake |
| In-flight input events during the outage | nobody | **lost** — accepted (§7) |

Everything in column one except WM state is exactly the shadow a
correct client library already implicitly holds. The design makes
that shadow an explicit contract.

## 2. Server epoch

`COMP_HELLO` (wire-format.md §8) carries a **`server_epoch`**: a
random `u64` drawn once at server start, returned to every client
at handshake.

- Reconnect sees the **same epoch** → server state survived (the
  disconnect was transport-local); no replay needed.
- Reconnect sees a **new epoch** → fresh server instance; the
  client's slots, windows, and ledger entries are gone; full
  replay required.

The epoch also future-proofs **live upgrade**: a frescod that
serializes its scene and `exec`s a new binary may keep the epoch,
and clients reconnect without replay. Out of scope for v1; the
handshake field is the enabler.

The aqueduct possession ledger (aqueduct.md §6.6) is keyed by the
epoch implicitly: a new server instance has empty ledgers, so
replayed uploads are full uploads — the recovery path and the
oracle rule compose with no special cases.

## 3. Client-replay recovery

### 3.1 Shadow state contract

A recovery-capable client library (fresco-rs, libfresco; Pergola
inherits) maintains, at all times:

- the set of `(hash, len)` it has uploaded **and still
  references** (uploads referenced by no live slot may be
  forgotten — they'll re-upload lazily if needed);
- the current value of every live slot field it has set
  (transform hash, content hash, children, flags/clip, text);
- every live window and the client-side properties it requested;
- the negotiated extension set it depends on.

This is O(scene) memory the app substantially holds anyway; the
library formalizes it. Raw-protocol clients that opt out of the
shadow simply keep today's behavior (§7).

### 3.2 Reconnect & replay sequence

On transport EOF (kqueue EOF / server-died KNOTE on the cdev,
`ECONNRESET` on socket) or `EVT_RESYNC_REQUIRED`:

1. Library emits `FRESCO_SUSPENDED` to the app. App-side render
   loop pauses; no commands accepted (queued or dropped per
   library policy).
2. Reconnect with capped exponential backoff (default: 50 ms
   initial, ×2, 5 s cap, no give-up — the supervisor decides when
   frescod is truly gone).
3. `CMD_HELLO` → `COMP_HELLO`. Same epoch → emit
   `FRESCO_RESUMED`, done.
4. New epoch → replay, strictly ordered:
   a. recreate windows (`WINDOW_*`), carrying the same
      `client_window_id`s as before (§4 identity);
   b. re-upload referenced blobs (inline or DMA; the fresh
      ledger means no early-ACK — full bytes, per
      aqueduct.md §6.6);
   c. replay the slot graph: alloc slots, set fields to current
      shadow values;
   d. re-request extension-dependent state (animations
      re-`ANIMATION_START` from current interpolated values once
      D4.5 lands).
5. Emit `FRESCO_RESUMED`. The app resumes its normal loop.

Replay is **idempotent by construction**: slot-graph ops are
set-valued (set transform, set content), not deltas, so an
interrupted replay re-run converges. The legacy scene-graph ops
(`CMD_ADD_NODE` et al.) are delta-shaped and are **not** covered
— legacy clients keep die-on-crash semantics (wire-format.md §10
rule 7).

### 3.3 What the app sees

A Pergola app sees nothing but a `SUSPENDED`/`RESUMED` pair and a
possible animation hitch. No app code participates in recovery.
Apps that draw on external clocks should treat `SUSPENDED` as a
vsync stall (stop submitting), nothing more.

### 3.4 Transport + supervision integration

- **kmod transport**: the existing `slots_alive_mask` covers
  client death; the reverse direction is added — when the
  server's privileged peer closes, the kmod KNOTEs every client
  slot so blocked clients wake immediately instead of timing
  out. Ring memory is reset by the new server instance at slot
  re-allocation.
- **Supervision**: frescod runs under the standard Portcullis
  `[supervision]` restart policy (`on-crash`, failure-budget
  retries). Nothing recovery-specific; the supervisor restarts,
  the clients replay.
- **GPU/display**: per-fd context teardown on process exit is
  GPU ABI invariant I5 (gpu-isolation.md); the new instance
  re-opens `/dev/atrium-display0` and re-modesets. Scanout shows
  the splash/last-frame per driver policy during the gap.

## 4. WM checkpoint

The only server-owned state worth preserving. frescod writes a
small advisory file (default
`/var/db/atrium/frescod/wm-state.toml`) on a debounce after WM
mutations and on clean shutdown:

```toml
schema = 1
[[window]]
app_id    = "org.atrium.edit"     # jail/manifest identity
window_id = 1                      # client-chosen, stable across restarts
rect      = [120, 80, 1024, 768]
stacking  = 3
workspace = 0
focused   = true
```

- **Identity** is `(app_id, client_window_id)`. `app_id` comes
  from the connection's jail manifest (portcullisd-attested), not
  from the client's claim. `client_window_id` stability across
  restarts is a *toolkit convention* — Pergola numbers windows in
  creation order per process; raw-protocol apps that number
  nondeterministically get cascade placement, not breakage.
- On replay, a recreated window matching a checkpoint entry gets
  its geometry/stacking/workspace restored; focus restored to the
  recorded window if it reappears within a grace window (default
  2 s), else normal focus policy.
- The file is **advisory, not correctness state**: corruption or
  staleness costs window positions only. No journaling; write to
  temp + rename is sufficient.
- Entries for apps that never reconnect age out (default: dropped
  at next clean rewrite older than 7 days).

## 5. Server-side CAS budgets

### 5.1 Accounting: the possession ledger is the charge set

Each per-connection possession ledger entry (aqueduct.md §6.6)
records `(hash, len)`. A client's **CAS charge** is the sum of
`len` over ledger entries still referenced by its live slots or
windows; unreferenced entries are charged until evicted (§5.3),
giving clients an incentive to `EVICT_HINT`.

Accounting is **logical per client**: every client referencing a
blob pays its full size; dedup is the server's gain, not the
client's discount. This mirrors tessera-quotas.md §3.2 and is
**oracle-required**, not just consistent: charging blob N bytes
to client A but 0 to client B because "someone else already has
it" would leak possession through the quota counter —
the same existence oracle closed in tessera-fs.md §20.

The budget rides the manifest's existing `[resources]` table:

```toml
[resources]
fresco_cas_max = "64MiB"    # default if unset: 64 MiB
```

### 5.2 Enforcement

An upload (`CMD_UPLOAD_BEGIN` / `CMD_UPLOAD_DMA`) or a slot op
that would materialize CAS bytes beyond the client's budget fails
with `COMP_ERROR`, status `STATUS_RESOURCE_EXHAUSTED`
(wire-format.md §6.4, allocated by this design). The connection
stays healthy — over-budget is a per-op error, not a kill. The
existing `STATUS_CAS_FULL` remains the *global* "server is out of
memory" signal; `STATUS_RESOURCE_EXHAUSTED` is *per-client*.

### 5.3 Eviction: the CAS is a cache

Because every blob's bytes are re-acquirable from a referencing
client (`FETCH_REQUEST`, sender-must-serve), frescod may evict
under global pressure:

1. **Unreferenced blobs** (no live slot/window references), LRU.
2. **Referenced-but-cold blobs** (not drawn for N frames —
   occluded windows, hidden workspaces), LRU. Re-fetched lazily
   when a referencing slot becomes visible; the frame that
   triggers the re-fetch composites without the blob (slot
   renders as placeholder per node type) and repairs on arrival.

GPU-resident copies follow the same policy; the host-RAM copy and
GPU copy are independently evictable tiers.

A client that loses its backing bytes (it evicted its own copy
after upload) and cannot serve a `FETCH_REQUEST` gets its
referencing slots degraded to placeholder — its loss, locally
contained.

### 5.4 Recovery is eviction's limit case

Crash-replay (§3.2) and post-eviction re-fetch are the same
client-side machinery: "the server doesn't have H anymore; supply
it." Implementations share the code path. This is the design's
keystone — one mechanism, two failure modes.

## 6. Demonstration artifact (phase-exit criterion)

Per ARCHITECTURE.md's demonstrator discipline, the milestone
exits with a recorded side-by-side:

1. Atrium desktop with editor + terminal + demo apps live;
   `kill -9 $(pgrep frescod)`; desktop re-renders with windows in
   prior positions within a target of **≤500 ms** (measured
   doorbell-to-first-recomposite); apps never exit; the editor's
   unsaved buffer is intact.
2. Same experiment on a Wayland compositor (every app dies) and
   X11 (session gone).
3. CAS-budget demo: hostile client loops unique uploads; hits
   `STATUS_RESOURCE_EXHAUSTED` at its 64 MiB budget; the rest of
   the desktop is unaffected; server RSS bounded.

## 7. What v1 explicitly does not cover

- **Legacy scene-graph clients** (delta-shaped ops): die on
  crash, as today. Permitted by wire-format.md §10 rule 7.
- **Raw-protocol apps without shadow state**: same.
- **Input during the outage**: lost. Keystrokes typed into a dead
  server do not replay. (Stoa-backed terminals recover their own
  stream; that's Stoa's property, not Fresco's.)
- **Mid-replay visual atomicity**: the desktop may composite
  partially-replayed scenes for a few frames. Acceptable; a
  future refinement could hold compositing until a client's
  replay-complete fence.
- **Live upgrade** (epoch-preserving handoff): enabled by the
  epoch field, not designed here.
- **Server-side scene checkpointing** (recovery without client
  cooperation): rejected for v1 — it cannot capture ring/fd
  state, doubles the persistence surface, and client replay
  makes it redundant. Revisit only if a class of shadow-less
  clients matters.

## 8. Open questions

- Budget for **system services** (Forum, Vestibulum) — larger
  static budgets in their manifests, or exempt? Lean: explicit
  larger budgets; nothing is exempt.
- Should `EVICT_HINT` be mandatory toolkit behavior on texture
  drop (Pergola image views)? Lean yes — it tightens the charge
  to truly-live content.
- WM checkpoint scope creep: should workspace *definitions* (not
  just per-window assignment) live in the checkpoint or in
  Forum's own state? Lean Forum-owned; the checkpoint stores only
  per-window facts.
- Replay storms after a crash with many clients: all clients
  re-upload simultaneously. Per-connection upload pacing exists
  (aqueduct rate limits); is that sufficient, or does the server
  need a replay-admission queue? Measure at M2.5.
