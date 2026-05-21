# Tabellarius — push delivery protocol

Status: design sketch (pre-implementation).
Last updated: 2026-05-21.

**Tabellarius** (Latin: *Roman courier / letter-carrier*)
is Insula's push delivery service. It is the device-wide
daemon that receives encrypted push notifications from
publisher servers via a system-chosen relay and delivers
them to the target Insula app via Aqueduct.

This document expands `insula.md` §11.5 into
implementation detail.

## 0. Position

### 0.1 What Tabellarius is

- A **device-wide** daemon — one per device, all apps.
- The **delivery layer**: remote publisher → relay →
  Tabellarius → app's Aqueduct channel (or wake-on-event
  via Portcullis if no resident background exists).
- The **isolation enforcer**: an app sees only its own
  pushes; cross-app push leakage is structurally
  prevented.
- The **single network endpoint**: one long-lived TLS
  connection to the chosen relay, replacing the web's
  N-TCP-connections-per-device problem (one per
  push-enabled webapp).

### 0.2 What Tabellarius is not

- **Not Praeco.** Praeco shows notifications to the user.
  Tabellarius delivers data to the app. They are
  separate concerns; apps frequently use both
  (receive via Tabellarius, display via Praeco).
- **Not a message queue for apps to each other.**
  Local IPC is Aqueduct directly. Tabellarius is
  specifically for *remote → device* delivery.
- **Not a generic webhook receiver.** Push payloads are
  small (bounded) and arrive over the relay, not via
  publisher-pushed HTTPS to the device.

## 1. Architecture

```
Publisher's server                                     Device
       │                                                  │
       │  encrypted push                                  │
       │  (to: app-public-key)                            │
       ▼                                                  │
Push relay (system-chosen)                                │
       │                                                  │
       │  single TLS connection                           │
       │  (per-device, kept alive)                        │
       ├──────────────────────────────────────────────────┤
                                                          ▼
                                                Tabellarius (tabellariusd)
                                                          │
                                                          │  1. decrypt
                                                          │  2. identify target app via pub-key
                                                          │  3. dispatch:
                                                          │     a. if resident bg running →
                                                          │        send Aqueduct `push` message
                                                          │     b. otherwise →
                                                          │        ask Portcullis to spawn
                                                          │        triggered-bg, then deliver
                                                          │
                                                          ▼
                                                 Target Insula app
```

`tabellariusd` is in the TCB (`insula.md` §24.4) because
a leak between apps' pushes is a privacy compromise,
and the daemon decrypts payloads.

## 2. Key model

### 2.1 Per-app push keypair

At install (or first-push-subscribe), an Insula app
mints a keypair via Vestibulum keychain. The public key
is registered with the app's publisher; the private key
stays in Vestibulum, accessible only to Tabellarius (via
a Vestibulum capability scoped to "decrypt for app X").

Note: this is *not* the same as the per-service keypair
in `insula.md` §13.3 (which authenticates a sign-in
identity). Push keys are about *delivery confidentiality*
— "only this app can read these pushes."

### 2.2 Symmetric layering

Push messages are doubly-encrypted:

1. **Per-app envelope** (asymmetric, X25519 + HKDF +
   ChaCha20-Poly1305) — confidentiality to the target
   app. Tabellarius cannot read; only the target app's
   private key (via Vestibulum) can decrypt.
2. **Relay envelope** (TLS to relay; the long-lived
   device connection) — confidentiality on the network.

Tabellarius unpacks the *outer* (TLS) layer, sees an
encrypted blob + a target-key identifier, looks up the
app, and forwards the inner blob via Aqueduct to the
app (which decrypts).

**Tabellarius never sees plaintext push content.** It
sees:
- Which app a push is for (key identifier).
- Push timestamp.
- Push size.

That metadata is necessary for routing and rate-
limiting; the payload itself is end-to-end between
publisher and app.

### 2.3 Why double encryption

The outer TLS layer is for transport confidentiality.
The inner envelope is for app isolation — even if
Tabellarius is compromised, push contents to apps it
has not been explicitly given decryption capability for
are still confidential. Defense in depth.

## 3. Relay protocol

### 3.1 Relay selection

The user picks a relay at OS setup. Options:

- **Platform default relay** — operated by the OS
  vendor or a designated public relay.
- **Self-hosted** — user runs their own.
- **Third-party** — any compatible relay.

Once selected, the relay choice is persisted in Curia
and survives reboots. Switching requires user
authorization (it changes what entity sees per-app
metadata).

### 3.2 Wire protocol (device ↔ relay)

A single long-lived TLS connection with mutual auth:

- Device side: signed with device identity (Vestibulum-
  attested).
- Relay side: standard TLS cert.

Framed messages, CBOR:

```
device → relay:
  { "type": "subscribe", "keys": [...] }     ; register interest
  { "type": "unsubscribe", "keys": [...] }
  { "type": "ack", "id": ... }
  { "type": "ping" }

relay → device:
  { "type": "push", "id": uint, "to-key": bstr,
    "ts": uint, "blob": bstr }
  { "type": "pong" }
```

Push delivery is at-least-once; device acks ids; relay
retries until ack or expiry.

### 3.3 Publisher ↔ relay

Publishers push to the relay over HTTPS, addressed by
the target app's public key:

```
POST /push
Authorization: bearer publisher-token
Content-Type: application/cbor
Body: { "to": app-pub-key, "blob": encrypted-blob, "ttl": seconds }
```

Relay validates the publisher token (publishers register
with the relay; details out of scope here — different
relays have different policies), enqueues the blob for
the target key, delivers to whichever devices have
subscribed to that key.

### 3.4 No publisher → device direct connections

Insula apps **never** open inbound listening sockets for
push. Pushes always go via the relay. Eliminates the
"N TCP connections per device" problem and gives the
user one consent point ("trust this relay") instead of
N ("trust each publisher to ping you directly").

## 4. Delivery flow

### 4.1 Resident background present

The most common case for chat-shaped apps:

```
1. Tabellarius receives push from relay.
2. Looks up target app by `to-key`.
3. App has resident bg jail running.
4. Tabellarius sends Aqueduct typed `push` message on
   the app's existing channel with the encrypted blob.
5. SCM_CREDS attests the message came from Tabellarius.
6. App decrypts via Vestibulum capability.
7. App handles (update local state, post Praeco
   notification if user-visible, etc.).
8. App returns success → Tabellarius acks relay.
```

Latency: ~ms from relay arrival to app receipt.

### 4.2 No resident background — wake the app

When the app has no resident bg running (declared
triggered-bg in manifest):

```
1. Tabellarius receives push from relay.
2. Looks up target app — no resident bg.
3. Tabellarius asks Portcullis to launch the
   triggered-bg entry per manifest with event=`push`
   and the encrypted blob as payload.
4. Portcullis spawns a fresh jail from the pool
   (~500 µs).
5. Triggered-bg process boots, decrypts via Vestibulum,
   handles.
6. On completion (or SIGKILL on `max-runtime`),
   Tabellarius acks relay.
```

Latency: ~10 ms from arrival to handler execution (jail
spawn + early process startup).

### 4.3 Rate limiting and budget

Per-app `max-invocations-per-hour` from manifest
(`insula.md` §11.4) is enforced by Tabellarius. Excess
pushes are:

- **Dropped silently** if push has TTL and expires
  before next slot.
- **Coalesced** if multiple pending of same kind (per
  manifest hint).
- **Buffered** up to a small system-wide queue, then
  dropped FIFO.

Excess at relay → Tabellarius coalescing is not the
solution; the relay also has rate limits. End-to-end,
the publisher cannot drive an app harder than the
user's allowance.

## 5. App subscribe / unsubscribe

### 5.1 Subscribe flow

When an Insula app wants to start receiving pushes:

```
1. App calls atrium_tabellarius_subscribe().
2. libatrium asks Vestibulum to mint a push keypair
   for this (app, persona) tuple if none exists.
3. The public key + key-id is returned to the app.
4. App sends pub-key to its publisher's server via
   normal HTTPS (atrium_net_connect to publisher).
5. Publisher records the pub-key as a destination.
6. libatrium tells Tabellarius "subscribe to key K"
   (via Aqueduct).
7. Tabellarius tells the relay "subscribe to K."
```

### 5.2 Unsubscribe

```
1. App calls atrium_tabellarius_unsubscribe(key-id).
2. libatrium tells Tabellarius.
3. Tabellarius tells the relay.
4. Vestibulum can be asked to retire the keypair.
```

### 5.3 Re-subscription on relay change

When the user changes their relay, Tabellarius re-sends
all subscribe messages over the new relay's connection.
Publishers do *not* need to re-receive pub-keys (they
push to the key, not to the relay; the relay matches by
key).

## 6. Failure modes

| Failure | Behavior |
|---|---|
| Relay connection lost | Tabellarius retries with exponential backoff; pending pushes queue at relay for TTL; on reconnect, relay delivers buffered |
| Relay rejected device cert | Tabellarius surfaces error; user prompted via Forum to re-auth |
| App's Vestibulum keypair lost (device wipe / reset) | App must re-subscribe with fresh pub-key; old pushes for retired pub-key are undeliverable (relay TTL drops them) |
| Push decryption fails in app | App returns error to Tabellarius; Tabellarius drops the message; logs (without payload) for diagnostics |
| Triggered-bg process exceeds wall budget | SIGKILL by `rctl`; Tabellarius treats as "delivered, handler failed"; acks relay |
| Daemon crash | Tabellarius restarts; in-flight subscriptions persisted to Tessera-backed state; on restart, re-establish to relay; in-flight unacked pushes get redelivered |

## 7. Privacy posture

### 7.1 What each party sees

| Party | Sees |
|---|---|
| Relay | Per-device connection metadata; per-app push *metadata* (key id, timestamp, size); never plaintext |
| Tabellarius | Same as relay + which app maps to which key; never plaintext |
| Vestibulum (keychain) | The private key (briefly, when decrypting); never plaintext payload itself |
| App | Plaintext payload |

### 7.2 What can leak

- **Push pattern timing** — relay and Tabellarius see
  *when* pushes arrive. Active observer can correlate
  with external events to infer app behavior. Standard
  push privacy concern; cover-traffic could mitigate
  (out of scope for v1).
- **Subscription set** — relay knows which keys the
  device cares about. Pseudonymous (random keys per
  app, not per user), but a determined relay could
  correlate.

### 7.3 Mitigations available

- **Cover traffic.** Send dummy pings on a schedule so
  push patterns are not visible. Opt-in, costs battery.
- **Multiple relays.** A user can configure multiple
  relays for different apps; one app's pushes do not
  reveal anything about another's. Currently v2.

## 8. Notification handoff to Praeco

The most common app behavior on push receipt:

1. Decrypt the payload.
2. Update local state (mark message read, etc.).
3. Display notification via Praeco for user attention.

Praeco and Tabellarius are independent — apps may push
without notifying (silent background update) or notify
without pushing (locally-triggered alarm). The pairing
is convention, not requirement.

```c
/* Common pattern in a triggered-bg handler */
push_blob_t blob = atrium_tabellarius_get_blob();
unsigned char plaintext[1024];
size_t plain_len = decrypt(blob, plaintext);

/* Update local state. */
sync_inbox(plaintext);

/* Notify the user. */
atrium_praeco_post(&(atrium_praeco_notification_t){
    .title = "New message from Alice",
    .body  = "Hey, are you free…",
    .actions = ...,
});
```

## 9. API

### 9.1 C ABI (`libatrium_tabellarius.h`)

```c
typedef struct {
    char    key_id[64];
    uint8_t pub_key[32];   /* X25519 */
} atrium_tabellarius_sub_t;

/* Subscribe — mints keypair via Vestibulum, registers with daemon. */
int atrium_tabellarius_subscribe(
    const char* purpose,           /* "primary", "secondary", … */
    atrium_tabellarius_sub_t* out);

int atrium_tabellarius_unsubscribe(const char* key_id);

int atrium_tabellarius_list(
    atrium_tabellarius_sub_t* out_array, size_t cap);

/* In a triggered-bg handler */
typedef struct {
    char     key_id[64];
    uint64_t timestamp;
    uint8_t* ciphertext;
    size_t   ciphertext_len;
} atrium_tabellarius_push_t;

int atrium_tabellarius_get_push(atrium_tabellarius_push_t* out);
```

Decryption is via Vestibulum's keychain ABI, not via
Tabellarius — apps go to Vestibulum directly with the
key-id and the ciphertext.

### 9.2 Rust SDK

```rust
let sub = tabellarius::subscribe("primary").await?;
publish_to_my_server(&sub.pub_key).await?;

// In a triggered-bg handler:
let push = tabellarius::get_push()?;
let plaintext = vestibulum::decrypt(&push.key_id, &push.ciphertext).await?;
handle(plaintext);
```

## 10. Performance and resource targets

| Metric | Target |
|---|---|
| Relay-arrival → app handler (warm, resident bg) | <5 ms |
| Relay-arrival → app handler (cold, triggered bg) | <50 ms |
| Idle daemon RAM | <8 MB |
| Idle daemon CPU | ~0 (long-poll connection) |
| Sustained push throughput | 1000/s per device without backpressure |
| Battery cost (idle, connection kept-alive) | comparable to a single TLS ping every few minutes |

## 11. Bring-up phases

### 11.1 Phase A — single-relay MVP

- `tabellariusd` daemon with one configurable relay.
- Subscribe / unsubscribe + relay connection lifecycle.
- Decrypt-via-Vestibulum integration.
- Resident-bg delivery via Aqueduct.

Goal: a sample app can receive a push from a test
publisher end-to-end.

### 11.2 Phase B — triggered-bg + rate limiting

- Wake-on-push for apps with no resident bg.
- Per-app rate limits enforced.
- Coalescing per manifest hints.
- Failure recovery (relay disconnect, daemon restart).

### 11.3 Phase C — Praeco integration polish

- Standard helper API for "decrypt + display"
  triggered-bg shape.
- User-visible per-app push budget dashboard (in
  Curia).

### 11.4 Phase D — multi-relay + privacy

- Per-app relay choice (different relays for different
  apps).
- Cover-traffic opt-in.
- Privacy-pass tokens (anonymous publisher → relay
  authentication, prevents relay from linking pushes
  across an app's user base).

## 12. Open questions

- **Relay discovery and ecosystem.** Who operates
  default relays? How does a publisher find which relay
  a device uses? Publishers push to the public key, not
  the relay; the relay must accept pushes for any
  registered key, which implies relays are public
  endpoints. Operational scaling and abuse handling are
  not yet designed.
- **Key rotation for push keys.** Like publisher
  signing keys (`insula.md` §14.6), push keys may need
  rotation. App-driven (mint new, register with
  publisher, retire old) is straightforward but the
  per-publisher coordination needs design.
- **Push from non-Atrium publishers.** A web service
  sending a push to an Insula client needs a way to
  encrypt to the app's pub-key. Standard libsodium
  primitives work; we should ship a reference
  publisher SDK in multiple languages.
- **Cover-traffic design.** Specifics of dummy-push
  cadence, payload size distribution, and battery cost
  tradeoffs are unspecified.
- **Relay-side authentication of publishers.** Who can
  push to which keys? Open registration is one extreme;
  publisher-verification (signed manifests) the other.
  Likely policy-per-relay, not platform.

## 13. References

- `docs/spec/insula.md` — parent; §11.5 is the design
  summary, §11.7 is the web-replacement table.
- `docs/spec/aqueduct.md` — IPC substrate; the daemon-
  to-app delivery path.
- `docs/spec/portcullis.md` — triggered-bg spawn from
  cold.
- `docs/spec/vestibulum.md` (or equivalent — keychain
  spec) — push-key custody and decryption capability.
- `docs/spec/praeco.md` — user-visible notification
  layer that often pairs with Tabellarius.
- `docs/NAMING.md` — naming reference.
