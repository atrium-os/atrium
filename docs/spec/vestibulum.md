# Vestibulum — login, session, identity, keychain

Status: design sketch.
Last updated: 2026-05-21.

**Vestibulum** (Latin: *entry hall*) is Atrium's
authentication and identity service. It owns the user's
trust anchor for everything the platform does — login at
boot, session lifecycle, per-service keypairs, biometric /
passcode prompts, recovery flows.

The original NAMING.md entry described Vestibulum as
"display manager + login screen + session handoff." This
spec preserves that role and extends it with the identity
work that Insula depends on (`insula.md` §13).

## 0. Position

### 0.1 What Vestibulum is

- The **trust anchor** for the user's identity on the
  device. Owning Vestibulum's keys = owning the user's
  Atrium identity.
- The **login screen** at boot and at session unlock.
- The **session supervisor** that launches the user's
  Forum (shell) after authentication.
- The **keychain** holding per-service keypairs (Insula
  apps' identity material), the per-app push keypairs
  (Tabellarius), the master key wrapping Loculus, and
  the device identity material used by Concursus +
  Aqueduct network handshakes.
- The **sign-in UI host** — when an Insula app requests
  sign-in to a service (via Limen's `sign-in` role),
  Vestibulum renders the UI outside any app's jail so
  credentials never cross the boundary.

### 0.2 What Vestibulum is not

- **Not an identity provider in the network sense.** It
  authenticates the user to *this device*. Federated
  sign-in to a remote service uses Vestibulum's
  per-service keys but the remote service is the
  identity authority for its own users.
- **Not a password manager UI.** Loculus is the user-
  visible "your saved stuff" UI; Vestibulum is the
  invisible-most-of-the-time machinery.
- **Not the WM / dock.** Forum is the shell. Vestibulum
  hands off to Forum after authentication and is
  invisible until session unlock / sign-out.

### 0.3 Two halves

Vestibulum has two distinct surfaces that share a
trust core:

1. **Display manager + session supervisor.** The boot-
   time login screen, session-unlock prompt, sign-out
   handler. User-visible mostly at session boundaries.
2. **Keychain + identity service.** Always-running
   daemon serving keychain + sign-in requests from
   Insula apps and other Atrium services. User-visible
   only when prompted for biometric / passcode.

Both run in the same process (`vestibulumd`) for
simplicity; their APIs are distinct.

## 1. Architecture

```
At boot:
    init (rc.d) starts vestibulumd
        │
        ▼
    Display manager surface:
        - Locks the display until user authenticates
        - Renders login UI via Pergola (system-trusted surface)
        - On successful auth: unlocks the device keychain,
          spawns Forum as the user
        - Hands display surface to Forum

After login:
    Forum + apps running, vestibulumd in background

When an Insula app requests sign-in:
    App → Limen request_embed("sign-in", role-specific data)
    Limen → vestibulumd
    Vestibulum renders sign-in UI in the slot
        (Pergola surface owned by Vestibulum, not the app)
    User completes flow
    Vestibulum returns typed result to the app

When an Insula app requests a keychain operation:
    App → libatrium_keychain.h API
    libatrium → vestibulumd over Aqueduct
    vestibulumd checks capability, may prompt biometric
    Returns result (sign-this, mint-keypair, decrypt-blob, …)
```

`vestibulumd` is in the TCB (`insula.md` §24.4 lists
"Vestibulum" implicitly via "keychain, sign-in UI"). Its
surface is small but the consequences of compromise are
broad — every per-service identity flows through it.

## 2. Login and session lifecycle

### 2.1 Device unlock at boot

1. Kernel boots; init launches vestibulumd.
2. vestibulumd locks the display surface via Fresco.
3. Reads device-encryption metadata.
4. Renders login UI (passcode / biometric / recovery
   key prompt).
5. On success:
   - Unwraps the device keychain master key.
   - Spawns Forum as the authenticated user.
   - Hands display surface ownership to Forum.
   - Continues running in background.
6. On failure: rate-limited retry; lockout after N
   failures; recovery-key fallback.

### 2.2 Session unlock (lock screen)

When the user locks the device (or it auto-locks):
- Vestibulum re-takes display surface ownership from
  Forum.
- Apps continue running in jails (unless OS power-saver
  suspends them); their display output is blocked at
  the compositor.
- On unlock: re-verify via passcode / biometric; return
  display ownership to Forum.

This is iOS-style lock semantics — apps don't die when
the screen locks; they just become invisible. State
preservation across lock is automatic (the jail keeps
running, modulo power policy).

### 2.3 Sign-out

User invokes sign-out from Forum:
- All user-session jails get SIGTERM.
- Vestibulum re-encrypts and zeroes the keychain master
  key from memory.
- Display returns to login screen.

### 2.4 Multi-user

A device may have multiple users. Each user has their
own:
- Login credentials (passcode / biometric / recovery
  key).
- Encrypted keychain database.
- Persona / Loculus / Tessera namespace.

Vestibulum switches between users at the login screen.
Concurrent sessions for multiple users (fast user
switching) is a v2 concern.

## 3. The keychain

### 3.1 Structure

```
~/Library/Atrium/vestibulum/<user>/keychain/
   master.wrapped     ; master key wrapped by user-credential-derived KEK
   items.db           ; encrypted SQLite + content-addressed blob refs
   recovery.bundle    ; recovery-key-wrapped backup metadata
```

Each item is encrypted with a per-item key wrapped by
the master. Items have:

- **Owner** — which service or app owns this item.
- **Type** — `per-service-keypair`,
  `push-keypair`,
  `loculus-master-key`,
  `device-identity`,
  `service-cookie`, …
- **Access policy** — when can this be used (always /
  user-presence / biometric-required).
- **Payload** — the encrypted bytes.

### 3.2 Per-service keypair (Insula apps)

Per `insula.md` §13.3: each (persona, service) pair
gets a fresh ed25519 keypair on first sign-in. The
*private key never leaves Vestibulum*. Apps go through
Vestibulum for sign/decrypt operations.

```
service: "com.example.weather"
persona: "personal"
type:    "per-service-keypair"
algo:    "ed25519"
access:  { presence: false, biometric: false }   ; quiet use after initial setup
payload: { sk: <encrypted>, pk: <plaintext> }
```

### 3.3 Per-app push keypair (Tabellarius)

Per `tabellarius.md` §2: each app that subscribes to
pushes mints an X25519 keypair. The private key is
used by the app (via a Vestibulum capability scoped
to "decrypt for app X") to decrypt push payloads.

```
service: "com.example.chat"
persona: "personal"
type:    "push-keypair"
algo:    "x25519"
access:  { presence: false }
payload: { sk: <encrypted>, pk: <plaintext>,
           key-id: "primary" }
```

### 3.4 Loculus master key

Per `loculus.md` §3.1: Loculus encrypts its items
with a master key derived from Vestibulum. The master
key is a keychain item:

```
service: "atrium.loculus"
persona: "personal"
type:    "loculus-master-key"
algo:    "aes-256"
access:  { presence: true }   ; user-presence on use
payload: <encrypted>
```

Loculus calls Vestibulum to unwrap the master at
service start; the unwrapped master lives in Loculus's
memory and is zeroed on shutdown.

### 3.5 Device identity (Concursus, Aqueduct network)

The device's long-lived identity key, used for:
- Peer-channel handshake authenticity (`concursus.md`
  §3.3).
- Aqueduct-over-network mutual auth (`insula.md`
  §20.2).
- Push-relay device cert (Tabellarius).

```
service: "atrium.device"
persona: <device-wide, not per-persona>
type:    "device-identity"
algo:    "ed25519"
access:  { presence: false }
payload: { sk, pk, fingerprint }
```

## 4. The sign-in UI

### 4.1 Why Vestibulum-rendered

The fundamental property: **apps never see user
credentials.** When a user signs into a service from an
app, the credential-entry happens in a UI owned by
Vestibulum, *outside the app's jail*, rendered into a
Limen slot that the app cannot read pixels from.

This is the iOS Touch-ID-prompt model done correctly:
the prompt is *not* part of the app; the app *cannot*
fake it; the user can trust what they see because the
chrome around it identifies the system rather than the
app.

### 4.2 Sign-in flows

Three kinds:

#### 4.2.1 First sign-in to a service (per-service keypair mint)

1. App requests `sign-in` Limen embed for service S.
2. Vestibulum renders a UI:
   - "App A wants to sign you in to service S."
   - Persona selector (Personal / Work / …).
   - Biometric / passcode confirmation.
3. Vestibulum mints a fresh keypair for (persona, S).
4. Registers public key with S (via app's network
   broker request — the actual HTTPS call goes through
   the app).
5. Returns success + per-service identifier to app.

#### 4.2.2 Subsequent sign-in (challenge-response)

1. App requests `sign-in` Limen embed; tells
   Vestibulum which service.
2. Vestibulum looks up the existing keypair; may
   prompt biometric for high-stakes sessions.
3. Performs the challenge-response with the service
   (via Vestibulum, not the app).
4. Returns a session token to the app.

#### 4.2.3 Federated sign-in (unlinkable pseudonyms)

Per `insula.md` §13.4:

1. App A requests sign-in via federator F.
2. Vestibulum renders: "Sign in to App A with F? F
   will see: <claims>."
3. User authorizes.
4. F issues a signed claim about the user, scoped to A.
5. Crucially: the identifier F gives A is a
   *per-relying-party pseudonym* (BBS+ / deterministic
   hash). A gets a stable handle but cannot link the
   user across services.

### 4.3 Sign-in UI surface

The UI is a Pergola surface owned by Vestibulum,
embedded via Limen into the requesting app's window.
Rendering happens in Vestibulum's process; the app
never sees the pixels (per `limen.md` §5.1's pixel-
readback prohibition).

The chrome around the surface is system-identifiable:
distinct visual treatment (e.g., a system-colored
border), a non-spoofable "this is a system prompt"
indicator.

## 5. Capabilities and access policies

### 5.1 The capability shape

Each keychain item declares an access policy:

```
access: {
  presence:    bool,    ; user-presence (touch screen, key press)
  biometric:   bool,    ; biometric / passcode confirmation
  re-auth-after: duration, ; require re-auth after N minutes
  origin:      [string], ; which jails may request this item
}
```

A keychain operation request specifies the item; the
daemon checks the policy + the requesting jail's
manifest + (possibly) prompts the user via Vestibulum.

### 5.2 Per-app scoped capabilities

An app's manifest can declare `[capabilities.keychain]`:

```toml
[capabilities.keychain]
service-keypairs = ["com.example.weather"]
push-keypair = true
loculus-master = false
device-identity = false
```

The daemon enforces: an app requesting an operation
not declared in its manifest gets `EACCESS`.

### 5.3 The "use" capability vs. the "read" capability

Vestibulum supports **use** (sign / decrypt via the
key) without **read** (extract the raw key bytes).

For per-service keypairs and push keypairs, only
"use" is granted to apps — they can ask Vestibulum to
sign / decrypt on their behalf but cannot extract the
raw key. This means a fully-compromised app still
cannot steal the key — it can only ask Vestibulum to
do operations within its capability scope.

## 6. Recovery and backup

### 6.1 Recovery key

At first device setup, Vestibulum generates a recovery
key (24-word BIP-39-style mnemonic, or a printable
40-character key, depending on user preference). The
user is required to record it.

The recovery key wraps a backup version of the master
key. If all credential authenticators (passcode,
biometric) are lost, the recovery key restores access.

### 6.2 Backup encryption

The keychain database is included in device backups
(per `insula.md` §13.7). The backup is encrypted
specifically with a recovery-derived key, not with the
user's passcode (the passcode is device-local;
recovery-key encryption is portable).

### 6.3 Cross-device sync

Opt-in. Per `insula.md` §13.6, devices paired with
the same Vestibulum identity can sync the keychain.
Sync is end-to-end encrypted with a per-pair key
derived during pairing (`concursus.md` §8 pairing flow).

No cloud account required; the user can sync via:
- Paired-device direct.
- A user-chosen cloud relay (encrypted blobs only).
- Manual export / import (sneakernet).

## 7. API

### 7.1 Login-flow API (used by Forum + lock-screen UI)

Internal to vestibulumd and its display-manager
component. Not exposed to apps.

### 7.2 Keychain API (used by Insula apps via libatrium)

```c
typedef struct {
    char service[128];
    char persona[64];
    char type[32];        /* "per-service-keypair", … */
} atrium_keychain_item_ref_t;

int atrium_keychain_mint(
    const atrium_keychain_item_ref_t* ref,
    const char* algo,
    /* access policy */
    bool require_biometric,
    /* out */
    uint8_t* pub_key, size_t pub_key_cap,
    char* key_id, size_t key_id_cap);

int atrium_keychain_sign(
    const atrium_keychain_item_ref_t* ref,
    const uint8_t* challenge, size_t challenge_len,
    uint8_t* signature, size_t* signature_len);

int atrium_keychain_decrypt(
    const atrium_keychain_item_ref_t* ref,
    const uint8_t* ciphertext, size_t ct_len,
    uint8_t* plaintext, size_t* pt_len);

int atrium_keychain_unwrap(
    const atrium_keychain_item_ref_t* ref,
    uint8_t* unwrapped, size_t* unwrapped_len);
/* For Loculus master-key-style items where the
   unwrapped form is needed in the caller's memory. */
```

All these calls go via Aqueduct to vestibulumd; the
daemon checks the requesting jail's manifest, may
prompt biometric, and returns the result. The private
key never crosses the Aqueduct boundary for `sign` /
`decrypt`.

### 7.3 Sign-in flow API

Apps don't invoke sign-in directly; they request the
`sign-in` Limen role embed. Vestibulum implements the
role.

```c
/* Initiated via libatrium_limen.h, role="sign-in" */
/* Messages on the role channel:
       parent → child:  { service, hints? }
       child → parent:  { result: { token, expires-at, … } }
                        or { cancelled }
                        or { error, code } */
```

## 8. Performance and resource

| Metric | Target |
|---|---|
| Login → desktop visible | <2 s on warm boot |
| Lock screen → unlock | <500 ms |
| Keychain `sign` round-trip | <5 ms (no biometric) |
| Keychain `sign` round-trip | <500 ms (biometric prompt) |
| Idle daemon RAM | <24 MB |
| Sign-in UI cold launch | <100 ms |

## 9. Bring-up phases

### 9.1 Phase A — login + minimum keychain

- Login screen + display lock.
- Local keychain DB (encrypted at rest).
- Per-service keypair mint + sign for one sample
  service.
- Sign-in Limen role end-to-end.
- LocalAuthentication / biometric prompt (on macOS host
  adapter: TouchID / passcode).

Goal: an Insula app can sign in to a service with a
fresh per-service keypair, signed by Vestibulum, with
biometric confirmation when high-stakes.

### 9.2 Phase B — full identity surface

- Push keypair, Loculus master key, device identity
  item types.
- Multi-persona switching (in Forum).
- Recovery key generation + verification at setup.
- Backup encryption.

### 9.3 Phase C — federated sign-in

- BBS+ / unlinkable-pseudonym primitive integration.
- Federated provider protocol (Vestibulum as the
  attestor between user persona and per-relying-party
  pseudonym).
- Federation UI flow ("App A wants to sign you in
  with F …").

### 9.4 Phase D — sync and multi-device

- Paired-device direct sync via Concursus.
- Cloud-relay sync (encrypted blobs).
- Manual export / import.

### 9.5 Phase E — production polish

- Multi-user support.
- Fast user switching.
- Concurrent sessions.
- Threat-model hardening (lockout policies,
  brute-force resistance, side-channel mitigations on
  passcode entry).

## 10. Open questions

- **Biometric primitive abstraction.** macOS provides
  LocalAuthentication; FreeBSD-on-Atrium provides
  what? Atrium native biometric primitives are open
  for design (and likely depend on the device's
  hardware — TPM-equivalent, secure element).
- **BBS+ vs. simpler unlinkable scheme.** BBS+ is
  cryptographically powerful but tooling is sparse.
  Deterministic per-relying-party hash is simpler but
  weaker (a colluding federator + relying party can
  link). Decision pending real evaluation.
- **Recovery-key UX.** 24-word mnemonics are familiar
  to crypto users and alien to most others. A short
  printable key + a "save to cloud" option might be
  more practical. Tradeoff: cloud save weakens the
  recovery-from-everything story.
- **Multi-user concurrent sessions.** Linux X11 had
  it via per-user X servers; macOS doesn't really.
  Atrium-native multi-user is plausible (per-user
  jails are natural) but the UX is unclear. Defer to
  v2+.
- **Federation revocation.** A federated provider may
  want to revoke claims. Vestibulum needs to know how
  to disclose this to apps that previously accepted
  the claim. Likely tied to short-lived assertions
  with refresh, not long-lived tokens.

## 11. References

- `docs/spec/insula.md` — parent for the identity
  story; §13 is the design summary.
- `docs/spec/loculus.md` — wallet using Vestibulum
  master-key for encryption.
- `docs/spec/tabellarius.md` — push using Vestibulum
  for per-app keypair custody + decryption.
- `docs/spec/concursus.md` — peer channels using
  Vestibulum device-identity for handshake.
- `docs/spec/limen.md` — sign-in role; Vestibulum is
  the implementer.
- `docs/spec/portcullis.md` — jail launcher;
  Vestibulum hands off to Forum after auth.
- `docs/spec/insula-host-macos.md` — Vestibulum-
  macos-bridge implementation.
- `docs/NAMING.md` — naming reference.
