# Loculus — wallet and autofill service

Status: design sketch (pre-implementation).
Last updated: 2026-05-21.

**Loculus** (Latin: *small carried purse / box for
valuables*) is Insula's structured-data wallet — the
service holding user-curated items (addresses, payment
methods, saved form profiles, generated identities) and
exposing them to apps via the powerbox autofill pattern.

This document expands `insula.md` §16 into implementation
detail.

## 0. Position

### 0.1 What Loculus is

- A **user-curated store** of structured data items.
- The **powerbox surface** for autofill — apps request a
  data item by type filter; user picks one; only that
  picked item is delivered.
- The **trusted UI** for editing the wallet — outside any
  app's jail, owned by Loculus itself.
- The **single home** for personal structured data the
  user wants to share with apps on demand.

### 0.2 What Loculus is not

- **Not the keychain.** Passwords, passkeys, and signing
  keys live in Vestibulum (§13). Loculus holds non-
  credential structured data: addresses, profiles, card
  metadata (but not raw card numbers — see §3.4).
- **Not a password manager.** Sign-in flows go through
  Vestibulum's sign-in UI; apps never see passwords.
  Loculus is for the *other* fields on a form (name,
  address, phone, …).
- **Not a contacts app.** Contacts are people *other
  than the user*; the user manages them in a contacts
  app. Loculus stores items about the user themselves.

### 0.3 Why the keychain/wallet split

Two reasons keeping these separate matters:

1. **Threat models differ.** A leaked password
   compromises an account; a leaked address compromises
   privacy but not authority. The data with the
   strongest threat (keychain) gets the strongest
   isolation.
2. **Sharing semantics differ.** A password is
   *secret-from-the-app* (apps never see it; the
   keychain proves possession on their behalf). An
   address is *shared-with-the-app* (the user gives it
   to the app explicitly). Powerbox handles the
   second; Vestibulum's sign-in UI handles the first.

## 1. Architecture

```
App                       loculusd                  Vestibulum
  │                          │                          │
  │ request_embed("autofill", filter)
  ├────────────► Limen ─────►│                          │
  │                          │                          │
  │                          │ enumerate items matching filter
  │                          │ (from local store)       │
  │                          │                          │
  │                          │ render Loculus picker UI
  │                          │ (Pergola surface, owned by Loculus)
  │                          │                          │
  │                          │ user picks item X        │
  │                          │                          │
  │                          │ if item X has sensitive  │
  │                          │ fields (card number) →   │
  │                          │ delegate token mint      │
  │                          │ ────────────────────────►│
  │                          │ ◄────── token────────────│
  │                          │                          │
  │ message: picked(item X data, possibly token)        │
  │◄─────────────────────────┤                          │
  │                          │                          │
```

`loculusd` is a system daemon, in the TCB (`insula.md`
§24.4) because it holds personal data and renders the
trusted picker. Surface is small: item store + picker
UI + Limen `autofill` role implementation.

## 2. Item types

Loculus ships with a curated set of item schemas. Each
schema declares fields, validation, and a *minimum-
share* default — the field subset apps get by default
when a user picks the item, with the user able to
expand selectively.

### 2.1 Address

```cddl
address = {
  "id"           : tstr,
  "label"        : tstr,        ; "Home", "Work", "Mom's place"
  "name"         : tstr,        ; recipient name
  "lines"        : [+ tstr],    ; street lines
  "city"         : tstr,
  "region"       : tstr,        ; state/province
  "postal-code"  : tstr,
  "country"      : tstr,        ; ISO 3166-1 alpha-2
  ? "phone"      : tstr,
}
```

Minimum-share default: all fields except `phone`.

### 2.2 Payment-method metadata

```cddl
payment-method = {
  "id"           : tstr,
  "label"        : tstr,        ; "Personal card"
  "scheme"       : tstr,        ; "visa", "mastercard", "amex", "device-pay"
  "last-four"    : tstr,
  "expiry"       : tstr,        ; "MM/YYYY"
  "billing-address-id" : tstr,  ; references an address item
}
```

**Loculus does NOT store the full card number.** Full
PANs (Primary Account Numbers) live in a trusted payment
provider's vault (Apple Pay / Google Pay / a bank's
secure-element analogue). What Loculus stores is metadata
sufficient to *identify* the card to the user during
picking, plus a reference to the secure-element-stored
real number.

When an app uses Loculus to pay, the powerbox flow
returns a **payment token** (single-use, time-limited,
amount-bound) minted by the payment provider via
Vestibulum, never the underlying card number.

### 2.3 Profile

```cddl
profile = {
  "id"           : tstr,
  "label"        : tstr,        ; "Personal", "Work"
  ? "given-name" : tstr,
  ? "family-name": tstr,
  ? "email"      : tstr,
  ? "phone"      : tstr,
  ? "date-of-birth" : tdate,
  ? "language"   : tstr,        ; BCP 47
}
```

Profiles are sub-personas-of-personas: a user might
have one Vestibulum persona ("personal") with two
Loculus profiles ("real name", "online pseudonym").

### 2.4 Generated identity (one-off)

For "create account" flows where a user wants to *not*
share their real info:

```cddl
generated-identity = {
  "id"           : tstr,
  "label"        : tstr,        ; "ForExampleSite.com 2026"
  "given-name"   : tstr,        ; generated
  "family-name"  : tstr,        ; generated
  "email"        : tstr,        ; aliased through a relay
  "phone"        : tstr,        ; aliased through a relay
  "created-at"   : uint,
  "scope"        : tstr,        ; the site/app it was generated for
}
```

The aliased email + phone require relay services (Apple
Hide My Email-shape) — out of scope for v1 in detail,
but the Loculus item shape accommodates them.

### 2.5 User-defined item types

Loculus permits user-defined item schemas. Schemas are
declared in Curia; items of those types are stored and
participate in autofill, but apps must explicitly opt
in to receive user-defined types (since they aren't in
the standard catalogue).

## 3. Storage

### 3.1 Local store

Loculus persists items in its own Tessera namespace,
encrypted with a Loculus master key derived from
Vestibulum:

```
/var/db/atrium/loculus/<persona-id>/
  items.cas       ← CAS-stored encrypted items
  index.db        ← lightweight metadata index for picking
```

Items are individually encrypted (one symmetric key per
item, wrapped by the master key) so that exposing one
item via autofill does not leak others.

### 3.2 Sync

Loculus can opt into sync via Atrium's sync subsystem
(referenced in `insula.md` §15.5). Synced items
propagate to the user's other Atrium devices, end-to-
end-encrypted with the persona's keys.

Loculus sync is **per-persona**: switching personas
switches the wallet contents the user sees.

### 3.3 Backup

Loculus items are included in the device backup. The
backup is encrypted with the user's recovery key
(`insula.md` §13.7). Loss-of-everything still loses the
wallet without the recovery key — that is honest, not
papered over.

### 3.4 Card numbers — handled by payment provider

For PCI-class data (full card numbers), Loculus delegates
to a configured payment provider (Apple Pay / Google Pay /
an Atrium-native payment vault). The flow:

1. User taps "Add card" in Loculus UI.
2. Loculus invokes the payment provider's enrollment UI
   via Limen `payment-enroll` role (if exists) or by
   handing off to the provider's app.
3. The provider tokenizes the card; returns metadata
   only (label, last-four, expiry).
4. Loculus stores the metadata; the real number lives
   in the provider's vault.
5. At payment time, Loculus asks the provider to mint
   a transaction token for the requested amount.

Loculus is a *metadata index* for cards; the actual
PAN never crosses Loculus's process boundary.

## 4. Autofill protocol — the `autofill` Limen role

### 4.1 Role schema (Limen wire format)

| Direction | Message | Payload |
|---|---|---|
| Parent → Child (Loculus) | `request` | `{ types: [string], hints: { … }, scope: string }` |
| Child → Parent | `picked` | `{ item-type, item-data, optional payment-token }` |
| Child → Parent | `cancelled` | — |
| Child → Parent | `no-match` | — (user explicitly chose to skip) |

### 4.2 Type filter and hints

The app's `request` declares acceptable item types and
optional hints:

```cbor
{
  "types": ["address"],
  "hints": {
    "shipping": true,           ; we want a shipping address
    "country-pref": "US",       ; bias the picker
  },
  "scope": "com.example.checkout"  ; for user transparency
}
```

The `scope` is shown to the user in the picker — "App
com.example.checkout is asking for an address." Honest
disclosure of which app is requesting.

### 4.3 Picker UX

Loculus renders the picker inside the Limen slot
allocated by the parent app, but the surface is **owned
by Loculus** — the parent never sees the picker's
pixels or its DOM-equivalent state.

Picker shows:
- Items matching the type filter.
- Hint-sorted (closest match first).
- "Choose different fields" option for granular sharing.
- "Cancel" / "Skip this time."

User picks. If granular sharing was selected, user
ticks which fields to share. Loculus emits `picked` with
those fields and *only* those fields.

### 4.4 What apps receive

The app receives a typed CBOR blob with the chosen
item's selected fields. For card-shaped items, the
blob includes a payment-token (single-use, bound to
amount + merchant + timestamp) rather than the card
number.

### 4.5 What apps do NOT receive

- The list of other items in the wallet.
- The user's other personas.
- Fields the user did not opt in to share.
- Anything about *non-pick* outcomes beyond the `picked`/
  `cancelled`/`no-match` distinction.

## 5. Item lifecycle

### 5.1 Create / edit

Items are created via the Loculus app — a normal Insula
app, just shipped by the platform and trusted because
the user got it through Opifex. Editing happens in its
own UI; no app can write to Loculus.

(Exception: payment-enroll flow can trigger Loculus to
create a payment-method item after the provider
tokenizes. This is a delegated create, mediated by
Limen.)

### 5.2 Delete

User-driven from Loculus UI. Deleted items are removed
from local store + sync; backed-up versions persist
until next backup rotation.

### 5.3 Audit log

Loculus keeps a per-item audit log: when, which app,
which fields shared. User can review in Loculus UI —
"this app saw your address on these dates." Stored
locally; not synced (audit log is per-device).

## 6. Trust model

### 6.1 Apps cannot read the wallet

The wallet is never readable from an app's jail. The
*only* way an app sees any item is the powerbox flow,
which delivers exactly what the user picked.

### 6.2 Loculus is trusted to handle the data correctly

Loculus is in the TCB. The user trusts it to:
- Render the picker outside any app's jail.
- Encrypt at rest.
- Mediate payment-provider tokenization correctly.
- Surface the audit log accurately.

### 6.3 Payment provider is trusted for PAN

Loculus does not see card numbers. The payment provider
is trusted for that data. The user's trust statement is
"I added this card; my payment provider has it."
Loculus exists to make this fact navigable from apps,
not to replace it.

## 7. API

### 7.1 App-side C ABI (`libatrium_loculus.h`)

Apps don't talk to Loculus directly — they go through
Limen with the `autofill` role. The convenience wrapper:

```c
typedef struct {
    const char**  types;        /* item types accepted */
    size_t        n_types;
    const char*   scope;        /* app's identity for user disclosure */
    /* optional hints — type-specific structured data */
} atrium_loculus_request_t;

typedef struct {
    char         item_type[32];
    uint8_t*     item_data;     /* CBOR */
    size_t       item_data_len;
    /* for payment: */
    uint8_t*     payment_token;
    size_t       payment_token_len;
} atrium_loculus_result_t;

int atrium_loculus_request(
    atrium_window_t* parent,
    atrium_rect_t    rect,
    const atrium_loculus_request_t* req,
    atrium_loculus_result_t* out);
```

Under the hood this calls `atrium_limen_request_embed`
with the `autofill` role, sends the request message,
awaits picked / cancelled.

### 7.2 Loculus internal admin ABI

For the Loculus app itself (which manages the wallet),
plus the payment-provider-handoff case. Not exposed to
ordinary apps.

## 8. Performance and resource

| Metric | Target |
|---|---|
| Picker cold launch | <100 ms |
| Item enumeration on filter | <10 ms (1000 items in store) |
| Sync delta upload | <1 s for typical wallet (~100 items) |
| Idle daemon RAM | <16 MB |

## 9. Bring-up phases

### 9.1 Phase A — addresses and profiles

- `loculusd` daemon.
- Local Tessera-backed store.
- Address + profile item schemas.
- Picker UI in Loculus app (Pergola).
- Limen `autofill` role end-to-end.
- Sample app exercising autofill.

Goal: a checkout-shape demo app can request an address;
user picks; app receives. No payment yet.

### 9.2 Phase B — payment

- Payment-method item schema with metadata-only storage.
- Payment-provider integration interface (initial:
  one configured provider).
- Payment-token mint flow with amount + merchant + ts
  binding.

### 9.3 Phase C — sync and personas

- Per-persona separation; persona switch in Forum.
- Sync integration (encrypted, end-to-end).
- Recovery via recovery key.

### 9.4 Phase D — generated identities + audit polish

- Generated-identity item with aliased email/phone
  (depends on relay services existing).
- Audit log UI.
- User-defined item types.

## 10. Open questions

- **Payment provider abstraction.** What's the protocol
  between Loculus and a payment provider? Apple Pay /
  Google Pay are closed; an open Atrium-native vault
  is needed eventually. Probably its own spec.
- **Cross-app share patterns.** Some "wallet" use cases
  (loyalty cards, tickets, IDs) shade into "shareable
  credentials." These probably are a sibling type of
  item, not generic Loculus items.
- **Bulk fill detection.** A malicious app that
  repeatedly invokes autofill could try to harvest
  many items through small probes. Rate limiting per
  app + audit-log surfacing should handle, but design
  TBD.
- **Aliased email / phone relay.** The "generated
  identity" item type implies a relay service that
  forwards email/SMS. Atrium-native or contracted from
  an existing provider? Open.
- **User-defined item schema validation.** Allowing
  user-defined schemas opens validation and
  interoperability questions. Likely v2.

## 11. References

- `docs/spec/insula.md` — parent; §16 is the design
  summary, §13 is the keychain (Vestibulum) split.
- `docs/spec/limen.md` — the embed broker; Loculus
  implements the `autofill` role.
- `docs/spec/vestibulum.md` (or equivalent) — keychain;
  master-key derivation source.
- `docs/spec/tessera-fs.md` — encrypted storage backing.
- `docs/NAMING.md` — naming reference.
