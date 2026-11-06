# Arca — the system secret store

*Proposed 2026-11-05 at the user's request; §12's recommendations accepted the same day: "it should be in some core area which apps like
navigator can access... keeping it safe and secure and when sharing ensuring it goes only to
the intended recipient, while avoiding common malicious attacks." Nothing here is built. The
name is a proposal (Latin* arca*: strongbox).*

**Arca** is one system service that holds every secret a user has: keys, passwords, tokens,
cookies and API keys. Every app reaches it through the same API, and the Navigator is one
client among many. It replaces two things that were heading towards separate designs:

- **The keychain inside `vestibulumd`** ([vestibulum.md](vestibulum.md) §3, §5, §7.2).
  Vestibulum keeps login, lock, user verification and the root key. The items move to Arca.
- **The Navigator's "credential vault"** ([threat model](atrium-navigator-threat-model.md)
  §6). Its rules become Arca's web profile (§8 here), and its "per-origin fetcher" becomes
  Arca's courier (§5).

[Loculus](loculus.md) stays separate. It holds data the user *shares with* an app (addresses,
profiles). Arca holds data that is *kept from* the app. Loculus's master key is an Arca item.

## 1. The three promises

1. **Use without read.** Wherever the protocol allows, a secret is *used* for the app
   (sign, decrypt, attach) and never *given* to it. A fully compromised app gets the
   effect of the secret while it runs, and never the secret itself.
2. **Only the intended recipient.** Every secret is bound, when it is created, to the one
   party that may receive it: a network endpoint, an app or the user's own eyes. Arca
   releases it only to that party, **as the system verified it**, never as the request
   claims it.
3. **Useless at rest.** On disk, a secret can be read only after the user unlocks, and
   only on this machine where hardware allows.

Everything below is how those three hold up against real attacks.

## 2. What Arca holds

Secrets come in two shapes, and the shape decides how they can be handed over.

| shape | examples | how it is used | can the holder be kept from reading it? |
|---|---|---|---|
| **key** | passkeys, per-service keypairs ([insula.md](insula.md) §13.3), SSH keys, signing keys, TLS client-certificate keys, device-bound session (DBSC) keys, push keys, the Loculus master key | Arca signs, decrypts or derives on request | **yes, always.** The private half never leaves Arca, or the TPM where one exists |
| **bearer** | passwords, cookies, OAuth access and refresh tokens, API keys, HTTP auth | whoever presents it is believed | **only if Arca does the presenting.** That is the courier (§5) |

Bearer secrets are the hard case. Whoever holds one *is* the user to the service that
accepts it. So the design goal for bearer secrets is that they travel only on a connection
Arca controls, and reach no app's memory unless the user agrees to that weaker class (§4.3).

Each item records:

| field | meaning |
|---|---|
| owner | the user and persona ([insula.md](insula.md) §13.6). Personas never share items |
| shape and kind | key or bearer; password, cookie, passkey and so on |
| **recipient** | who may receive it or its effect (§3). Set at creation, never widened silently |
| class | when it is usable: after first unlock, while unlocked, or with user verification (§6.2) |
| scope | optional narrowing: methods, paths, header name, expiry |
| provenance | who created it: the user typed it, a server set it, an app minted it |
| audit | last used, by which client, for which recipient |

## 3. Recipients: "only the intended recipient", made precise

A recipient is something the **system** can verify, never a string an app supplies.

| recipient | verified by | example |
|---|---|---|
| **network endpoint**: scheme, host, port, and for web items the partition (§8.2) | the courier's own TLS handshake: WebPKI name check, plus an optional pinned key | the cookie for `https://mail.example`; an API token for `https://api.github.com` |
| **app**: publisher key plus app id | the signed manifest jaild verified at launch ([jaild-policy.md](jaild-policy.md)); Arca gets the label from the kernel peer uid and the launch registry (§4.1), not from a message | a database password that a native client must hold itself |
| **the user** | a Vestibulum-verified presence check, then the trusted UI | "show password", copy to clipboard |
| **a key operation** | not released at all: Arca does the operation | WebAuthn assertion for RP ID `example.com` |

**The binding rule.** An item is released only to its recipient. A request names a
recipient, but the release happens only after the system has checked that recipient:

- the courier has completed TLS to the bound endpoint, or
- the connection's peer uid belongs to the bound app (§4.1), or
- the user has passed Vestibulum's check.

Matching is exact:

- **Endpoint matching is by origin.** Registrable-domain matching is never enough.
  `https://login.example.com` and `https://example.com` are different recipients unless
  the item says otherwise, and the trusted UI shows that to the user.
- **App matching is by publisher key and app id.** A process name, path or claimed bundle
  id never matches.

A changed recipient is a new item. A server can't move its cookie to another host; an app
update keeps its items only if the publisher key is unchanged. Moving an item to a new
recipient is a user action in the trusted UI, never an API call.

**Sharing between apps** works the same way. App B can use App A's item only after a
trusted prompt ("App B wants to use your X credential"). The user's yes grants one item, to
one app, and can be revoked. It is logged and shown in the trusted UI.

## 4. Who talks to Arca, and how Arca knows who they are

### 4.1 Identity is the kernel's answer, not the message

*Amended 2026-11-06 (user's decision): Arca uses the platform's one identity primitive
instead of a socketpair of its own.*

Arca never reads "I am app X" from a request. It asks the kernel who is connected:

1. portcullisd launches every app under a **dedicated per-app uid** (portcullis.md §9.0)
   after verifying its signed manifest, and records `uid → (owning user, app id,
   publisher key)` in the launch registry (`portcullis-peer`). Tabula, Praeco, Choragus
   and Fresco read the same registry.
2. Arca takes the peer's uid with `getpeereid` on each accepted connection and resolves it
   in the registry. The label is (user, publisher key, app id). A uid with no registry
   entry is not an app, and gets nothing.
3. An app id that is re-installed under a different publisher key is a different
   identity: its registry entry carries the new key, and none of the old items match
   (§3, a changed recipient is a new item).
4. Couriers (§5) run under uids from their own reserved range, recorded with their label
   (user, persona, recipient endpoint) the same way. Vestibulum is a fixed system uid.

So the label is fixed by jaild, portcullisd and the kernel. A compromised app can send any
bytes it likes, but its uid is not among them. No path, socket name or pid is ever
trusted. One mechanism for every service is also one thing to audit, rather than a
second channel scheme only Arca uses.

### 4.2 The clients

| client | gets | never gets |
|---|---|---|
| **an app** (native, ported or the Navigator's parts) | key operations on its own items; courier sends for its items; bearer values only for items of class *app-held* (§4.3) | any other app's items; any bearer item it does not hold itself |
| **a courier** (§5) | for one connection: the bearer items bound to its endpoint, inserted into that connection | items for any other endpoint |
| **Vestibulum** | unlocks Arca's root at login and locks it on sign-out and lock (§6); renders the user-verification prompt | item values |
| **the trusted UI** (part of Arca, drawn as a Pergola surface owned by Arca) | lists, edits, reveals with user verification | n/a, it is Arca |

The Navigator's broker, converter, document worker and island engines are ordinary clients.
They hold no item values at all. Their requests go through couriers (§8).

### 4.3 Three handover classes for bearer secrets, strongest first

1. **courier-attached** (default). The secret goes only into a connection the courier
   makes to the bound endpoint. The app sees the response and never the secret.
2. **slot-filled.** The app builds a request template with a slot, for example
   `Authorization: Bearer {arca:item/7f3a}`, and the courier fills it. It's the same
   guarantee as class 1, and it serves apps that build their own requests.
3. **app-held.** The value goes to the app's own memory. This is for protocols the courier
   doesn't speak yet: IMAP, a database wire protocol, a legacy app under Insula. It
   requires a one-time user grant naming the app and the item. It is logged, and it is
   shown in the trusted UI as weaker. Release is still bound to the app's verified
   identity (§3).

## 5. The courier: how a bearer secret reaches a server

A courier is a short-lived jail that makes **one connection to one endpoint** on behalf of
one user and persona. It is the only process that ever holds a bearer secret outside Arca.

**What it does, per request:**

1. It receives a request from its client in a **fixed, length-prefixed schema**: method,
   path, query, an allowlist of headers, body bytes and slots. There is no free-form
   header text and no URL parsing beyond the path.
2. It resolves and connects through the existing fetch rules: the private-address filter
   and the address pin of threat model §2.1. It completes TLS and checks the name against
   its bound host.
3. **Only now** does it ask Arca for the items bound to this endpoint that match the
   request (§8.1 for cookies). Arca checks the courier's label against each item's
   recipient.
4. It attaches the items and sends the request.
5. It parses the response headers. It sends `Set-Cookie` and any declared token fields to
   Arca as structured records, removes them, and passes the rest to the client.
6. It wipes the items from memory. Then it exits, or stays for that endpoint only (§5.2).

**Redirects.** The courier never follows a redirect to another endpoint. It returns the
redirect to its client, which must start a new courier for the new endpoint. So an open
redirect on `example.com` can't carry `example.com`'s token to `evil.example`. That's a
common token-leak route on today's web, and here it is closed by construction.

**TLS** is rustls with the `ring` provider, the stack the rest of Atrium uses. The choice,
and the hardening still to do (revocation, ECH, pinned keys, a system trust store), are in
[../TLS.md](../TLS.md).

### 5.1 Why a courier compromise is bounded

The courier parses two hostile inputs, so each one has to be weighed.

- **Response bytes from the server.** TLS ensures only the bound server can send them. If
  that server exploits its courier, it learns the items bound to *itself*: secrets it
  issued, or the user's password for it. That is nothing it couldn't get from an honest
  login.
- **Request bytes from its client.** This is the real risk. A compromised client could
  attack its courier to read the secret. That's why the request side is a fixed binary
  schema, fuzzed and allocation-bounded, with no text grammar. The Navigator's case is
  sharpest: a mail page's converter parses attacker-written email, then asks the mail
  origin's courier for requests.
- **Defence in depth.** The courier receives an item only *after* TLS is up, holds it for
  one request, and never holds an item for another endpoint. The worst case is one
  endpoint's items for one user and persona. A browser renderer exploit commonly reaches
  every site's cookie store.

### 5.2 Cost

One jail per request would add about 47 ms where the jail needs its own vnet
([network.md](network.md) §0). So couriers are **per endpoint and reused**: kept warm while
that endpoint has an open view, then recycled after a bounded number of requests or minutes.
Reuse doesn't widen what a courier holds, because each request still fetches its items fresh
and wipes them afterwards.

### 5.3 As built: A1 (2026-11-06)

`arca/arca-courier` is the courier and `arcad` its counterpart.

- **Identity.** Couriers run as uids 64000–64999. The launcher writes each uid's label to
  the courier registry (`/var/run/atrium/courier-registry`, one line
  `uid user persona https host port [partition]`). Both arcad and the courier read the
  label from there by uid. A courier uid never resolves as an app, and a duplicate line
  voids the uid.
- **Whose items.** An app stores a token for an endpoint with `STORE_FOR`. The item's
  recipient is the endpoint, and its persona is the app's own
  (`app-` + 16 bytes of SHA-256 over publisher and app id). So only couriers whose label
  names that persona and that endpoint can fill it. The app itself can't read it back,
  and another app's courier to the same server gets nothing. The launcher sets a
  courier's persona from the app that asked for it. The Navigator's web personas come
  with A2.
- **The request** (`schema.rs`) is a fixed binary frame. It has a method, a path and a
  query (visible ASCII, no `#`, no `?` in the path), up to 16 headers from a seven-name
  allowlist, up to 8 slots and up to 256 body segments (1 MiB). A slot is an item id or a
  credential handle. It goes either into `Authorization: Bearer`, or into the body as raw,
  form or JSON-string text. Every body slot must be used, and anything accepted has
  exactly one encoding. The courier writes Host, Content-Length, Connection and
  Accept-Encoding itself. A Bearer value with any byte outside visible ASCII is refused
  `Unsendable`.
- **Order.** The courier resolves through navigator-fetch's address policy (private
  ranges refused unless granted) and connects. It finishes the rustls handshake to the
  label's host. Only then does it send arcad `FILL`, which is all or nothing. It builds the
  request in locked memory, sends it, and reads the response with navigator-fetch's
  bounded reader. A kept-alive connection is reused for the next request to the same
  endpoint.
- **The response.** Every `Set-Cookie` goes to arcad as a `SET_COOKIE` record and is
  removed. Arca never fills a cookie by id; it validates and matches them itself (§8.7). Any echo of a sent value in headers or body, raw or encoded, is masked
  with `*`. Redirects come back to the client as they are: the courier follows none.
- **Gates.**
  - `arca-courier/tests/courier.rs` runs against loopback TLS servers. It checks that the
    token reaches only the bound host and that the cookie goes to Arca and the echo is
    masked. A 302 to evil.example is returned and not even looked up. A wrong-name cert,
    an untrusted CA or a refused address asks Arca nothing. Form and JSON encoding are
    checked. A hostile-client run of 20,000 mutated frames checks that every malformed
    frame is refused before Arca or the network and that valid frames ask for exactly
    their own refs.
  - `fuzz/` is a libFuzzer target for the schema.
  - `arcad/tests/courier.rs` checks the rules: another endpoint, another app or publisher,
    the app itself, all-or-nothing fills, cookies never by id, and couriers getting no
    other ops.
  - `scripts/arca-vm-test.sh` §4 runs the courier in the VM under its own uid against
    the jailed arcad (with `su`; §5.4 is the jailed launch).
- **Not yet.**
  - Capsicum for the courier after connect.
  - Handles from arca-ui are bound to the app, so they can't be redeemed by a courier
    yet. They need the field's endpoint (A3).
  - Pinned keys and revocation ([../TLS.md](../TLS.md)).

### 5.4 As built: a jail per courier (2026-11-06)

`arca/arca-courierd` starts every courier in its own jail. It runs as root, because
jaild serves only root, and it is never jailed itself. It holds no secret and reads no
item.

- **Asking.** An app connects to `/atrium/sockets/arca-courier/courierd.sock` and sends
  an endpoint in arcad's encoding (`u8 scheme=1 | str host | u16 port`). It reads one
  status byte. On success the courier's socket comes with that byte (SCM_RIGHTS). The
  app is the kernel's peer uid resolved through the launch registry, as in arcad. The
  courier's persona is that app's endpoint persona. Partitions (web personas) are
  refused here; they come with A2's web broker.
- **The address.** The launcher resolves the host once, to IPv4 addresses that
  navigator-fetch's policy admits (at most 8). No admitted address refuses the request.
  The courier is given the same list (`ATRIUM_COURIER_ADDRS`) and connects only to it,
  with the policy still applied.
- **The uid.** The lowest free uid in 64000–64999, at most 16 live per app. Its label
  is appended to the courier registry before the jail starts. When the courier exits,
  the launcher first appends `uid retired`, then removes the jail, and only then frees
  the uid. The registry is append-only while couriers run; it is truncated in place
  when the launcher starts (never replaced: arcad's nullfs mount pins the file).
- **The jail** (jaild `[couriers]`). It is named `system-arca-courier-<uid>` and rooted
  at `/var/lib/atrium/jails/<name>`. It runs exactly `/usr/local/bin/atrium-arca-courier`
  as that uid, with the app's socket as stdin and stdout. jaild lets only such a jail take
  its caller's descriptors, apart from one-shot instances, and gives it `allow.mlock`.
  It sees `/lib`, `/libexec`, `/usr/lib` and `/usr/local/bin` read-only, arcad's socket
  directory, the courier registry read-only and a copy of the trust store. It sees
  nothing of the user's. Its network is its own vnet (`Routed`), and its pf anchor
  passes TCP only to the endpoint's addresses on its port, plus the host's resolvers,
  which jaild adds to every restricted anchor.
- **Teardown** is the janitor's: it retries until the kernel frees the jail, then
  unmounts, removes only what the launcher wrote and then the empty directories. A
  surviving mount is never removed recursively. At start the launcher hands every
  courier jail a previous run left to the janitor.
- **Gates.**
  - `arca-courierd/tests/launcher.rs` covers the wire, the address filter (no loopback,
    LAN without the grant, metadata or IPv6), the per-app cap and uid reuse, the
    descriptor passing, and the in-place registry reset.
  - jaild's `a_courier_jail_and_only_a_courier_jail` checks that a wrong uid, root,
    binary, name or range is an ordinary jail and is refused.
  - `scripts/arca-vm-test.sh` §5 runs an app's courier in its jail against a TLS server
    outside the VM. It checks the uid, the label, the pf anchor, the root's contents,
    the token arriving and masked, the jail gone with its label retired and no mount
    left when the app closes, and the refusals.

## 6. At rest and in memory

### 6.1 The key hierarchy

```
user credential (passcode, biometric, recovery key)        — Vestibulum
   └─ user root key  (Argon2id from the passcode; sealed to the TPM when present)
        └─ Arca master key            (unwrapped into Arca at unlock, never written in clear)
             ├─ class key: after-first-unlock
             ├─ class key: while-unlocked          (evicted at lock)
             └─ class key: verify-each-use         (unwrapped per use after a Vestibulum check)
                  └─ per-recipient data key (random; wrapped by its class key)
                       └─ items  (XChaCha20-Poly1305; AAD = user, persona, class, recipient, item id, version)
```

- **The AAD binds each record to its recipient.** Copying an encrypted record from one
  recipient's file to another's makes it fail to decrypt. Recipient binding then holds even
  against someone who can edit the store.
- **A per-recipient data key makes clearing real.** "Forget this site" destroys one wrapped
  key, and every record under it is unreadable. No search through the store is needed.
- **Hardware.**
  - *TPM 2.0* (present on most x86_64 machines, including the R9700 box's class): the user
    root key is sealed to it, and keys of class *device-bound* (DBSC, optionally passkeys)
    are generated in it and never leave it. The TPM's dictionary-attack lockout limits
    passcode guessing on a stolen disk.
  - *No TPM* (the dev VM, some laptops): allowed. The root key comes from the passcode
    through Argon2id alone, at a high cost setting. That's weaker against a stolen disk,
    and setup says so.
  - *Optional FIDO2 key:* a user can mix an `hmac-secret` answer from a hardware key into
    the root key, so unlocking needs the key as well as the passcode.
  - *Not in scope: the Insula host on macOS.* `vestibulum-macos` keeps its own keystore
    (with `master.key` in clear at mode 0600) for Atrium apps running on a Mac. That is an
    Insula-host question, not Arca's, and is out of this plan (user, 2026-11-06).
- **Rollback.** A store restored from an old copy would bring back sessions the user logged
  out of. With a TPM, a monotonic NV counter is stored in the header and checked at unlock.
  Without one, a rollback is detected only across devices that sync.
- **The store** is a per-user file outside Tessera's CAS. It is never deduplicated and never
  shared (threat model §2.6).

### 6.2 Classes: when is a secret usable?

| class | usable | default for |
|---|---|---|
| after-first-unlock | from the first unlock after boot until shutdown, including while the screen is locked | cookies, tokens, push keys: so push and the refresh of **pages already open** work while locked. A navigation that starts while locked gets no cookies |
| while-unlocked | only while the session is unlocked; the key is evicted at lock | passwords, refresh tokens, API keys |
| verify-each-use | each use passes a Vestibulum check (biometric or passcode) | revealing a password, exporting, passkey assertions with user verification, high-value items the user marks |

### 6.3 Arca's own process

- **No network, no parser of anything external.** Arca reads only its own store and the
  structured records couriers send, under a small fixed schema.
- **A jail of its own, Capsicum after start.** Its descriptors are its store directory,
  its listening socket, the launch registry (read-only) and its Vestibulum and TPM links.
- **Memory.**
  - Every page holding a clear key or item is `mlock`ed, so it never reaches swap or the
    zram compressor ([atrium-memory-pressure.md](atrium-memory-pressure.md)).
  - Core dumps and tracing are disabled (`procctl` `PROC_TRACE_CTL`).
  - Secrets are zeroized after use. Comparisons are constant-time.
- **Logs never contain values**, only item ids and recipients.
- **Small and fuzzed.** The request schema, the Set-Cookie record and the store format
  each have a fuzz target and a positive control.

## 7. Common attacks, and what stops each one

| attack | what stops it |
|---|---|
| **phishing**: a lookalike site asks for the password | Fill happens only for the bound origin, matched exactly (§3). Confusable hosts are refused (threat model §2.11). The fill chip is drawn by the trusted UI, not the page. Passkeys can't be phished, because the RP ID is checked against the origin the courier actually reached |
| **a malicious app asks for another app's secret** | Identity is the peer uid's registry label, from the signed manifest (§4.1). There is no lookup by name |
| **a compromised app or renderer reads memory** | Keys: never there. Bearer secrets: only in the courier, after TLS, for one request (§5). App-held items are the stated exception (§4.3) |
| **an open redirect carries a token away** | Couriers never follow a redirect to another endpoint (§5) |
| **a man in the middle or a downgrade** | The courier attaches only after TLS verifies the bound host. An item from an `https` recipient is never sent over `http`. A user-installed root CA that intercepts is flagged in the trusted UI |
| **DNS rebinding or hijack** | TLS name check plus the address pin and the private-range filter (threat model §2.1) |
| **CSRF or a confused deputy**: a page makes the user's session act | SameSite and partitioning for the web (§8.2). Request rate and shape limits per recipient (threat model §6.6). Optional per-item scope (methods, paths) |
| **a fake prompt or clickjacking** | All prompts are Vestibulum or Arca surfaces with system chrome that no app can draw ([vestibulum.md](vestibulum.md) §4.3). Limen forbids pixel readback |
| **keylogging the password field** | Keystrokes for a marked password field go from the UI to Arca, never to the app or page (§8.3) |
| **stolen disk or laptop** | Items are encrypted under a key that needs the passcode, sealed to the TPM where there is one. The TPM lockout limits guessing |
| **cold boot or memory dump** | `mlock`, no core dumps, while-unlocked keys evicted at lock, zeroize |
| **a restored old store** | The TPM monotonic counter (§6.1) |
| **leaks through backup, sync or clipboard** | Backup and sync are end to end under the recovery key ([vestibulum.md](vestibulum.md) §6). Cookies never sync. Clipboard copies are user-initiated, shown and cleared after 60 s |
| **brute force through the API** | Vestibulum's lockout. Per-client rate limits on verify-each-use |
| **a supply-chain bug in Arca** | Rust, a minimal dependency set, fuzzed boundaries, and its own jail, with no network to send anything anywhere |

## 8. The web profile: how the Navigator uses Arca

This is threat model §6 restated on Arca. Nothing in it is a Navigator-only mechanism.

### 8.1 Cookies

- **Recipient = (partition, origin).** One item group per pair. The courier for
  `(news.example, cdn.example)` can never receive `(mail.example, cdn.example)`'s
  cookies.
- **Arca does the matching**: domain, path, `Secure`, `SameSite`, expiry and the prefixes.
  The courier receives only the `Cookie` header value for the request it is about to send.
- **Set-Cookie** arrives from the courier as a structured record. Arca validates it against
  the courier's label:
  - the `Domain` must domain-match the origin and must not be a public suffix (a public
    suffix list is built into Arca);
  - `__Host-` and `__Secure-` are enforced;
  - `Secure` is accepted only from `https`;
  - size and count limits apply per recipient.
- **`document.cookie`.** Recommendation: the page's own script sees only that origin's
  non-`HttpOnly` cookies, in that origin's private, credentialed conversion or its tier-4
  island. That matches the web's contract, since those cookies are script-readable by
  design. `HttpOnly` cookies never leave Arca and its couriers. Writes go to Arca through
  the same validation.

### 8.2 Partitioning, SameSite and tickets

These are as in threat model §6.3: no third-party cookies, `SameSite=Lax` by default, and
first-party IdP cookies at the IdP.

**Tickets.** Arca needs to know why a request is made. **The broker says so, not the
page.** For each credentialed request, the broker gives Arca a ticket naming:
- the reason: a navigation, a subresource or a form submission;
- the SameSite context: the initiator site and the target;
- whether a user gesture started it.

Tickets are single-use, expire within seconds, and are bound to one origin and one
request, so a ticket can't be replayed or reused for another request. The courier redeems
the ticket with the origin it actually reached over verified TLS. Arca
releases the `Cookie` value and any slot values only when the ticket, the courier's label
and the item's recipient all agree, over `https`. A compromised converter can't claim a
same-site context or a gesture, because it never holds a ticket.

### 8.3 Passwords

- **Typing.** The UI recognises a password field from the semantic tree (threat model
  §6.4), and the broker validates the mark. Keystrokes for that field go from the UI to
  Arca. The page sees a fixed number of bullets, not the real length.
- **Submitting.** The broker builds the form POST, and the password field holds a slot.
  The courier for the form's action origin fills the slot only if all of these hold:
  - the origin is exactly the item's recipient;
  - the action is same-origin and `https`;
  - the broker has stamped a user gesture.
- **Echoes.** If the site echoes the submitted password back in its response, the courier
  replaces it with bullets before the body goes on to the converter.
- **Saving.** After a login succeeds, Arca's trusted UI offers to save. Generated passwords
  are created inside Arca and are never shown or copied unless the user reveals them with
  verification.
- **Filling** happens only on a user gesture, on the trusted fill chip. It is refused on
  `http:`, on a confusable host, and in a tier-4 island unless the user confirms.
  The full design, for every app and not only the Navigator, is
  [arca-autofill.md](arca-autofill.md): a credential field is an Arca-owned Limen slot,
  and the app holds a handle, never the value.

### 8.4 Passkeys (WebAuthn)

- **The authenticator is Arca.** A passkey is a key item with recipient = RP ID. ES256 is
  required, because nearly every relying party asks for it; Ed25519 is optional.
- **The WebAuthn client is the broker.** It computes the RP ID check against the origin
  the courier actually connected to, and builds `clientDataJSON` from trusted values.
- **User verification** is Vestibulum's prompt. Arca signs. Pages can request a credential,
  never read one.
- Passkeys sync through Vestibulum's end-to-end sync. Hardware security keys over CTAP2
  come later, through a USB HID helper jail.

### 8.5 Tokens, DBSC, client certificates, HTTP auth

- **OAuth and API tokens:** slot-filled (§4.3). A recorded binding names the response
  field that sets the token. The courier moves that field to Arca and passes a handle on.
  Tier 4 is the honest exception: the engine holds what its script reads (threat model
  §6.5).
- **DBSC:** a device-bound key, in the TPM where there is one. Arca signs the refresh
  challenge, so a stolen cookie is useless off this machine.
- **TLS client certificates:** the courier runs the handshake, and Arca signs
  `CertificateVerify`. The key never reaches the courier.
- **HTTP Basic, Digest and Bearer challenges:** a trusted prompt. The answer is stored as a
  password item bound to the origin and realm.

### 8.6 Lifetime and clearing

- Session cookies die when the origin's last view closes.
- "Forget this site" destroys the recipient's data key (§6.1).
- Credentialed conversions stay private to the user and are deleted with the jar (threat
  model §6.7).

### 8.7 As built: A2 cookies (2026-11-06)

- **The jar is arcad's** (`arcad/src/cookie.rs`). A cookie is an item: recipient
  = (https, its domain, the courier's port, partition), account = name, scope = its
  attributes (host-only, Secure, HttpOnly, SameSite, expiry, creation time, path). A
  `Domain=example.com` cookie lives in example.com's group. Matching walks the request
  host's parent domains up to the public suffix.
- **Set-Cookie** (RFC 6265bis) is checked against the courier's label. A `Domain` must
  domain-match the host and must not be a public suffix (the `psl` crate's built-in list).
  `__Secure-` and `__Host-` are enforced, and `SameSite=None` needs `Secure`. Lifetimes are
  capped at 400 days and name plus value at 4 KiB. A group keeps its newest 50. A refused
  cookie is ignored, as browsers do, and the courier strips the header either way.
  Expiry is by arcad's clock.
- **The courier** asks `COOKIES` with the request path after its TLS handshake, as it
  does `FILL`, and writes the `Cookie` header in locked memory. Each `Set-Cookie` goes
  to arcad with that path. Cookie values of 16 bytes or more (session ids) are masked
  in what the client gets back. Shorter ones are left alone, so a `true` in a page
  survives.
- **An app's courier** (no partition, the app's endpoint persona) gets every cookie that
  matches. Its requests are the app's own, so SameSite doesn't apply.
- **Web couriers** belong to the web broker, which `ARCA_WEB_BROKER` names as
  `app_id publisher` (arcad and arca-courierd; never unsigned). Only the broker may ask
  arca-courierd for a courier with a partition. That courier carries the user's `web`
  persona, and each partition is a separate jar.
- **Tickets** (`W_TICKET`, 0x70). The broker names the endpoint, partition, method, path,
  query, reason (navigation, subresource or form), the initiating host ("" for the
  reader's own navigation) and whether a user gesture started it.
  - A ticket lives 10 s and is redeemed once by `COOKIES`, from a courier whose label
    matches its user, partition, host and port, for exactly its method, path and query.
    A web courier's `COOKIES` carries the request's method and query after the ticket;
    a mismatch is refused and does not spend it. (Before 0bb0f5c3 a ticket named the
    path alone, so one for `GET /a?x=1` also opened `/a?x=2` and a `HEAD` or `POST` to
    `/a`.)
  - After redemption, it admits that response's `Set-Cookie`s for 60 s.
  - A web courier with no ticket, or the wrong one, gets no cookies and can set none.
  - An app's courier can't use a ticket.
- **SameSite by the ticket.** Same-site means the initiator's registrable domain is the
  target's, or the reader started the navigation. Strict cookies go only on same-site
  requests. Lax, and cookies with no attribute, also go on a cross-site top-level GET or
  HEAD. None always goes.
- **Gates.**
  - `arcad/tests/cookies.rs` has 11 tests: matching, Domain and public-suffix refusals,
    prefixes, expiry and deletion, the 50 limit, tickets bound to their request, partition
    isolation, the SameSite table, and broker-only tickets.
  - `arcad/fuzz` `set_cookie` fuzzes the parser. Its first run found a panic in date
    parsing on a multi-byte character, which is now fixed; it then ran 2.3M inputs clean.
  - `scripts/arca-vm-test.sh` §5 checks that a jailed app courier sends back the cookie
    its first response set. §6 runs a web courier from the broker through four contexts
    against a real server: the reader's navigation, a cross-site subresource, a same-site
    navigation, and no ticket. Only the same-site navigation carried the cookie.
- **Not yet.**
  - Session cookies dying with the origin's last view (§8.6): the broker has to say when
    a view closes.
  - `document.cookie` for the converter.
  - "Forget this site" in the trusted UI.
  - ~~The Navigator's broker issuing tickets.~~ Built 2026-11-06 in `navigator-cookies`:
    a ticket per request of a cookied view, sent through the courier for its
    (partition, origin), with redirects followed by the broker (legacy-web §5.4.1e,
    "Built"). `scripts/arca-vm-test.sh` §7 runs the broker's own clients through jailed
    couriers: 7/7 in the dev VM, 2026-11-06.
  - ~~**A teardown leak found by that run**~~ Fixed 2026-11-06. A courier jail that held
    TCP connections stays dying for 2 MSL (TIME_WAIT). The launcher used to wait a fixed
    time and then give up, which left the jail's six mounts behind. Now a janitor retries
    each exited courier's teardown every 2 s with no deadline, and reports one still
    pending after 5 minutes. The uid goes back to the pool only once nothing of its jail
    is left. Leftovers from a previous launcher go to the janitor at start instead of
    blocking it. `arca-vm-test.sh` now also checks, before stopping the launcher, that
    every courier jail of the run is gone with no mount or root left: 79/79.
  - Password fill by form ticket and gesture (§8.3) comes with A3.

## 9. Native apps and other clients

The same API covers apps that are not browsers:

| use | handover |
|---|---|
| an Insula app signing in to its service | per-service keypair, challenge-response ([insula.md](insula.md) §13.3); key class |
| an app calling a REST API with a token | slot-filled through a courier, or Arca mints a scoped short-lived token ([insula.md](insula.md) §13.5) |
| `git` over https, package registries | slot-filled through a courier (HTTP) |
| SSH, `git` over SSH, commit signing | an agent socket backed by Arca key operations, on the `ssh-agent` model |
| a mail or database client that must hold a password | app-held, with a user grant (§4.3), until a courier speaks its protocol |
| Loculus | its master key is an Arca item with a key-unwrap release to the Loculus app identity |

**The API** (replaces [vestibulum.md](vestibulum.md) §7.2, same shape):
`arca_mint`, `arca_sign`, `arca_decrypt`, `arca_derive` for keys; `arca_send` (courier) and
`arca_template_send` (slots) for bearer items; `arca_release` for app-held items, which fails
without a grant; `arca_store` for an app saving a secret it was given (the recipient is
required and shown to the user). There is deliberately no call that lists another app's
items or reads a value by name.

## 10. Audit

Every release and every operation is logged with the item id, client label, recipient,
time and whether a user gesture was present. Values are never logged. The trusted UI shows
it per item ("used by Navigator for mail.example, 14 times today"). Anomalies go to the
monitor (legacy web §10.5) and, where exact, end the client:

- a courier asks for an item whose recipient isn't its own (exact, so it terminates);
- items are used for an origin with no open view;
- a burst of verify-each-use requests.

## 11. Build plan

This replaces threat model §6.8's C0–C3 with a sequence in which every client benefits from
each step.

| phase | work | gate |
|---|---|---|
| **A0** | Arca daemon, store format, key hierarchy, classes, peer identity from the launch registry (§4.1); Vestibulum unlock and lock (ostiarius, which verifies the passcode, derives the root key at login and locks at sign-out). Atrium has no `vestibulumd` keychain to migrate: Arca is the keychain | a test app gets EACCES on another app's item; the store fails to decrypt a record moved between recipients; the item is unreadable after lock (while-unlocked) |
| **A1** | the courier for HTTP(S): fixed request schema, TLS-then-attach, no cross-endpoint redirects, Set-Cookie records; fuzz targets | a hostile-client fuzz run finds no read of an item; a redirect test proves no attach crosses endpoints |
| **A2** | the web profile: cookies, partitioning, SameSite with broker context, private credentialed conversions | **this is the gate for any login in the Navigator** (replaces C0) |
| **A3** | password secure input, slot-filled form POST, trusted save and fill UI | password login |
| **A4** | passkeys (Arca authenticator, broker WebAuthn client), DBSC, TPM sealing | phishing-resistant and device-bound sessions |
| **A5** | slot templates for app APIs and the Navigator's bindings; SSH agent; client certificates | native apps and SPAs with tokens |
| **A6** | sync and backup through Vestibulum; FedCM and `form_post` continuation | multi-device |

## 12. Decisions, with recommendations

**Accepted by the user, 2026-11-05: all seven recommendations below.**

1. **Arca is its own daemon, and Vestibulum keeps login and the root key.** *Recommend
   yes.* `vestibulumd` draws the login screen and takes input, so it parses events and
   fonts. Keeping item values out of a UI process, and the cookie traffic out of the login
   daemon, makes both smaller.
2. **Bearer secrets default to courier-attached; app-held only by user grant.** *Recommend
   yes.* It's the only way "only the intended recipient" survives an app compromise.
3. **Cookies stay usable while the screen is locked, but only for pages already open;
   passwords are while-unlocked.** *Recommend yes,* close to what iOS does.
4. **`document.cookie` exposes non-`HttpOnly` cookies to the origin's own script** (§8.1).
   *Recommend yes;* refusing it breaks many sites for no gain, since those cookies are
   script-readable by design.
5. **TPM sealing where present. Devices without one, such as the dev VM, are allowed, with
   a warning at setup and a high Argon2id cost.** *Recommend yes.*
6. **Passwords ship together with passkeys (phase A3 and A4 together), with passkeys
   offered first in the UI.** *Recommend yes;* most sites still need passwords.
7. **The name Arca.** If agreed, it goes into `docs/NAMING.md`.
