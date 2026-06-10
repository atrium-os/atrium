# Atrium-RPC — unified IPC substrate

Status: design + substrate done; first production consumer landed (2026-05-08).
Last updated: 2026-05-08.

> **Implementation status (2026-05-08):** The substrate
> (`aqueduct/`: Connection, envelope, CAS, classes registry) was
> live in earlier passes; aqueduct-echo demo exercises the full
> CAS upload/fetch/event path. New this round:
>
> - **`CLASS_PORTCULLIS = 6`** registered for portcullisd's
>   capability-mediation opcode dictionary
>   (`portcullis-protocol` crate). Three opcodes:
>   `OP_ATTACH_MOUNT`, `OP_DETACH_MOUNT`, `OP_MOUNT_REPLY`.
> - **First production consumer:** `atrium-portcullisd-daemon` +
>   `atrium-portcullisd-aq` client. An in-jail `uid 1001` process
>   talks aqueduct over a per-capability bind-mounted socket
>   (`/atrium/sockets/portcullisd/portcullisd.sock`) to drive
>   runtime mount attach/detach. Verifies §6.1 (capability =
>   mount) and the "no daemon-side trust check on the path"
>   property end-to-end, with peer-uid → manifest cross-check
>   for defense in depth.
> - **Fresco migration onto aqueduct** still pending — see
>   `spec/fresco-production-rollout.md` for the M2 cutover plan.

This document specifies the aqueduct substrate used by every
Atrium service that needs IPC between two processes — including
between processes in different jails. It is the foundation
underneath Fresco (display), atrium-broker (URI handler),
clipboard, notifications, audio control plane, and any future
services.

## 1. Goals

- **One protocol envelope** across every Atrium service. One client
  library, one debugging lens, one schema-evolution policy.
- **Capability-scoped via the filesystem.** Which services a jail
  can talk to is encoded as which sockets exist in its mount
  namespace. The kernel enforces this; no userspace daemon is in
  the trust path for the capability check itself.
- **Content-addressed payloads.** All non-trivial payloads
  reference content by hash. Shared CAS namespace means an
  image displayed by Fresco AND copied to clipboard AND in a
  notification is **one** allocation.
- **Tessera integration.** Tessera CAS hashes and aqueduct
  hashes share a namespace. Trusted services can read content
  by hash directly from Tessera storage, bypassing the wire.
- **High-bandwidth via shm + fd-passing.** Bulk data (decoded
  video frames, GPU textures) goes through anonymous shm
  rendezvous; the fd is the capability.
- **Native FreeBSD primitives.** Unix domain sockets, nullfs,
  SCM_RIGHTS fd-passing, SHM_ANON. No new kernel surface.

## 2. Non-goals

- Not a marshalling library. Per-service opcodes define their
  own payload schemas (postcard, manual binary, whatever fits).
  aqueduct gives them an envelope to ride in.
- Not a service discovery / naming framework. Service rendezvous
  is done by readdir on `/atrium/sockets/`. No registry.
- Not a security boundary in itself — that's the service's
  job. aqueduct gives services a clean substrate; trust
  decisions remain with the service.
- Not a replacement for sockets when sockets fit. A streaming
  audio pipe with millions of small messages can use the
  envelope, but doesn't have to.

## 3. Architecture

### 3.1 Layers

```
+-----------------------------------------------+
| Per-service opcode dictionary                 |
| (display | clipboard | notify | broker | ...) |
+-----------------------------------------------+
| aqueduct envelope                           |
|   - opcode-tagged messages                    |
|   - bidirectional (client ↔ service)          |
|   - async events alongside RPC completions    |
+-----------------------------------------------+
| CAS layer                                     |
|   - UPLOAD_BEGIN / DATA / FINISH              |
|   - REFER_BY_HASH (with optional tessera_path)|
|   - hash-keyed cache (per-process)            |
+-----------------------------------------------+
| Transport                                     |
|   - SOCK_STREAM Unix domain socket            |
|   - SCM_RIGHTS fd-passing                     |
|   - SHM_ANON for bulk shm rendezvous          |
+-----------------------------------------------+
```

### 3.2 Wire format

All messages share an envelope:

```
+--------+-------------+-------+-------+--------------+
| ver(1) | opcode_class| op(2) | flags | length(4)    |   payload (length bytes)
| u8     | u8          | u16   | u16   | u32 LE       |
+--------+-------------+-------+-------+--------------+
```

| Field          | Notes                                       |
|----------------|---------------------------------------------|
| `ver`          | Envelope version. Currently 1.              |
| `opcode_class` | Top-level dictionary selector. 0 = aqueduct (CAS, events). 1 = display (Fresco). 2 = clipboard. 3 = notify. 4 = broker. 5 = audio-control. 6 = portcullis (capability-mediated runtime mounts; `portcullis-protocol` crate). 7 = input (HID-native keyboard/pointer/gamepad; `spec/atrium-input.md`). 8..62 reserved. 63 = echo (smoke/fuzz). 64..255 vendor/private. |
| `op`           | Opcode within the class. Class-specific dictionary. |
| `flags`        | Bit 0: payload contains hash refs (receiver should consult cache). Bit 1: response expected. Bit 2: this is a response. Bit 3: async event (no response expected). Bits 4..15 class-specific. |
| `length`       | Payload byte count (excluding envelope).    |
| `payload`      | Class+opcode-defined.                       |

Min message: 10 bytes. Max payload: `2^32 - 1` (in practice capped per-service).

### 3.3 The CAS layer (opcode_class = 0)

Built-in opcodes. Every aqueduct speaker implements these.

| op   | name             | direction      | purpose |
|------|------------------|----------------|---------|
| 0x01 | UPLOAD_BEGIN     | client→server  | "I will send blob H of size N" |
| 0x02 | UPLOAD_DATA      | client→server  | continuation chunks            |
| 0x03 | UPLOAD_FINISH    | client→server  | end-of-blob                    |
| 0x04 | UPLOAD_ACK       | server→client  | "I have H now"                 |
| 0x05 | FETCH_REQUEST    | server→client  | "send me bytes for H" (fallback when receiver doesn't have it) |
| 0x06 | FETCH_BEGIN      | client→server  | response to FETCH_REQUEST      |
| 0x07 | TESSERA_PROBE    | server→client  | "is H in your reachable Tessera CAS?" (advisory) |
| 0x08 | EVICT_HINT       | client→server  | "you can drop H from your cache; I won't refer to it again" |
| 0x09 | SESSION_HANDSHAKE | client→server | remote-session auth; first message on any non-`SO_PEERCRED` transport (aqueduct-remote.md §3) |
| 0x0A | SESSION_ACCEPT   | server→client  | negotiated session caps + ledger scope; non-zero status = rejected |
| 0xFF | NEGOTIATE_CAPS   | both           | exchange supported opcode_classes + version |

Hash format: SHA-256 (32 bytes), matching Tessera. Hashes are
**advisory pointers**, not capability tokens — see §6. Whether a
receiver may *reveal* that it already has H (early `UPLOAD_ACK`,
`TESSERA_PROBE` answers) is governed by the possession-scoping
rule in §6.6.

### 3.4 Service opcode dictionaries

Each service registers an `opcode_class` and publishes its
dictionary as a Rust crate (`fresco-protocol`, `aqueduct-
clipboard`, etc.). The class registry lives in this document
and in `aqueduct/src/classes.rs` as a single source of
truth.

A service implementation:

```rust
trait AtriumService {
    const OPCODE_CLASS: u8;
    type Request;
    type Response;
    type Event;

    fn handle(&mut self, req: Self::Request) -> Self::Response;
}
```

Boilerplate (envelope decode, CAS handling, event dispatch) lives
in aqueduct; only `handle` and the `Request`/`Response`/
`Event` types are per-service.

## 4. Transport

### 4.1 Sockets and rendezvous

Each service exposes one Unix-domain socket on the host:

```
/atrium/sockets/fresco.sock
/atrium/sockets/clipboard.sock
/atrium/sockets/notify.sock
/atrium/sockets/broker.sock
```

**Permissions:** owned by the service's UID, mode 0600. The
service decides which client UIDs can connect (typically the
session user only).

**Cross-jail:** Portcullis (D2.5) reads each app's `atrium.toml`
and for every declared capability, adds a `mount.nullfs` entry
mounting just that socket into the jail's `/atrium/sockets/`.
A jail can only see sockets it asked for.

```toml
# atrium.toml example
[capabilities]
graphics  = "fresco"     # → mount /atrium/sockets/fresco.sock
clipboard = true         # → mount /atrium/sockets/clipboard.sock
notify    = true         # → mount /atrium/sockets/notify.sock
network   = "loopback"   # (orthogonal — devfs.rules)
filesystem = ["~/Documents"]  # (orthogonal — nullfs of paths)
```

### 4.2 Big payloads via shm + fd-passing

For payloads where the bytes are not in CAS (decoded video frames,
GPU textures, raw audio buffers) and don't make sense to hash:

1. Producer creates `shm_open(SHM_ANON, ...)`, gets fd.
2. Producer sends an aqueduct message describing layout (size,
   stride, format, etc.) and includes the fd via `SCM_RIGHTS`.
3. Consumer receives the fd, mmaps. Producer signals updates via
   subsequent normal messages (or an eventfd-style doorbell).

The shm is **anonymous** — only processes that received the fd
can map it. The fd is the capability. This crosses jail boundaries
exactly when the UDS does (because Portcullis nullfs-mounted the
socket).

Fresco's existing buffer-upload pattern is exactly this; it'll be
the reference implementation.

## 5. Versioning

### 5.1 Envelope version

Currently 1. Reserved bytes in the envelope header allow
extension without a version bump. A future version 2 must keep
the first byte (`ver`) as the version field at the same offset,
so unrecognised versions can be cleanly rejected.

### 5.2 Opcode classes

Classes 0..63 are reserved for Atrium core. Classes 64..255 are
vendor / experimental. New core classes added by editing this
document and `aqueduct/src/classes.rs` together.

### 5.3 Per-class opcodes

Each class manages its own opcode space. The convention:

- 0x00..0x7F: stable opcodes. Once shipped, never repurposed.
- 0x80..0xFF: experimental / per-service-version. May change.
- 0xFF in any class: NEGOTIATE_CAPS-like discovery (per-class
  feature flags).

Receivers MUST silently drop messages with unknown opcodes
(within a known class) UNLESS a `response expected` flag is set,
in which case respond with an "opcode unknown" error.

### 5.4 Schema evolution within a class

Per-service responsibility. aqueduct doesn't impose a marshalling
format — services pick (postcard for Rust↔Rust, hand-rolled binary
for cross-language, etc.). A common pattern: a leading version byte
in the payload, with backwards compatibility maintained at the
service.

## 6. Jails — how this all hangs together

### 6.1 Capability = mount

Portcullis materialises an app's IPC capabilities at jail-start
time as a directory tree:

```
(in the jail's mount namespace)
/atrium/sockets/
    fresco.sock         ← from `graphics = "fresco"`
    clipboard.sock      ← from `clipboard = true`
    notify.sock         ← from `notify = true`
    (nothing else)
```

The kernel enforces this — there is no daemon-side check the app
could trick. If a socket isn't mounted, the path doesn't exist.

### 6.2 CAS hashes are advisory pointers, not capability tokens

A hash arriving in a message **does not grant access** to the
content. The receiver must still acquire the bytes:

- Hit in own cache → use cached bytes (verify hash on use).
- Miss → send `FETCH_REQUEST`; sender must serve.
- Sender lost the bytes → operation fails.

This means a hostile app cannot exfiltrate content from another
jail by guessing or learning a hash. Hashes only enable **dedup**
across channels where both ends saw the bytes legitimately.

### 6.3 Cache integrity

Every cached blob is SHA-256-keyed. On every cache hit, before
trusting the bytes, verify `sha256(bytes) == claimed_hash`.
SHA-256 is fast enough that this is essentially free for the
sizes typical of UI payloads (icons, manifests, small images).

For very large blobs where re-hashing is expensive, alternative:
one-time verification on insertion + tamper-resistant cache
allocation (e.g., immutable `MAP_PRIVATE` mapping). Optional;
default is verify-on-use.

### 6.4 The Tessera × aqueduct zero-copy path (optional capability)

Tessera and aqueduct share a hash space. When a service has the
optional `tessera-cas-read` capability granted by Portcullis:

```
(in the jail's mount namespace)
/atrium/cas/            ← read-only nullfs of /var/lib/tessera/cas
```

…the service can answer a hash lookup without going through the
wire:

1. Client sends `clipboard.set(H)` where H is in Tessera CAS.
2. Daemon checks own cache → miss.
3. Daemon issues `TESSERA_PROBE(H)` to hint that Tessera might
   have it. Or: daemon directly opens `/atrium/cas/<hex(H)>`
   and reads. (Implementation detail: Tessera CAS-on-disk
   layout is content-addressed sectors; a userspace helper
   provides the hash → bytes lookup.)
4. On Tessera hit → daemon never asks for upload. Net: 32-byte
   message instead of N-MB stream.

This capability is granted to **system services** (clipboard,
notifier, thumbnailer) by default in Portcullis policy. **Apps
do not get it** — apps must always upload via the wire. This
preserves the "sender-must-serve" isolation property: an app
can't trick a service into reading content the app wasn't
authorized to see.

### 6.5 Jail-to-jail covert channel via shared service

If two apps both talk to the clipboard daemon, they could in
principle communicate via paste/copy operations. This is a
property of any shared system service (true on every desktop
OS). Mitigations live in service policy:

- Per-jail clipboard namespacing in the daemon (Jail A's paste
  isn't visible to Jail B unless user explicitly hand-offs).
- Audit trail in the service for sensitive operations.
- "Privacy mode" capability that opts a jail out of the shared
  service entirely.

Out of scope for aqueduct itself; in scope for clipboard /
notify policy.

### 6.6 Dedup visibility is scoped to proven possession

> Added 2026-06-10 after architecture review. Normative.

The dedup *negotiation* is an existence oracle: if a server
short-circuits an upload ("I already have H — skip the bytes"),
the sender learns that someone else already transferred those
exact bytes. Stated adversarially, "a glyph atlas uploads once
for the whole desktop" reads "client A can detect what client B
is displaying." Same attack family as the Tessera write-path
oracle (tessera-fs.md §20); here it leaks *live* activity rather
than at-rest content.

The rule: **a receiver may exhibit dedup-visible behavior for H
only toward peers whose trust domain has already proven
possession of H.**

- Per-connection, the receiver keeps a *possession ledger*: every
  hash this peer has uploaded (`UPLOAD_FINISH` verified) or been
  legitimately served (`FETCH_BEGIN` completed, or referenced in
  a message the service itself addressed to the peer). Entries
  SHOULD record `(hash, len)` — frescod reuses the ledger as the
  charge set for per-client CAS budgets (fresco-recovery.md §5.1),
  so one structure serves both the oracle rule and accounting.
- Early `UPLOAD_ACK` (acking an `UPLOAD_BEGIN` without requesting
  the bytes) is permitted iff H is in that peer's ledger.
  Otherwise the receiver MUST accept the full upload — even if
  its CAS already holds H — and MUST NOT exhibit
  hash-hit-dependent timing before the ACK.
- Ledgers may be widened from per-connection to a *trust domain*
  (e.g., all clients of one user session) by service policy.
  Within one user's session the oracle is moot; across users or
  across jails of different apps it is not.
- `TESSERA_PROBE` answers and the `tessera-cas-read` zero-copy
  path (§6.4) are already restricted to system services; those
  services MUST NOT relay probe results to app-rank peers.
- Internal storage dedup is unaffected: the CAS stores one copy
  regardless. Only the *observable wire behavior* is scoped.

Cost: the first upload of a popular blob happens once per trust
domain instead of once per system. On local transports this is
noise; on remote transports a session's clients share one domain,
so the steady-state bandwidth story (mutations + per-session
one-time uploads) is intact. This mirrors the industry response
to the 2011 Dropbox cross-user dedup disclosure: keep storage
dedup, drop cross-tenant *client-visible* dedup.

The display dictionary (class 1) inherits this rule when Fresco
migrates onto aqueduct: frescod's per-slot upload negotiation
keeps a per-slot (or per-session) ledger, not a global one.

## 7. Threat model

In scope:

- **Cross-jail content isolation** — sender-must-serve + service-
  is-policy. Apps can't read each other's content via the protocol.
- **Cache poisoning** — verify-on-use + content-addressing.
- **Resource exhaustion (RAM)** — per-client cache budget caps,
  per-connection rate limits, oversized-payload rejection.
- **Capability spoofing** — kernel-enforced via filesystem.
- **Cross-domain existence disclosure via dedup** — added
  2026-06-10: the possession-scoping rule (§6.6) makes dedup
  negotiation non-observable across trust domains. The at-rest
  half of the same oracle is handled by Tessera dedup domains
  (tessera-fs.md §20).

Out of scope:

- **Service compromise** — if the clipboard daemon is exploited,
  it has access to clipboard contents. Standard. Mitigation:
  service-side hardening, capsicum where applicable.
- **Side channels via timing (general)** — beyond the specific
  hash-hit timing covered by §6.6, the substrate can't equalize
  service response timing; services should be careful about
  user-data-dependent timing in responses.
- **Rogue kernel** — aqueduct trusts the FreeBSD kernel.

### 7.1 Authentication is load-bearing; encryption is not (for local transports)

Aqueduct does **not** encrypt its wire on local transports (Unix
domain socket, posix shared memory, ivshmem). The reason is not
"we forgot": encryption defends against an attacker who can read the
wire but cannot read process memory. On a single machine, anyone who
can read the wire bytes (root, kernel-level access, or a process with
the same uid as a daemon) can already read process memory by
definition — encryption keys live in process memory, so encryption
defends against no threat the kernel doesn't already defend against.

This is the same posture every other local-IPC protocol on commodity
OSes takes: D-Bus, Wayland, X11, every Unix socket-based service.
Local IPC is plaintext + kernel-permission-enforced.

Authentication, on the other hand, is essential and is *enforced at
every connection*:

- `SO_PEERCRED` on Unix sockets attests the connecting process's uid/pid
  to the kernel.
- Portcullis's manifest-based gate (`atrium-portcullisd-daemon`)
  cross-checks `peer_uid → manifest` before granting any capability.
- `SCM_RIGHTS` moves capabilities by file descriptor, not by token —
  caps cannot be replayed, guessed, or forged.

These three mechanisms together replace what TLS+mTLS would do over
a network: they prove *who* the peer is, *what* they can ask for, and
*ensure* the capability cannot be reused outside its intended fd.
Encryption would add only protection against an adversary already
positioned to defeat all three.

### 7.2 Remote aqueduct: tunnel, do not embed

Atrium will eventually grow remote aqueduct use cases — Stoa
persistent SSH sessions, thin-client compositor displaying an
Atrium app on a remote server, future cross-machine collaboration
shapes. For all of these, the recommendation is:

- **Tunnel through an existing encrypted transport.** SSH for
  interactive sessions (Stoa already builds on this).
  WireGuard for site-to-site or trusted-fleet shapes. QUIC + TLS
  for thin-client server-to-thin-client. Aqueduct itself stays
  plaintext and transport-agnostic; the wrapping transport carries
  the encryption.

- **Aqueduct's envelope format is already transport-agnostic.**
  Length-prefixed records work on Unix sockets, TCP, QUIC streams,
  ivshmem rings. No design change is needed to support remote use;
  we just don't pay for crypto today.

- **Remote authentication is designed** (2026-06-10):
  [aqueduct-remote.md](aqueduct-remote.md). Local `SO_PEERCRED`
  does not cross a network; remote sessions use the generalized
  Stoa handoff — one-time SSH userauth mints a session credential
  (`K_sess = KDF(ssh_session_id ‖ service_nonce)`) carrying a
  **session capability set**, presented via
  `OP_AQUEDUCT_HANDSHAKE` (class-0 ops 0x09/0x0A). Default mint =
  the user's full session trust domain; restricted mints
  (view-only, single-app) are user-initiated capability
  downgrades. `AuthMethod::CapToken` (portcullisd-signed,
  federation) and `AuthMethod::Mutual` (mTLS mapping) remain
  declared-but-deferred there.

The composition principle is the same one HTTP used: don't embed
TLS in HTTP, run HTTP over TLS. The wrapping layer composes cleanly,
the protocol's hot path stays narrow, and the responsibility for
crypto lives in code already audited for it.

### 7.3 Aqueduct-gpu (the higher-bandwidth path)

`aqueduct-gpu` (see `aqueduct-gpu.md`) carries shader bytecode,
texture data, command streams, and large mappable memory regions.
Its trust model is detailed in `aqueduct-gpu.md` §12: per-jail data
flow, per-fd kmod resource isolation, `gpu.scanout` as a distinct
Portcullis capability for display-server-only operations. The
encryption posture above (no for local, tunnel for remote) applies
identically — the kmod-level isolation and Portcullis cap mediation
are the load-bearing primitives.

## 8. Performance targets

Order-of-magnitude expectations on commodity hardware (real iron,
not the QEMU-on-macOS dev rig):

| Op                              | Target          |
|---------------------------------|-----------------|
| Small RPC round-trip (≤256 B)   | ≤ 50 µs         |
| Bulk upload (1 MiB, no Tessera) | ≥ 500 MB/s      |
| Bulk reference (1 MiB, in CAS)  | ≤ 20 µs         |
| shm fd-pass + mmap setup        | ≤ 100 µs        |
| Cache hit ratio (typical UI)    | ≥ 80%           |

Subject to revision once we have real measurements on D2.5
workloads.

## 9. Implementation plan (D1.6 + Fresco migration)

D1.6 was built greenfield (envelope substrate + classes.rs
constants), grandfathering Fresco's 128-byte fixed-frame format. As
of M1 of the production rollout (`docs/spec/fresco-production-rollout.md`),
Fresco is migrating onto the envelope as a **hard cutover** (no
NEGOTIATE_CAPS coexistence window). The new
[`fresco-protocol`](../../fresco-protocol/) crate publishes the
CLASS_DISPLAY=1 dictionary; frescod and fresco-socket-rs are being
refactored to speak it.

Order of work:

1. **Spec freeze.** This document plus `aqueduct/src/classes.rs`
   enumerating opcode_class constants. Done at D1.6.
2. **Crate scaffold.** `aqueduct` (std for transport; keeps the
   option open for no_std bits later). Defines: envelope codec,
   `Connection` type, CAS upload/fetch state machines, async event
   channel, fd-pass helper. Done at D1.6.
3. **Smoke-test service.** `aqueduct-echo` server + client.
4. **`fresco-protocol` crate** publishes the display dictionary
   (class 1) — control + scene + window-management op families.
   Lands at M1 of the production rollout.
5. **frescod migration.** Dispatcher rewritten to parse envelope
   ops instead of 128-byte frames. fresco-socket-rs becomes a thin
   layer over `aqueduct` + `fresco-protocol`. All clients
   migrate atomically. Lands at M2.
6. **Document patterns** for downstream services in
   `docs/spec/aqueduct-services.md` (template + examples).

The hard-cutover decision (vs the originally-planned coexistence
window via NEGOTIATE_CAPS) was taken because the consumer set is
small (frescod + 7 demo apps) and changes can land atomically; a
compatibility window would carry both code paths longer than needed.

## 10. Open questions

- **Should we standardize a Rust marshalling format?** Postcard
  would be a defensible default (compact, deterministic, no_std).
  Manual binary fits very small per-service ops better. Probably
  start with "postcard for Rust↔Rust, document a hand-rolled
  binary pattern for performance-critical opcodes."
- **Cross-language story** (eventually we want Slint apps in any
  language). The envelope is language-agnostic; the per-class
  schemas would need polyglot specifications. Defer to the
  language is asking for it.
- **Authentication beyond UID/GID.** Currently the service trusts
  any UDS connection from a permitted UID. For multi-user
  scenarios or fine-grained per-app auth, we may need a per-jail
  identity (UUID from atrium.toml?) the service can use for
  policy. Defer to first real use case.
