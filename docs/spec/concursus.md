# Concursus — peer-to-peer channel broker

Status: design sketch (pre-implementation).
Last updated: 2026-05-21.

**Concursus** (Latin: *a coming-together / meeting*) is
Insula's peer-to-peer channel broker — the system service
that establishes symmetric device-to-device channels for
apps that need to talk to peers (other devices, other
users), with NAT traversal, end-to-end encryption, and
typed role messages on the resulting channel.

This document expands `insula.md` §19 into implementation
detail.

## 0. Position

### 0.1 What Concursus is

- A broker that **matches** two Insula apps wanting to
  speak the same peer role.
- A NAT-traversal coordinator (STUN/TURN-equivalent,
  but Insula-shaped).
- The trust-anchor for **per-peer-channel** end-to-end
  encryption: keys are derived from device identities
  attested by Vestibulum.
- The carrier of **typed role messages** on the
  established channel — same shape as Limen's typed
  embed roles, applied to peers.

### 0.2 What Concursus is not

- **Not WebRTC.** Concursus borrows the architecture
  (broker + signaling + NAT traversal) but ships with
  typed role messages on top, not raw data channels.
- **Not a generic VPN.** Apps get *typed channels for
  the role they declared*, not raw IP between devices.
- **Not the relay.** Like Tabellarius, Concursus uses
  a relay for signaling and (when needed) TURN-class
  fallback transport. The relay is configurable.
- **Not client-server.** Concursus is for symmetric
  peering. Server-side apps that listen for connections
  are a different shape (regular Aqueduct network
  service, `insula.md` §20.5).

## 1. Architecture

```
App A (device 1)            Concursus           Concursus           App B (device 2)
       │                        │                   │                       │
       │ request_peer(role,     │                   │                       │
       │   peer_identity)       │                   │                       │
       ├───────────────────────►│                   │                       │
       │                        │                   │                       │
       │                        │ signaling via known relay                 │
       │                        ├───────────────────►                       │
       │                        │                   │                       │
       │                        │                   │ peer_request          │
       │                        │                   │ (consent prompt)      │
       │                        │                   ├──────────────────────►│
       │                        │                   │                       │
       │                        │                   │ ◄──── accept ─────────┤
       │                        │                   │                       │
       │                        │ ICE / STUN / TURN-equivalent              │
       │                        │   exchange  via   relay                   │
       │                        │ ──────────────────────────────────────────│
       │                        │                                           │
       │                        │ end-to-end keys derived from              │
       │                        │ device identities (Vestibulum)            │
       │                        │                                           │
       │  channel ready         │                   │                       │
       │◄───────────────────────┤                   │                       │
       │                                                                    │
       │  ═══════════ E2E-encrypted typed channel ════════════════════════►│
       │
```

`concursusd` is a system daemon, in the TCB
(`insula.md` §24.4) because its key derivation is the
trust anchor for peer channels.

## 2. Roles for peer channels

Concursus carries roles in the same typed-message shape
as Limen embed roles (`limen.md` §2). Initial peer
roles:

### 2.1 `file-share`

Direct device-to-device file transfer.

| Direction | Message | Payload |
|---|---|---|
| Initiator → Other | `offer` | `[{ name, size, mime, hash }]` |
| Other → Initiator | `accept` / `decline` | `?selection: [indices]` |
| Initiator → Other | `chunk` | `index, offset, data` (rate-limited) |
| Other → Initiator | `progress` | `index, bytes-received` |
| Either side | `done` / `error` | … |

### 2.2 `call`

Real-time audio/video. Two sub-flavors: `call.audio` and
`call.video`.

| Direction | Message | Payload |
|---|---|---|
| Either | `media` | (opaque codec data; Opus / AV1 / etc.) |
| Either | `mute` / `unmute` | `kind: audio|video` |
| Either | `quality` | `bitrate, fps, resolution` (renegotiation) |
| Either | `hangup` | — |

Concursus does *not* mandate a codec; it carries opaque
media frames; codec choice is part of role schema
negotiation.

### 2.3 `text`

Text messaging.

| Direction | Message | Payload |
|---|---|---|
| Either | `msg` | `id, ts, content, reply-to?, attachments?` |
| Either | `typing` | `state: start|stop` |
| Either | `read` | `id` |
| Either | `ack` | `id` |

### 2.4 `game-session`

Shared real-time state for multiplayer games.

| Direction | Message | Payload |
|---|---|---|
| Either | `state-update` | role-specific |
| Either | `input` | role-specific |
| Either | `event` | role-specific |

Specific games sit on top with their own
sub-protocols; Concursus carries the typed messages.

### 2.5 `collab-edit`

Real-time collaborative editing primitive — CRDT or OT-
shaped state sync.

| Direction | Message | Payload |
|---|---|---|
| Either | `op` | CRDT/OT operation |
| Either | `presence` | cursor, selection, ID |
| Either | `request-sync` | from-version |
| Either | `sync-snapshot` | full state |

### 2.6 Vendor-specific roles

Same as Limen — apps can define their own peer roles
under reverse-DNS namespaces; Concursus mediates launch
+ identity + encryption, doesn't enforce wire
correctness.

## 3. Identity, consent, and key derivation

### 3.1 Peer identity

Each Atrium device has a device identity (Vestibulum-
managed). When App A requests a peer connection to
"App B on Bob's phone," the identifier is structured:

```
peer-target = {
  "device": device-identity-hash,    ; Bob's phone's identity
  "app": app-identifier,             ; the Insula app on that device
  ? "user": user-identifier,         ; optional persona qualifier
}
```

How an initiator knows the target identity is
out-of-band: discovery via QR code, address book,
pairing flow, etc. (Concursus does not perform
discovery — it performs *connection*).

### 3.2 Consent prompt

When Concursus on the recipient device receives a
peer_request, it surfaces a consent prompt:

```
"App A on Alice's laptop wants to connect to App B
 for role 'file-share'. Accept?"
```

The prompt shows:
- Identity of the initiator (device + app).
- The role being requested.
- (For high-stakes roles) what the role can do.

User accepts → Concursus completes the handshake.
User declines → initiator receives `EPEERDECLINED`.

For **paired** peers (devices that have previously
connected, or are part of the same user's device-set),
consent can be pre-authorized at pairing time — no
prompt per connection.

### 3.3 End-to-end key derivation

Channel encryption keys are derived from the device
identities of both endpoints via a Noise-protocol-style
handshake:

1. Initiator's Concursus sends an `ng` (Noise IK,
   Curve25519, ChaCha20-Poly1305) handshake message to
   recipient.
2. Recipient verifies initiator's identity claim
   against the device-identity in the consent prompt.
3. Recipient responds; both sides derive a shared
   session key.
4. Forward secrecy: ephemeral keys rotated per session
   (and periodically within a long-running session).

**The relay never sees plaintext.** It carries opaque
signaling + (when used) opaque media frames in the
TURN path.

### 3.4 What Concursus's daemon sees

- Which apps on this device are open to which roles.
- Which peers are connected.
- Connection timing and size metadata.
- Never plaintext (the daemon hands the channel keys
  to the apps via Vestibulum's keychain).

## 4. NAT traversal — signaling and ICE

### 4.1 Relay role

The relay's responsibilities:
- **Signaling:** forward handshake messages between
  endpoints.
- **STUN-equivalent:** reflect public address back to
  endpoints so they can attempt direct connection.
- **TURN-equivalent fallback:** carry encrypted traffic
  when direct connection fails (symmetric NAT,
  restrictive firewalls, etc.).

Default: platform-shipped public relay.
Self-hosted relays: supported, same protocol.

### 4.2 ICE-shape candidate exchange

Endpoints exchange candidate addresses (host, server-
reflexive, relayed). Standard ICE-style probing
selects the best path. Path can change mid-session
(e.g., Wi-Fi → cellular handoff).

The encryption layer is independent of the path —
session keys are not re-derived on path change; only
the transport endpoint changes.

### 4.3 Direct vs. relayed

- Same LAN: direct (mDNS-discoverable; instant).
- NAT-traversable: direct via STUN.
- Restrictive: relayed (TURN-shape; relay carries
  encrypted bytes; sees timing/size only).

For real-time roles (call, game-session), relayed
fallback adds latency. Concursus surfaces "currently
relayed" status to the role; apps can show indicators
("on relay — may be slower").

## 5. Channel lifecycle

States observable from each peer:

```
                request_peer
                     │
                     ▼
                ESTABLISHING   ── consent prompt on recipient
                     │
                     ▼
                CONNECTED
                /        \
                │         │
                ▼         ▼
            PATH_CHANGED  PEER_LOST  ── path migration / loss
                │            │
                ▼            ▼
            CONNECTED      DISCONNECTED

[either side may DISCONNECT explicitly]
```

`PATH_CHANGED` is informative — the app can react if
relevant (show "on relay" indicator). `PEER_LOST` after
a grace period (e.g., 30 s no traffic) signals
disconnect.

## 6. API

### 6.1 Initiator side (`libatrium_concursus.h`)

```c
typedef struct atrium_peer_t atrium_peer_t;

typedef struct {
    const char* device_id;       /* peer device identity */
    const char* app_id;          /* peer app identifier */
    const char* role;            /* "file-share", "call.audio", ... */
} atrium_concursus_request_t;

atrium_peer_t* atrium_concursus_request(
    const atrium_concursus_request_t* req);

int atrium_concursus_send(atrium_peer_t* peer,
                          const char* msg_name,
                          const uint8_t* payload, size_t len);

typedef struct {
    enum {
        PEER_EVENT_ESTABLISHED,
        PEER_EVENT_MESSAGE,
        PEER_EVENT_PATH_CHANGED,
        PEER_EVENT_PEER_LOST,
        PEER_EVENT_DISCONNECTED,
    } kind;
    /* event-specific payload */
} atrium_concursus_event_t;

int atrium_concursus_poll(atrium_peer_t* peer,
                          atrium_concursus_event_t* out);

void atrium_concursus_disconnect(atrium_peer_t* peer);
```

### 6.2 Recipient side

Apps that implement peer roles declare them in manifest:

```toml
[peer.implements]
"file-share"   = { schema = "1.x" }
"call.audio"   = { schema = "1.x" }
"call.video"   = { schema = "1.x" }
```

When a peer request arrives, Concursus surfaces the
consent prompt. If accepted (or auto-accepted for
paired peers), Concursus launches the implementing app
via Portcullis (if not running) with a "peer-attach"
hint; the app calls `atrium_concursus_self_attach()` to
pick up the channel.

```c
typedef struct atrium_peer_self_t atrium_peer_self_t;

atrium_peer_self_t* atrium_concursus_self_attach(void);

const char* atrium_concursus_self_role(atrium_peer_self_t*);
const char* atrium_concursus_self_initiator(atrium_peer_self_t*);

/* Then poll / send / emit as initiator side. */
```

## 7. Performance and resource

| Metric | Target |
|---|---|
| Same-LAN direct connection setup | <100 ms |
| NAT-traversable setup (STUN) | <500 ms |
| Relayed setup (TURN-equivalent) | <1 s |
| Real-time message latency (direct) | <5 ms above network RTT |
| Audio frame: encode + send + decode + play | <50 ms end-to-end on LAN |
| Idle daemon RAM | <12 MB |

## 8. Pairing for trusted peers

### 8.1 What pairing buys

A **paired** peer (e.g., two devices owned by the same
user, or two friends who have explicitly paired) can:
- Skip the per-connection consent prompt.
- Be discovered on the local network without QR-code
  exchange.
- Auto-resume connections after network changes.

### 8.2 Pairing flow

1. Initiator opens Forum → "Pair a device."
2. UI shows a QR code with device identity + ephemeral
   pairing key.
3. Recipient scans the QR with their own Forum.
4. Both devices verify identity match + record the
   pairing in Vestibulum's keychain.
5. Subsequent connections between paired devices skip
   consent.

Pairings can be revoked from Forum at any time.

### 8.3 Same-user device set

When a user signs into multiple devices with the same
Vestibulum identity, all those devices are
auto-paired. Switching devices for the same app
("continue on iPad") becomes a Concursus connection
without ceremony.

## 9. Bring-up phases

### 9.1 Phase A — same-LAN direct + file-share

- `concursusd` daemon.
- Same-LAN mDNS discovery (no external relay).
- Direct connection via UDP.
- Noise IK handshake; channel encryption.
- `file-share` role implemented end-to-end.
- Pairing flow (QR-code in Forum).

Goal: two Atrium devices on the same Wi-Fi can transfer
a file via a sample app.

### 9.2 Phase B — relay + NAT traversal

- Configured external relay.
- ICE-shape candidate exchange.
- STUN-equivalent + TURN-equivalent.
- Cross-network connectivity.

### 9.3 Phase C — additional roles

- `text` and `call.audio` end-to-end with sample app.
- Role schema versioning.

### 9.4 Phase D — same-user device-set automation

- Auto-pair across user's devices.
- Continuity-style hand-off ("continue on …" prompts).
- Multi-device session state sync.

### 9.5 Phase E — performance and reliability

- Path migration on Wi-Fi/cellular changes.
- Forward secrecy key rotation cadence.
- Cover traffic for traffic-analysis-resistant cases
  (opt-in).

## 10. Open questions

- **Relay ecosystem.** Same operational question as for
  Tabellarius: who runs public default relays? abuse
  prevention? Likely a shared concern between
  Tabellarius and Concursus — both want a "system
  relays" subsystem with shared operational practices.
- **Discovery beyond pairing.** A user wants to call
  Alice — how does the device know Alice's device-
  identity-hash? Contact-app integration with privacy-
  preserving discovery is a real subproblem; out of
  scope here.
- **TURN-class cost.** Relayed audio/video is bandwidth-
  expensive; who pays for the relay's egress? Default-
  public-relay is free at small scale; at scale, user-
  pays or app-pays models emerge.
- **Mesh sessions (>2 peers).** Conferencing, multi-
  player games. Star topology via a peer (one peer
  acts as forwarder) vs. SFU-style (relay is a
  forwarding unit) — design call deferred.
- **Game determinism / lockstep.** Some games need
  deterministic lockstep across peers. The `game-
  session` role schema needs hooks for this; spec
  belongs in a sibling game-state spec.

## 11. References

- `docs/spec/insula.md` — parent; §19 is the design
  summary.
- `docs/spec/aqueduct.md` — IPC substrate for the
  daemon-app channel.
- `docs/spec/portcullis.md` — peer-attach app launch.
- `docs/spec/vestibulum.md` (or equivalent — keychain
  spec) — device identity attestation, channel-key
  custody.
- `docs/spec/limen.md` — role-message pattern that
  Concursus's role schemas follow.
- `docs/spec/tabellarius.md` — shared "platform
  relay" operational concerns.
- `docs/NAMING.md` — naming reference.
