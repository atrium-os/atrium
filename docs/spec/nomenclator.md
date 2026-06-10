# Nomenclator — name resolution protocol

Status: design sketch (pre-implementation).
Last updated: 2026-05-21.

**Nomenclator** (Latin: *the Roman household servant who
whispered names to his master so he could greet visitors*)
is Insula's name-resolution service. It turns human-readable
names (`weather.example.com`) into signed publisher
manifests, which in turn point at content-addressed
artifacts in Tessera.

This document expands `insula.md` §12 into implementation
detail. Other dependent specs:

- **`insula.md` §12** — the design summary, three-layer
  resolution shape.
- **`insula.md` §14** — updates and publisher key rotation;
  Nomenclator is the resolution authority.
- **`artifex.md`** — Artifex consumes `atrium-app://`
  URIs (the IDE bundles, extensions, registries).

## 0. Position

### 0.1 What Nomenclator is

- The service that turns a human-readable name into a
  *signed publisher manifest*.
- The service that turns a manifest entry into a *content
  hash* in Tessera.
- The cache + verifier sitting between the network (DNS,
  HTTPS-shaped manifest fetch) and the local trust store.
- The publisher-key trust anchor — Nomenclator decides
  whether a freshly-fetched manifest is signed by a key
  the user (or platform) currently trusts for that
  publisher.

### 0.2 What Nomenclator is not

- **Not DNS.** Nomenclator uses DNS as one of its
  primitives but does not replace it. Replacing DNS is a
  separate, much larger problem.
- **Not a content store.** Tessera is the content store.
  Nomenclator points at content; Tessera holds it.
- **Not an identity provider.** Vestibulum + the keychain
  hold identity. Nomenclator verifies *publisher*
  signatures on manifests; it does not authenticate users.
- **Not a search engine.** Nomenclator resolves *known*
  names. Discovery / "find me articles about X" is a
  search app, not a resolver.

## 1. Architecture

```
App                       Nomenclator (libatrium + nomenclatord)
  │                            │
  │  resolve("atrium-doc://example.com/weather")
  ├───────────────────────────►│
  │                            │  1. DNS query (TXT record)
  │                            ├──────► DNS resolver
  │                            │
  │                            │  2. Fetch publisher manifest URL
  │                            ├──────► HTTPS / signed fetch
  │                            │
  │                            │  3. Verify signature against publisher key
  │                            │     (look up in local key store)
  │                            │
  │                            │  4. Look up "weather" in manifest
  │                            │     → atrium-doc://<hash>
  │                            │
  │                            │  5. Resolve hash via Tessera
  │                            ├──────► tessera-fs
  │                            │
  │  result: (hash, manifest meta, mime, …)
  │◄───────────────────────────┤
  │                            │
  │  fetch bytes from Tessera CAS (separate operation)
  │
```

`nomenclatord` is a system daemon. It is in the TCB
(`insula.md` §24.4) because malicious-signature acceptance
or stale-cache leak compromises the addressing layer.
Surface is small: DNS lookups, signed-manifest fetch,
signature verification, in-memory + Tessera-backed cache.

`libatrium`'s `atrium_nomenclator_resolve()` is the
client-side wrapper that apps actually call.

## 2. Three-layer resolution

Layer responsibilities, restated concretely:

| Layer | Responsibility | Backed by |
|---|---|---|
| 1 — Name | Map `example.com` → publisher manifest URL + signing key fingerprint | DNS TXT record |
| 2 — Manifest | Map (publisher, path) → content hash | Signed CBOR manifest, fetched + cached |
| 3 — Content | Map content hash → bytes | Tessera (local cache + any compatible CAS source) |

Each layer's failure modes are independent (DNS broken,
manifest stale, content missing) — Nomenclator surfaces
distinct error codes for each.

## 3. Layer 1 — DNS

### 3.1 TXT record format

Publishers add a TXT record at `_atrium.<name>` (or at
the apex `<name>`; both supported):

```
_atrium.example.com. IN TXT
  "atrium=1; manifest=https://example.com/.well-known/atrium-manifest;
   key=ed25519:base64...; key-id=2026-q2"
```

Fields:

| Field | Required | Meaning |
|---|---|---|
| `atrium` | yes | Protocol version (currently `1`). |
| `manifest` | yes | HTTPS URL of the signed publisher manifest. |
| `key` | yes | Publisher's current signing public key. |
| `key-id` | yes | Short identifier for this key; survives key rotation history. |
| `alt-key` | no | Successor key during a rotation window (key being introduced). |
| `prev-key` | no | Predecessor key during rotation (key being retired). |

### 3.2 Resolution

1. Query `_atrium.<name>` TXT records.
2. If absent, query `<name>` TXT records for an `atrium=1`
   entry (apex fallback).
3. If absent, fail with `ENONAME`.
4. Parse fields; verify protocol version is supported.

### 3.3 DNSSEC

Nomenclator *uses* DNSSEC when available but does not
require it. The integrity layer is the publisher's
manifest signature, not DNS. DNSSEC's value is preventing
an attacker from injecting a fake `_atrium` TXT pointing
at *their* manifest URL with *their* key fingerprint;
without DNSSEC, the attacker can attempt this and the
user's first encounter with the publisher locks in the
malicious key.

### 3.4 TOFU vs. published keys

Subsequent encounters are well-protected: §6.2's
chained-attestation property means an attacker who later
injects a fake TXT record cannot rotate a publisher's key
(no prior valid manifest endorsed the attacker's key). The
**only** genuinely exposed moment is the *first* encounter —
and this section tightens it rather than hand-waving "TOFU"
across all of it.

First-encounter trust is established by the strongest
available basis, in order, and the resolver **records which
basis was used** in the pin entry (so a later weaker
encounter can never silently downgrade it):

1. **Pre-seeded key** (§9) — no TOFU at all. Platform-default
   for the OS vendor; user/org-configured for high-stakes
   publishers (banks, identity providers).
2. **DNSSEC-validated first-use** — when the `_atrium` TXT
   record is covered by a valid DNSSEC chain, the
   first-seen key is accepted as DNSSEC-anchored, not bare
   TOFU. The attacker can no longer forge the first TXT
   response (§3.3). Resolvers SHOULD prefer this and MAY be
   configured (per §13 federation policy) to *require* it
   for any publisher carrying app-install or payment
   capabilities.
3. **Bare TOFU** — only when neither of the above is
   available. The first-seen key is pinned, the entry is
   flagged `trust = "tofu-bare"`, and the basis is visible
   to the user (a "this publisher's identity is unverified"
   affordance in Forum/Curia, like a browser's
   not-secure indicator).

**Federation guidance.** In a federated/decentralized
distribution context (the registry's peer/DHT tiers,
atrium-pkg-registry.md), bare TOFU is the weak link, not the
rotation flow. The recommended posture: app/content
publishers that can mint capabilities ship a pre-seeded or
DNSSEC-anchored key; the resolver's federation policy refuses
to *install* (as opposed to merely resolve) from a
`tofu-bare` publisher unless the user explicitly accepts the
unverified-identity prompt. This keeps TOFU usable for
low-stakes name resolution while denying it as a silent path
to capability-bearing installs.

Pre-seeding is described in §9.

### 3.5 Replacing DNS

DNS is centralized, politically-vulnerable, and
seizable. Nomenclator inherits these problems but
deliberately scopes them out — replacing DNS is a
separate, decade-scale project. Two future paths:

- **Out-of-band publisher manifests.** A user can
  manually configure a publisher entry, bypassing DNS
  entirely; useful for content distributed via USB,
  airdrop, or sneakernet.
- **DHT-based name layer.** Replace DNS lookups with a
  content-addressed P2P name layer (IPNS-shape, but
  re-designed). v3+ work.

Neither replaces DNS for v1; both are layers Nomenclator
could grow.

## 4. Layer 2 — publisher manifest

### 4.1 Schema (CBOR via CDDL)

```cddl
publisher-manifest = {
  ; Required header
  "v"        : 1,                       ; protocol version
  "publisher": tstr,                    ; canonical name
  "key-id"   : tstr,                    ; matches DNS key-id
  "signed-at": uint,                    ; unix seconds
  "expires-at": uint,                   ; unix seconds

  ; Content table — path -> content reference
  "content"  : { * tstr => content-ref },

  ; Archival entries — path -> historical hashes
  ? "archive": { * tstr => [ * content-ref ] },

  ; Capabilities the publisher claims this manifest delivers
  ? "claims" : { * tstr => any },

  ; Key rotation (see §6)
  ? "rotation": rotation-entry,
}

content-ref = {
  "hash"   : bstr,            ; tessera content hash, raw bytes
  ? "mime" : tstr,
  ? "size" : uint,
  ? "lang" : tstr,
}

rotation-entry = {
  "alt-key": bstr,            ; successor public key
  "alt-key-id": tstr,
  "until": uint,              ; unix seconds — rotation window end
}

; The whole manifest is wrapped in a COSE_Sign1 envelope
; with the publisher's current key.
```

### 4.2 Lookup

```
input: manifest, path, optional version specifier
1. Look up `path` in manifest.content.
   - If present: result = content-ref.
   - If absent and version specifier asks for archive:
     look up in manifest.archive; return matching version.
   - Otherwise: ENOENT.
2. Return (hash, mime, size, lang, ...).
```

### 4.3 Validity

A manifest is valid if all of:
- COSE signature verifies against the publisher's
  current key (or alt-key during rotation window).
- `signed-at` is in the past (allow small clock skew).
- `expires-at` is in the future.
- `key-id` matches the DNS TXT `key-id` (or matches
  `alt-key-id` if rotation is active).

Failing any → manifest is rejected; cached invalid
manifests are evicted; the resolver returns
`EMANIFEST_INVALID` with a sub-code.

### 4.4 Caching

Manifests are cached:

- **In memory** for the warm path. Configurable LRU; 1 MB
  default budget.
- **In Tessera** (each manifest is itself
  content-addressed). The Tessera CAS layer dedups
  manifests across publishers.
- Cache entries respect `expires-at`. Re-fetch on
  miss; on fetch-error past expiry, serve stale with a
  warning flag (see §7 failure modes).

The manifest URL itself can be a Tessera CAS reference
(`atrium-cas://<hash>`) — useful for fully-content-
addressed publishers — or HTTPS.

### 4.5 Freshness policy

Per-resolution, the caller can request:

- `FRESH` — always fetch; do not use cached.
- `RECENT` — fetch if cached is older than threshold
  (default 5 min).
- `CACHED` — use cached if valid (default).

Most app launches use `CACHED`. Security-sensitive
operations (sign-in, payment) use `RECENT` or `FRESH`.

## 5. Layer 3 — content via Tessera

Once the manifest yields a content hash, the resolver
delegates to Tessera:

```c
tessera_open_by_hash(hash, &fd_or_handle);
```

If Tessera has the bytes locally, the operation is
immediate (mmap-class). If not, Tessera fetches from any
configured upstream — publisher CDN, peer Atrium device,
content-addressed P2P — verifying the hash on receipt.

**This is the win** (`insula.md` §12.5): because the
address is the hash, the bytes can come from anywhere.
HTTPS-as-trust-anchor is not needed at the content
layer; the manifest's signature is the trust anchor.

## 6. Publisher key rotation

### 6.1 The rotation window

A publisher rotates from `key-A` to `key-B`:

1. **Pre-announcement** — publisher publishes a manifest
   signed with `key-A`, containing `rotation = { alt-key
   = key-B, until = T }`.
2. **DNS update** — TXT record updates: `key=key-A;
   alt-key=key-B`.
3. **Rollover** — at some point before `T`, publisher
   signs the next manifest with `key-B` and updates DNS:
   `key=key-B; prev-key=key-A`.
4. **Retirement** — after a grace period, DNS drops
   `prev-key`.

During the window, clients accept manifests signed by
either key. The rotation is **atomic from each client's
perspective**: a client fetching during step 1 sees
`key-A` signature; during step 3 sees `key-B` signature;
verifies either against the corresponding TXT field.

### 6.2 Resolver behavior across rotation

The resolver accepts a manifest as valid if:
- DNS says `key=K1` and manifest is signed by `K1`, OR
- DNS says `alt-key=K2` and manifest is signed by `K2`,
  AND `K2` was offered as `alt-key` in a previous
  manifest signed by `K1`.

The second condition is the **chained-attestation**
property: an attacker injecting a fresh DNS record with
their own `alt-key` cannot rotate the publisher because
no previous valid manifest endorsed that alt-key.

### 6.3 Key pinning

For high-stakes publishers, the platform / org / user
can **pin** a specific key. Pinned keys cannot rotate
without explicit user consent:

- A pinned publisher's resolver requires the pinned key
  to sign manifests.
- Rotation requires the user to approve the new key out-
  of-band (Forum prompt: "publisher X is rotating
  signing keys; confirm new key fingerprint Y?").

Pinning is opt-in per publisher; platform-default for
the OS vendor's own publishers and any provider the user
explicitly designates as high-stakes.

## 7. Failure modes and recovery

| Failure | Code | Recovery |
|---|---|---|
| DNS lookup failed | `ENOLOOKUP` | Retry with backoff; offline mode if cached manifest exists |
| No `_atrium` TXT | `ENONAME` | Surface to user — "this name is not an Atrium publisher" |
| Manifest fetch failed | `EFETCH` | Try alternate fetch paths (CAS, peer); serve cached if available with warning |
| Manifest signature invalid | `EMANIFEST_SIG` | Reject; surface to user; do not cache the bad manifest |
| Manifest expired | `EMANIFEST_EXPIRED` | Try fresh fetch; if still expired, serve with `STALE` flag |
| Key mismatch (DNS says K1, manifest signed by K2) | `EMANIFEST_KEY` | Check rotation chain; if not chained, reject |
| Content hash not in any CAS | `ECONTENT` | Surface to user — "publisher referenced content not available" |
| Publisher disappeared (TXT gone) | `ENOPUBLISHER` | Serve cached manifest with `PUBLISHER_GONE` flag; allow content access (archival case) |

The `PUBLISHER_GONE` flag is the load-bearing one — it
preserves the §0.5/§12.6 property that content survives
the publisher.

## 8. Privacy

### 8.1 What Nomenclator reveals to whom

- **DNS resolver sees:** which publishers the user is
  querying (`_atrium.example.com` lookups). Same
  privacy properties as any DNS use.
- **Publisher's manifest server sees:** which clients
  fetch the manifest and when. Standard HTTP-fetch
  privacy. Encrypted DNS (DoH/DoT) and aggregated
  fetch proxies help.
- **Content CDN sees:** content hashes fetched. Content-
  addressed → cannot tell *what* content (the hash
  doesn't reveal anything about the bytes), but
  *when* and *which hash*.

### 8.2 Mitigations

- **Manifest pre-fetch over Tor / mixnet.** Publishers
  can offer their manifest URL over a privacy-preserving
  transport in addition to plain HTTPS.
- **Tessera P2P** — once content is in any local peer
  on the network, fetching from that peer reveals
  nothing to the publisher's CDN.
- **Manifest aggregation** — opt-in: the platform
  fetches a batch of popular publisher manifests so
  individual lookups are noise.

### 8.3 What Nomenclator does NOT do

- Encrypt content (publisher's choice).
- Anonymize fetches (transport's responsibility).
- Replace DNS privacy (DoH/DoT/DoQ exist; use them).

## 9. Bootstrap and pre-seeded keys

### 9.1 Out-of-band publisher onboarding

A user can add a publisher entry manually, bypassing
DNS:

```
$ atrium-nomenclator add-publisher \
    example.com \
    --manifest atrium-cas://7f3a... \
    --key ed25519:... \
    --key-id 2026-q2
```

Useful for: pre-release platforms, air-gapped networks,
sneakernet content distribution.

### 9.2 Platform-default pre-seeded keys

The platform ships with pre-seeded entries for:
- The OS vendor's own publisher (Atrium core, foundation
  apps).
- A configurable "trusted publisher" list (Opifex
  registry, platform marketplace).

Users can edit this list; the defaults are conservative.

### 9.3 Bundle-shipped entries

An Insula bundle can include a pre-seed entry for
publishers it depends on (e.g., an extension's manifest
declares "I depend on `dep.example.com` with key K").
The user reviews this at install time and decides
whether to trust the bundled entry.

## 10. API

### 10.1 C ABI (`libatrium_nomenclator.h`)

```c
typedef struct atrium_nomenclator_t atrium_nomenclator_t;

typedef enum {
    NOMEN_FRESHNESS_CACHED,
    NOMEN_FRESHNESS_RECENT,    // refetch if older than 5 min
    NOMEN_FRESHNESS_FRESH,
} atrium_nomenclator_freshness_t;

typedef struct {
    atrium_nomenclator_freshness_t freshness;
    bool allow_stale_on_failure;       // serve stale with flag if fetch fails
    bool require_pinned_key;
} atrium_nomenclator_opts_t;

typedef struct {
    uint8_t hash[32];          // tessera content hash
    char    mime[64];
    size_t  size;
    char    lang[16];
    uint32_t flags;            // STALE | PUBLISHER_GONE | ...
    uint64_t manifest_signed_at;
    uint64_t manifest_expires_at;
} atrium_nomenclator_result_t;

int atrium_nomenclator_resolve(
    const char* uri,           // atrium-doc://... or atrium-app://...
    const atrium_nomenclator_opts_t* opts,
    atrium_nomenclator_result_t* out_result,
    char* out_error_msg, size_t error_msg_len);

int atrium_nomenclator_add_publisher(
    const char* name,
    const char* manifest_uri,
    const uint8_t* key, size_t key_len,
    const char* key_id);

int atrium_nomenclator_pin_publisher(const char* name);
int atrium_nomenclator_unpin_publisher(const char* name);
```

### 10.2 Rust SDK

Idiomatic wrapper:

```rust
let result = nomenclator::resolve("atrium-doc://example.com/weather")
    .freshness(Freshness::Recent)
    .await?;
let bytes = tessera::open(result.hash).await?;
```

## 11. Performance targets

| Operation | Cached | Cold (DNS + fetch + verify) |
|---|---|---|
| Resolve name to hash | ~50 µs | ~50–200 ms (network-bound) |
| Verify manifest signature | ~100 µs (ed25519) | (included above) |
| Hash lookup in Tessera | ~10 µs local mmap | tessera-driven for remote |

The cached path dominates real-world usage: an app the
user has seen before, content they have seen before, no
network round-trip. Cold lookups are bounded by DNS +
HTTPS latency, which Nomenclator does not control.

## 12. Bring-up phases

### 12.1 Phase A — single-publisher MVP

- `nomenclatord` daemon stub.
- DNS TXT lookup via system resolver.
- Manifest fetch over HTTPS.
- Ed25519 signature verification.
- In-memory LRU manifest cache.
- One resolved URI scheme: `atrium-doc://`.

Goal: end-to-end resolution from a typed URL to bytes
for the simplest case.

### 12.2 Phase B — full content/app resolution

- `atrium-app://` resolution against installed-app
  registry + Opifex.
- Archival lookups (`?at=2026-05-20`).
- Tessera-backed manifest cache (cross-publisher dedup).
- Key rotation chain verification.

### 12.3 Phase C — trust and key management

- Out-of-band publisher onboarding (`atrium-nomenclator
  add-publisher`).
- Platform-default pre-seeded keys for OS vendor +
  configurable trusted list.
- Bundle-shipped publisher entries (review at install).
- Pinning UI in Forum.

### 12.4 Phase D — privacy and resilience

- Privacy-preserving manifest fetch (DoH/Tor optional
  paths).
- Manifest aggregation prefetch.
- `PUBLISHER_GONE` flag handling end-to-end with the
  archival viewer experience.
- DHT-based name layer (v3 — out of scope here).

## 13. Open questions

- **CDDL choice.** Schemas in CDDL; tooling for Rust
  validation is decent but not universal. Alternative:
  hand-written CBOR with documented field layout.
- **Manifest size limits.** A publisher with many paths
  could ship a multi-MB manifest. Should manifests be
  split / chunked / Merkle-tree-shaped for large
  catalogues? Likely v2.
- **TOFU policy strictness.** §3.4 now grades first-use by
  basis (pre-seeded → DNSSEC-anchored → bare TOFU) and
  denies `tofu-bare` as a silent path to capability-bearing
  installs. Remaining open: whether to add a
  transparency-log-style key publication / CT-analog so even
  bare-TOFU publishers gain after-the-fact auditability.
  Probably v2; the registry's Sigstore/Rekor use
  (atrium-pkg-registry.md) may cover the app-install case
  without a Nomenclator-specific log.
- **Multiple manifest sources.** A publisher could
  publish their manifest URL in multiple places (DNS,
  bundle, OOB). The resolver currently picks DNS first;
  policy for handling conflicts TBD.
- **Search interaction.** A search app produces
  `atrium-doc://` URIs as results. The handoff from
  search to Nomenclator is clean (just resolve), but
  how does search *populate* its index? Likely outside
  this spec.
- **CAS-only publishers.** Some publishers might want
  to skip HTTPS entirely, publishing only via Tessera
  CAS. Workable but bootstrap requires a sidechannel
  (the initial hash must come from somewhere). May
  emerge in v3.

## 14. References

- `docs/spec/insula.md` — parent spec; §12 is the design
  summary, §14.6 is the key-rotation context.
- `docs/spec/tessera-fs.md` — content layer.
- `docs/spec/aqueduct.md` — IPC for the resolver-daemon
  connection.
- `docs/NAMING.md` — naming reference (Nomenclator entry).
- Related future specs:
  - `docs/spec/opifex-registry.md` (or current
    `atrium-pkg-registry.md`) — how registries publish
    using the Nomenclator pattern.
