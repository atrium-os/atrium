# Mail: Atrium's email app

**Status: proposed 2026-11-15**, for Shankar's review. Nothing here is built.
**Asked by Shankar (2026-11-15):** "Let's build specs for an email app for atrium. What can
we do to make the email experience better in addition to all the secure features we do for
atrium?"

**Name.** Plain, per [NAMING.md](../NAMING.md)'s foundation-app rule: **Mail**, binary
`atrium-mail`, bundle `org.atrium.mail`. Its helper processes are `mail-*`.

Related: [arca.md](arca.md) (credentials, couriers), [nuntius.md](nuntius.md) (fetching),
[atrium-navigator-threat-model.md](atrium-navigator-threat-model.md) §2.3, §2.4, §2.10,
§2.11, §2.12 (hostile documents, images, links, deception), [limen.md](limen.md) §2.1
(`doc-viewer`), [opening-links.md](opening-links.md) (Monstrator), [opening-files.md](opening-files.md)
(attachments, quick look), [scrinium.md](scrinium.md) (picker), [folium.md](folium.md) and
[officina.md](officina.md) (attachments, rich text), [sigillum.md](sigillum.md) §10.2
(S/MIME), [personas.md](personas.md) (Work and Personal), [vpn.md](vpn.md) §9.3 (sign-in
pages, tunnels), [tabellarius.md](tabellarius.md) and [tabula.md](tabula.md) (push,
notifications), [insula.md](insula.md) §11 (background work),
[atrium-navigator-legacy-web.md](atrium-navigator-legacy-web.md) §11.8 (why a native mail
app beats webmail), [LICENSING-POLICY.md](../LICENSING-POLICY.md).

## 0. What makes it better, not only safer

Shankar asked for these first. Each is one line; the section that carries it is in the last
column. §17 D1 asks which go into the first release; my recommendation is the **v1** column.

| # | idea | what the person gets | v1? | § |
|---|---|---|---|---|
| E1 | **Undo send, send later, snooze, follow-up nudges** | a 10 s window to take a message back; send at 8:00 tomorrow; a thread disappears until Monday; "no reply in 3 days" brings it back | yes | §9.3, §11.4 |
| E2 | **Instant search that finds things inside attachments** | type and see results as you type, across all accounts of a persona, including text in PDFs and office files; a saved search becomes a folder | yes | §11.2 |
| E3 | **Conversations that read well, or a plain timeline** | per folder: threads or strictly chronological messages; quick filters (Unread, To me, Needs reply, Has attachment); optional grouping by date, sender or account; quoted text and signatures folded, a "new since you last looked" line, every file in the thread in one strip at the top | yes | §11.1, §11.7 |
| E4 | **One-press unsubscribe and a newsletter lane** | a real unsubscribe button (RFC 8058) on every list mail, newsletters read with the Navigator's reading settings (text size, dark, comfortable) | yes | §11.3 |
| E5 | **Quiet notifications** | alerts only for people and threads you are in; everything else waits for the times you pick ("9:00 and 17:00"); quiet hours per persona (no Work mail after 19:00); a sound per account | yes | §12 |
| E6 | **Screener for first-time senders** | mail from someone new waits in a "New senders" list until you press Accept once; nothing from them bothers you before that | yes, off by default | §11.3 |
| E7 | **Attachments as first-class things** | quick look without opening an app; "every file from Priya"; save straight into the right persona's folder; big files sent as a share link instead of 25 MB of base64 | yes (share links later) | §8 |
| E8 | **Setup with just an address** | type your address, sign in, done: servers found by autodiscovery; import mbox/Maildir; export everything any time | yes | §5.1, §6.4 |
| E9 | **Keyboard-first, offline-first, fast at a million messages** | every action has a key and a command palette; everything works on a plane; lists scroll smoothly at any size | yes | §15, §6 |
| E10 | **Lanes computed on the machine** | People / Updates (receipts, shipping, travel) / Newsletters, sorted by rules you can read and change, never by a cloud | later | §11.3 |
| E11 | **Cards for invites, receipts and trips** | a calendar invite shows Accept / Maybe / Decline in the message; flights and orders become a short card | invites v1, cards later | §14 |
| E12 | **Private reply addresses** | a different address per site or sender on your own domain, and replies go out from the address the mail came to | later | §13.3 |
| E13 | **Thread summaries, on this machine only** | a three-line summary of a 40-message thread, by a local model, never sent anywhere; opt-in | later | §11.5 |

The security work is in every section, but the list above is what a person notices in a
week of use. None of it needs a server of ours.

## 1. Short answer

- **Mail is a native, jailed Pergola app**, one instance per persona (Mail · Work,
  Mail · Personal). It never renders a webmail page and runs no script from any message.
- **It never holds a password or token.** IMAP and SMTP connections are made by an Arca
  **mail courier**: a jail per account endpoint that does TLS and authentication with the
  secret, then carries the session for Mail's sync process, refusing any command that would
  log in again. JMAP is HTTP, so the existing courier serves it unchanged.
- **Nothing that parses a hostile message has network.** A sync process (network only to
  its courier) moves raw messages into the local store; a **parse jail** with no network
  and no files decodes MIME, checks DKIM/DMARC, and turns HTML into an Atrium document;
  the Navigator's document worker shows it in a Limen `doc-viewer` slot.
- **Remote content is off.** No remote image, font or style is fetched unless the person
  asks, per message or per sender; then everything is fetched once through Nuntius, without
  cookies or referrer, and kept with the message, so reading it again tells nobody. Hidden
  pixels are never fetched at all.
- **Links show where they really go** (redirect wrappers and tracking parameters peeled
  off for display), and open through Monstrator on a press in Mail's own chrome.
- **Who sent it is shown from what was verified**: the domain that DKIM/DMARC proved, a
  first-time-sender note, look-alikes of people you write to, and a mismatched Reply-To.
- **Attachments never run.** They open through the opening-files rules (quick look, the
  type's default app, a narrowed descriptor) and save through Scrinium with their origin.
- **S/MIME signing and encryption** with keys on tokens (Sigillum) or in Arca; OpenPGP
  later with a permissively licensed library.
- **Everything is local-first**: a full offline copy, local search, local rules, local
  lanes. Mail talks to the person's own mail servers and to nothing else of anyone's.

## 2. What exists that Mail builds on

| need | existing piece | gap Mail adds |
|---|---|---|
| secrets | Arca: items with a bound recipient, couriers per endpoint (arca.md §5), app-held as a fallback (§4.3) | the mail courier: IMAP/SMTP session courier (§5.2) |
| fetching remote content | Nuntius: jailed, address-pinned, no LAN, `net-fetch-sites` | none; Mail's engine holds a Nuntius |
| showing HTML safely | `navigator-normalize`, the document profile, the jailed document worker, Limen `doc-viewer` | a `doc-viewer` load with resources supplied by the parent and a "no remote" mode (§7.3) |
| opening links | Monstrator, `LINK` presses, `[[links]] schemes = ["mailto"]` | Mail declares `mailto`; Mail sends `https` links to the Navigator (opening-links.md §7 "links into the Navigator" still to build) |
| attachments | opening-files (types, `[[opens]]`, quick look), Folium, Officina converters in throwaway jails | open from a descriptor that is not a file in a folder (§8.2) |
| saving | Scrinium picker, `EMIT_FD`, persona places | none |
| signing | Sigillum §10.2 `identity.certificate`, Arca keys (`arca_sign`, `arca_decrypt`) | S/MIME glue (§10) |
| work and personal | personas: an instance per persona, persona Arca, persona route | nothing new; Mail follows the instance model |
| notifications | Praeco + Forum toasts | notification actions (CLASS_BROKER delivery, not built) for Archive / Reply |
| background | insula.md §11 resident background | none; sync runs as Mail's resident background |
| rich text compose | `officina-text`, Tabula HTML subset | a compose profile (§9.1) |

The test mail app (`org.test.mail`, used by `navigator-confined-vm.sh`) shows the
`mailto:` path works end to end. Mail replaces it as the system's `mailto` app; installing
it still sets no default (opening-links.md §4.4) except on a fresh system where it is the
only mail app and Atrium ships it.

## 3. Threats

Who attacks a mail app, and what they want.

1. **A hostile message** (anyone can send one): code execution through MIME, charset,
   HTML, CSS, image or attachment parsers; or reading other mail through a bug.
   *Answer:* every parser runs in a parse jail or the document worker, one message at a
   time, no network, no files, no other message (§7.1). Mail's own process never parses
   message bytes.
2. **Tracking** (senders and their mail-service vendors): open pixels, unique remote image
   URLs, tracked links, read receipts. *Answer:* no remote fetch without asking; hidden
   images never; links shown peeled; MDN read receipts never sent automatically (§7.4).
3. **Phishing without a bug**: display-name spoofing, look-alike domains, "your password
   expires" pages, invoice fraud by a changed Reply-To. *Answer:* sender shown from
   verified facts, warnings drawn by Mail's chrome (never by the message), the destination
   of every link shown before it opens (§7.5, §7.6).
4. **Credential theft**: the mail password is the recovery path for every other account
   (legacy-web §11.8.2). *Answer:* the password or token lives only in Arca and the
   account's courier jail; Mail and its sync process never see it (§5.2).
5. **A hostile or compromised server** (the person's own provider, or a man in the middle):
   malformed IMAP responses, STARTTLS stripping, a fake server from autodiscovery.
   *Answer:* the IMAP/SMTP client parser lives in the sync jail, which holds only that
   account's mail; implicit TLS by default, STARTTLS only when required and never
   downgraded; autodiscovered servers are shown and confirmed when they are not under the
   address's own domain (§5.1).
6. **Attachments that run**: macros, scripts, shortcuts, disguised executables.
   *Answer:* opening-files §1: nothing runs from a file; Officina keeps macros inert;
   a type mismatch between the name and the bytes is shown (§8).
7. **Cross-persona leaks**: work mail forwarded to a personal address by mistake, a work
   attachment saved into personal folders. *Answer:* the instance model of personas.md;
   crossings need the persona switch and a press (§13).
8. **Mail as a spam cannon** (a compromised Mail sends mail as the person). *Answer:*
   sending needs a press in the compose window, except scheduled sends the person set up,
   and the courier rate-limits submissions (§9.4).

What this does not cover: the person's provider reading their mail (use S/MIME or
OpenPGP, §10), and a sender who writes a convincing lie in plain text.

## 4. Processes

```
               frescod ── Mail window (Pergola) ──┬── doc-viewer slot (Navigator document worker, jailed, no network)
                                                  └── pdf-viewer / quick look slots (Limen)
   atrium-mail (UI) ──── Aqueduct ────▶ mail-engine (store, index, rules; no network, parses nothing)
                                            │            │                 │
                                            │            │                 └─ Nuntius (remote images, autodiscovery, WKD), on request only
                                            │            └─ mail-parse (one per message batch; no network, no files; MIME, DKIM, HTML → document)
                                            └─ mail-sync (one per account; resident background; only socket: its courier)
                                                         └─ Arca mail courier (jail per endpoint; TLS + AUTH with the secret; relays the session)
```

| process | runs as | holds | network | parses hostile bytes |
|---|---|---|---|---|
| `atrium-mail` | the instance's app uid | the window, drafts being typed | none | no: gets parsed, bounded structures from the engine |
| `mail-engine` | same uid, own jail inside the app's tree | the store and the index | none (asks Nuntius) | no: stores raw blobs by hash, reads only its own index |
| `mail-sync` | a per-account uid | that account's protocol state | its courier socket only | server responses (IMAP/JMAP/SMTP), within one account |
| `mail-parse` | an ephemeral jail, per batch of one account's messages | the message bytes it was given | none | yes: all of MIME, headers, charsets, HTML, DKIM |
| document worker | the Navigator's `doc-viewer` provider | one converted document and its image blobs | none | yes: the document, images, fonts |
| mail courier | `_arca-courier` range (arca.md §5.4) | one endpoint's secret, after TLS | that endpoint's addresses only | the server's greeting and auth replies |

Why the engine is separate from the UI: the UI is the process that a bug in Pergola or a
pasted HTML snippet would reach first; the engine holds every message. Keeping them apart
means the window process can be restarted (or crash) without the store, and the store
process never handles input or drawing. It is one extra process per instance, not per
message.

`mail-parse` is per batch, not per message: a jail costs about 50 ms (threat model §2.4),
and sync brings thousands of messages at first. A batch is messages of one account only,
up to 64 messages or 32 MiB, so a hostile message can at worst misreport its batch
neighbours from the same account, which the sync process already holds. The engine
re-parses a message alone, in its own jail, when the person opens it (§7.1), so what the
person reads was never decoded next to another message.

## 5. Accounts and credentials

### 5.1 Adding an account

1. The person types their address (and nothing else, usually).
2. The engine looks up settings, in this order, through Nuntius and the system resolver:
   - JMAP session resource at `https://<domain>/.well-known/jmap` (RFC 8620 §2.2);
   - Mozilla-style autoconfig at `autoconfig.<domain>` and `<domain>/.well-known/autoconfig`;
   - SRV records `_imaps._tcp`, `_submissions._tcp`, `_jmap._tcp` (RFC 6186, RFC 8314);
   - the MX host's provider, matched against a small built-in table (Gmail, Microsoft 365,
     iCloud, Fastmail, Proton Bridge, Yahoo) for OAuth endpoints and quirks.
   Results not under the address's own domain or its MX's provider are shown ("Mail for
   you@example.org is handled by mail.hostingco.net") and need a press; the Thunderbird
   ISPDB is not fetched (it is a third party learning every address added).
3. Sign-in: a password typed into **Arca's credential field** (Limen `credential` slot),
   or an OAuth sign-in in a Navigator sign-in page (§5.3) for providers that
   need it. Mail never has the value.
4. Arca stores the item bound to the endpoint (`imaps://imap.example.org:993`,
   `submissions://smtp.example.org:465`, or `https://jmap.example.org`), under the
   instance's Arca persona. One item can be bound to both the IMAP and SMTP endpoints when
   they share a password; the binding list is shown in Arca's UI.
5. The first sync starts (§6.1), newest first, so the inbox is readable in seconds.

### 5.2 The mail courier (new Arca work)

Today an IMAP client must hold its password (app-held, arca.md §4.3) because the courier
speaks only HTTP. Mail would be the most valuable app-held secret on the machine, so this
spec proposes the courier learn the three mail protocols' login step (§17 D2):

1. `mail-sync` asks courierd for a courier to `imaps://imap.example.org:993`. The courier
   is labelled with that endpoint, the person and persona (arca.md §5.3–5.4).
2. The courier connects (address pinned, pf passes only that endpoint), completes TLS
   (implicit TLS on 993/465; STARTTLS on 143/587 only if the account says so, and then the
   courier refuses to continue without TLS), and checks the name.
3. **Only now** it asks Arca for the item and authenticates: `AUTHENTICATE PLAIN` (or
   `XOAUTH2` / `OAUTHBEARER`, refreshing the access token at the token endpoint through an
   HTTP courier first) for IMAP; `AUTH PLAIN` / `AUTH XOAUTH2` for SMTP submission.
4. On success it wipes the item and becomes a **relay**: plaintext protocol from
   `mail-sync` over a socketpair, TLS to the server. In the client direction it runs a small
   fixed parser that tracks command tags and literals (`{n}`, `{n+}`) and refuses
   `LOGIN`, `AUTHENTICATE`, `STARTTLS`, `UNAUTHENTICATE` (IMAP) and `AUTH`, `STARTTLS`
   (SMTP). Refused means the connection ends and arcad logs it as an exact anomaly
   (arca.md §10).
5. The session ends with the TCP connection. A new connection is a new login.

What a compromised `mail-sync` gets: an authenticated session to its own account, which it
already had. What it can't get: the password, a session for another account, or a login
from somewhere else later. A Google/Microsoft OAuth refresh token never leaves Arca either.

The relay parser is small, fuzzed and allocation-bounded (the same rules as arca.md §5.1's
request schema). It does not parse server responses beyond the greeting and the auth
reply.

JMAP needs none of this: it is HTTP with a bearer token or Basic, so `mail-sync` uses the
existing courier with a slot-filled `Authorization` (arca.md §4.3 class 2), and the push
channel (EventSource, RFC 8620 §7.3) is one long-lived courier request.

### 5.3 OAuth providers

Gmail and Microsoft 365 want OAuth 2.0 for IMAP/SMTP. Sign-in: authorization code with PKCE
in a Navigator sign-in page; the final redirect is caught by Arca's courier (as vpn.md §9.3
catches a SAML token), which exchanges the code at the token endpoint with the PKCE
verifier Arca generated. Mail sees "signed in" and the account name, nothing else.

**The catch** (§17 D4): Google lets a mail client use the `https://mail.google.com/` scope
only after verifying the client, including a yearly third-party security assessment.
Microsoft needs only an app registration (and tenant admin consent where an organisation
requires it). Until Google verification exists, Gmail works with an app password (needs
2-Step Verification on the account), which Arca stores like any password.

### 5.4 What Mail knows about an account

The address, display name, server names, which protocol, a colour, the persona, and
whether the courier last logged in. Nothing secret. Removing an account deletes its local
mail (crypto-erase once Tessera keys exist per persona) and its Arca items, after a press.

## 6. Sync, store and offline

### 6.1 Sync

- **IMAP**: IMAP4rev2 (RFC 9051) or rev1 with `CONDSTORE`/`QRESYNC` (RFC 7162) for cheap
  resync, `IDLE` for push, `UIDPLUS`, `MOVE`, `SPECIAL-USE`, `LITERAL+`, `OBJECTID` where
  offered. Without QRESYNC it falls back to UID ranges and flags diffs. `COMPRESS` not used.
- **JMAP** (RFC 8620, 8621): `Email/changes`, `Mailbox/changes`, push via EventSource;
  `EmailSubmission` with `FUTURERELEASE`-style holds where the server has them.
- **SMTP** submission (RFC 6409, RFC 8314): 465 implicit TLS preferred, 587 only with
  STARTTLS that the courier insists on.
- **Not supported**: POP3; Exchange ActiveSync and EWS (Microsoft retires EWS for
  Exchange Online; 365 accounts work over IMAP + OAuth); proprietary APIs. §17 D3.
- One `mail-sync` per account, run as Mail's **resident background** (insula.md §11.3) so
  new mail arrives with the window closed. With `IDLE`/EventSource the radio sleeps between
  messages; without either it polls every 5 minutes on mains, 15 on battery.
- Offline actions (move, flag, delete, send) are queued in the engine as ops and replayed
  in order; a conflict (message gone on the server) drops the op and says so in the
  activity line.

### 6.2 The store

- **Raw messages are the source of truth**, kept as blobs by content hash in the
  instance's data folder (Tessera CAS dedups the same mail in two folders or two
  accounts of one persona). Each is a byte-exact RFC 5322 message as the server gave it.
- **The index** (the engine's): per message the parse jail's bounded summary (§7.1): ids,
  folder, flags, dates, from/to/cc as parsed addresses, subject, a preview, the part tree,
  thread ids, authentication results, list headers, and the extracted text for search.
  Rebuilt from the raw blobs at any time by re-parsing in jails.
- **How much is kept offline**: everything by default; a per-account window (last year,
  last 3 months) for small disks, with headers for the rest and bodies fetched on open.
- **At rest**: in the persona's volume (personas.md §4), so a named persona's mail is
  encrypted with that persona's key once Tessera encrypts.

### 6.3 Size goals

- 1,000,000 messages in one account: list scroll at the display rate, a folder of 200,000
  opens under 100 ms, search results as you type under 50 ms for the first page.
- First sync of a 50,000 message account: the newest 500 readable within 10 s on a fast
  link; the rest in the background without making the UI wait.

### 6.4 Import and export

- **Import** mbox and Maildir from a file the person picks (Scrinium picker, read-only
  descriptor), parsed in parse jails, written into a local folder or uploaded to an
  account with `APPEND`.
- **Export** a folder, a search, or everything as mbox or Maildir through the picker's Save.
  Mail never makes it hard to leave.

## 7. Reading

### 7.1 Parsing a message

The engine hands a parse jail raw bytes and gets back, per message, a **bounded summary**
in a fixed postcard schema: headers it needs (decoded, RFC 2047, charset-converted to
UTF-8), addresses (RFC 5322 + RFC 6532), the MIME tree with part types, sizes and names,
the text parts as UTF-8, list and unsubscribe headers, iCalendar parts, and the
authentication result (§7.5). For a message being opened it also returns the HTML body
already converted into an Atrium document (below) and the `cid:` parts it refers to.

- Limits: 64 MiB per message, 1,000 MIME parts, nesting 32, header block 1 MiB, a line
  998 bytes for headers per RFC (longer ones are folded, not refused); refusing over that
  is shown as "This message is too large to show; you can save it."
- Charsets: the WHATWG encoding set (`encoding_rs`); unknown is shown as Latin-1 with a note.
- TNEF (`winmail.dat`) is unpacked in the same jail into its attachments.
- Libraries are memory-safe Rust and permissively licensed (`mail-parser` or own,
  `encoding_rs`, RustCrypto for DKIM); no C mail libraries.
- Anything the summary schema can't hold is dropped. The engine checks every length and
  count again on receipt and retires a jail that lies.

### 7.2 Plain text

`text/plain` (and `format=flowed`, RFC 3676) is drawn by Mail itself with Pergola: quote
levels as bars, links found by a conservative linkifier (scheme `https`, `http`, `mailto`
only), signatures after `-- ` folded. No document worker needed; most mail people write to
each other takes this path and it is fast.

When a message has both, the HTML part is shown by default (most senders only design that
one); a per-sender "prefer plain text" switch exists.

### 7.3 HTML

1. The parse jail runs `navigator-normalize` on the HTML part: no script (there is no script
   engine in this lane at all), no forms that submit, no `<meta refresh>`, no `<base>`
   pointing elsewhere, no iframes/objects/embeds, CSS limited to the document profile.
   Every remote reference (images, CSS `url()`, fonts, `background`) is rewritten to an
   opaque resource id and listed separately with its original URL.
2. The engine stores the converted document with the message, so opening it again does
   no work and fires nothing.
3. Mail requests a `doc-viewer` embed (Limen §2.1) and loads the document as a blob.
   **New in the role** (small, added at the end per Limen's versioning):
   - `Load` gains `remote: Never | Provided`: with `Never` the child fetches nothing, ever
     (it can't today anyway: the document worker has no network, the parent supplies blobs);
   - `Provide { id, blob }` gives the child a resource it may show (a `cid:` part, or a
     remote image the person allowed); unknown ids stay as placeholders of the declared size;
   - `LinkActivated` already exists and is used as is (§7.6).
4. Mail sets the theme. **Dark mode works on HTML mail** because the document was
   normalised: the worker can invert backgrounds and text it can reason about, and keeps
   images as they are. Reading settings (text size, comfortable width) come from the
   Navigator's reader controls.
5. Placeholders for blocked images keep the layout from jumping when they load.

### 7.4 Remote content and tracking

The Navigator's rule "viewing fires no tracker" (threat model §2.12) applied to mail, where
it matters more: a remote image in a mail is mostly there to learn *that you opened it,
when, and from where*.

- **Default: nothing remote is fetched.** A bar in Mail's chrome (not in the message) says
  "12 pictures not loaded" with **Load pictures** and **Always for this sender**.
- **Never fetched, even after Load**: images the parse jail marks as hidden or tiny
  (≤ 3 px either side, `display:none`, zero opacity, off-canvas, 1×1 by declared size), and
  remote CSS and fonts (layout uses the system fonts and the inline CSS only).
- **When the person loads pictures**: the engine fetches them all at once through its
  Nuntius, with no cookies (Nuntius never has any), no `Referer`, an honest generic agent
  string, no client hints. Results are decoded only in the document worker and **kept with
  the message**, so a second view, a reply or a search fetches nothing.
- **Per sender** "always load" applies only when that sender passes DMARC (§7.5), so a
  forged From can't turn on loading.
- **Option, off by default**: load pictures through a Crypta tunnel (vpn.md) so the
  sender sees the tunnel's address, not the person's.
- **Read receipts** (`Disposition-Notification-To`, MDN) are never sent automatically;
  the person can press "Send receipt" in the bar.
- **Calendar invites and `List-Unsubscribe` URLs** are never fetched to "preview" them.

Not done, and why: fetching every image at arrival through a relay (what some platforms do
to blur open times) needs a relay service someone runs; Atrium doesn't have one, and fetching
from the person's own address at arrival tells the sender that the mailbox is live.
§17 D5 asks whether a Crypta tunnel should be the default when one exists.

### 7.5 Who sent it

What Mail shows comes from what was checked, never from what the message says about itself.

- **DKIM** (RFC 6376) verified in the parse jail, with the keys looked up by `mail-sync`
  through the system resolver (only `TXT` for `<selector>._domainkey.<domain>` and
  `_dmarc.<domain>`, DNSSEC-validated by local-unbound where signed). **DMARC** alignment
  (RFC 7489) and **ARC** (RFC 8617) from forwarders the person trusts. For accounts where
  the provider adds `Authentication-Results` (Gmail, 365, Fastmail), the topmost one whose
  `authserv-id` is the account's provider is used when our own check can't run (key gone).
- **The header** shows the name, and always the verified domain next to it: "Priya Nair ·
  example.com ✓". A failing or missing check shows the address in full with "not verified".
  A display name that contains an address or a domain different from the real one is shown
  as the real address only.
- **First contact**: "First message from this address" when the persona has never
  received from or sent to it.
- **Look-alikes**: a sender whose domain is confusable (UTS #39 skeleton) or one edit away
  from a domain the person corresponds with gets "Looks like example.com, but is
  examp1e.com", in Mail's chrome. Internationalised domains are shown as punycode when they
  mix scripts (Monstrator's rule).
- **Reply-To** that differs from From's domain is shown in the reply header: "Replies go to
  billing@other.example".
- **Not BIMI** for now: logos need certificate checks and fetches for decoration; the
  verified domain carries the same information.

### 7.6 Links

- A press on a link in the body comes to Mail as `LinkActivated { url }` (Limen, only after
  a press in the child). Mail shows a **link bar** under the message: the destination's
  scheme and registrable domain in bold, the path in light text, *Open* (Enter) and *Copy*.
  The press on *Open* is Mail's `LINK` press for Monstrator (opening-links.md §5), so the
  Navigator (or the `mailto` handler, which is Mail) opens it.
- **Peeled for display**: known redirect wrappers (Microsoft Safe Links, Proofpoint
  URLDefense, Mimecast, Google `/url?q=`, common ESP click trackers whose target is in the
  query) are unwrapped to show the real destination. Tracking parameters (`utm_*`, `fbclid`,
  `gclid`, `mc_eid` and a short list) are removed from the link that opens. In a named
  persona with "keep company link scanning" on (the default for Work), the wrapped link is
  what opens; the person still sees the real destination.
- **Mismatch**: a link whose visible text is a URL or domain different from its destination
  shows both in the bar, the destination first.
- §17 D6: whether links from verified known senders open on the first press, without the bar.

### 7.7 Messages Mail keeps out

Spam is the server's job; Mail shows the server's Junk folder and teaches it (moves to Junk
set `$Junk`/`$NotJunk` keywords where supported). Mail adds its own warnings (above) but
doesn't run a spam classifier of its own in v1.

## 8. Attachments

### 8.1 Seeing them

- Shown as a strip with name, type icon (SVG, from the types table), size, and the type
  detected from the bytes (atrium-types, opening-files T1). A name that disagrees with the
  bytes ("invoice.pdf" that is a ZIP with a script) is shown as what it is.
- **Quick look** in a Limen slot (pdf-viewer for PDF via Folium, doc-viewer for HTML and
  text, Officina converters for office files, the picture viewer for images, Player for
  audio/video with no network), in Mail's window. The slot gets a read-only sealed memfd,
  not a path.
- **All files in a thread** at the top of the conversation; **all files from a person** and
  a Files view across the persona's mail (a search with `has:attachment` shown as a grid).

### 8.2 Opening and saving

- **Open in…** the type's default app: portcullisd's open path takes a sealed, read-only
  descriptor of the attachment from Mail with a press (new verb, `OpenAttachment`,
  §16 step M5) and hands it to the app as opening-files §6.2 does for a file from Scrinium.
  The app can't learn where it came from beyond its name.
- **Save** goes through Scrinium's picker (Save target, persona places only), with the
  origin written to `atrium.origin` ("mail: from priya@example.com, 2026-11-15") like a
  Navigator download.
- **Nothing runs**: executables, scripts, shortcuts, disk images and installers are shown
  and savable but no "Open" is offered (opening-files §1). An Atrium app bundle opens the
  installer, which checks its signature.
- **Encrypted ZIPs and PDFs** ask for their password in Arca's field (Officina O7a's path).

### 8.3 Sending big files

Over a size the account allows (from `SIZE` / JMAP `maxSizeUpload`, default 20 MB), compose
offers "Send as a link" to a network share the person set up (network-shares.md: WebDAV,
SMB, SFTP) or a JMAP blob link where the server supports it. Later, not v1 (E7).

## 9. Writing

### 9.1 Compose

- A compose window per message (a Pergola window, so several drafts side by side), or
  inline in the thread for replies.
- Rich text uses `officina-text` with a **mail profile**: paragraphs, bold/italic/underline,
  links, lists, quotes, inline images, simple tables; sent as `multipart/alternative` with a
  plain text part generated from it. A "plain text only" setting per account and per
  recipient.
- **Recipient field**: completion from the persona's address book (§14.2); a recipient
  outside the account's domain in a Work persona is coloured; replying to a
  first-contact or look-alike sender shows the same note as on reading.
- **Drafts** saved every few seconds to the engine (and to the server's Drafts folder), so a
  crash loses nothing.
- **Attachments** from the Scrinium picker (read-only descriptor), drag and drop (Tabula),
  or the clipboard. Forgotten-attachment check: "attached" in the text with no attachment
  asks once.
- `mailto:` links arrive from Monstrator as `LINK` on Mail's document channel and open a
  compose window filled from the link. Monstrator already shows scheme only; Mail shows the
  whole prefilled message before anything is sent and never sends from a link. A `body=`
  that is long or has hidden characters is shown, not hidden.

### 9.2 Identities

Per account one or more From addresses (aliases), signatures per identity, and **reply from
the address the mail was sent to** (E12) by default.

### 9.3 Undo send and send later

- **Undo**: a sent message waits in the engine for 10 s (setting: 5–30 s) before the
  courier submits it; *Undo* puts it back into compose.
- **Send later**: on JMAP servers with submission holds, or SMTP servers that offer
  `FUTURERELEASE` (RFC 4865), the hold is on the server, so the machine can be off.
  Otherwise the engine keeps it and Mail's resident background sends it on time; if the
  machine was off it asks on next start: "2 messages were due while this computer was off.
  Send now?"
- Every send is logged in the activity line with the time it really left.

### 9.4 Sending is a press

A message leaves only on a press of *Send* in its compose window (frescod activation token,
spent by the engine), or because the person scheduled it. The mail courier caps submissions
per account (60 an hour by default, raisable in settings) so a compromised Mail can't spam
the person's contacts quietly; going over shows a notice.

## 10. Signing and encryption

### 10.1 S/MIME (v1)

- **Signing** with a certificate on a smartcard or token through Sigillum §10.2
  (`identity.certificate`: chooser and PIN each time, or the card session decision 7
  allows), or a software certificate Arca holds (`arca_sign`). The persona's tokens only.
- **Encrypting** to recipients whose certificates Mail has seen in their signed mail or in
  the address book; the session key is wrapped per recipient and for the sender.
- **Decrypting**: the parse jail finds the recipient info, Arca or the card unwraps the
  content key (`arca_decrypt`, or the card through Sigillum), and the parse jail decrypts
  the content with it. Neither Mail nor the parse jail sees a private key.
- **Verifying**: signature and chain checked in the parse jail against the system trust
  store and the person's added CAs; result shown in the header ("Signed by Priya Nair,
  certificate from Example CA"), never as a padlock drawn by the message.
- Decrypted messages are stored encrypted (as received) and decrypted again on view; the
  search index holds their text only in a named persona's encrypted volume, or not at all
  (setting, default: index).
- Libraries: RustCrypto `cms`, `x509-cert`; no OpenSSL.

### 10.2 OpenPGP (later)

With `rpgp` (MIT/Apache; Sequoia is LGPL and out per LICENSING-POLICY.md), keys in Arca
(key class, use-not-read), Autocrypt headers and WKD lookups (through Nuntius) for finding
keys, and an "encrypt when every recipient has a key" switch. §17 D7.

### 10.3 What the person sees

One line under the subject: *Signed by…*, *Encrypted*, *Signature not valid*, or nothing.
Encryption in compose is a lock toggle that is available only when every recipient has a
key, with the reason when it isn't.

## 11. Organising

### 11.1 Conversations

- Threads by `References`/`In-Reply-To`, falling back to subject within 14 days; JMAP
  `threadId` when the server has it. Splitting and merging by hand.
- Quoted text below the new text is folded with a "show quoted" line; signatures and
  long legal footers folded too.
- "New since you last looked" line in long threads; jump to it on open.
- A thread's files at the top (§8.1).

### 11.2 Search

- Local full-text index in the engine (a permissively licensed Rust index such as
  `tantivy`, or own), over headers, text, and **text extracted from attachments** in jails
  (Folium for PDF, Officina converters for office files; images not OCR'd in v1).
- Operators: `from:`, `to:`, `subject:`, `has:attachment`, `filename:`, `before:`/`after:`,
  `is:unread`, `in:`, `larger:`; plain words otherwise. Results as you type.
- **Saved searches** show as folders (smart folders), per persona.
- Server-side search (IMAP `SEARCH`, JMAP `Email/query`) only for mail outside the offline
  window.

### 11.3 Senders and lists

- **One-press unsubscribe**: `List-Unsubscribe-Post` (RFC 8058) is done as a POST through
  Nuntius with no cookies, after a press; a `mailto:` unsubscribe opens a prefilled
  compose; a web-only one opens in the Navigator. Afterwards "Block future mail from this
  list" moves later ones to Trash.
- **Newsletter lane**: mail with `List-Id` / `List-Unsubscribe` from senders the person
  hasn't written to; read with reading settings; one view of "all newsletters, newest
  first", independent of folders.
- **Screener** (E6, off by default, offered at setup): mail from addresses the persona has
  never received from or written to goes to *New senders*; Accept once moves the sender's
  mail to the inbox and remembers; Refuse files future mail under *Screened out*. The
  screener is a local rule; nothing bounces.
- **Lanes** (E10, later): People / Updates / Newsletters from readable local rules over
  headers (`List-Id`, `Precedence`, known transactional senders, schema.org types), editable,
  never a cloud classifier.

### 11.4 Snooze and follow-up

- **Snooze** a thread to a time ("tomorrow morning", "next week", a date). Stored as a local op plus an IMAP keyword
  (`$Snoozed`) and a hidden folder so other clients see it gone; JMAP the same with
  keywords.
- **Follow-up**: on send, "remind me if nobody replies in N days"; the thread comes back
  to the top with a note.

### 11.5 Local assistance (later, opt-in)

Thread summaries and "draft a reply" by a model that runs on this machine only, if Atrium
has one; nothing leaves the machine; off by default; never on encrypted mail unless the
person turns that on. §17 D1 lists it as later.

### 11.6 Rules

Local rules (from/to/subject/list → folder/flag/label/notify) in the engine, and
**ManageSieve** (RFC 5804) for servers that have it, so rules run when the machine is off.
Vacation replies through Sieve or JMAP `VacationResponse`.

### 11.7 Views: conversations or messages, filters, grouping (Shankar 2026-11-16)

- **Conversations or messages, per folder.** A toggle in the list header: *Conversations*
  (one row per thread, sorted by its newest message, with a count and every participant)
  or *Messages* (one row per message, strictly chronological). Default: Conversations for
  the inbox and ordinary folders; Messages for Sent, Drafts, Trash, Junk and the
  newsletter lane, where threads add nothing. The choice is remembered per folder and per
  saved search. A conversation shows messages from every folder (Sent included) unless
  "this folder only" is on.
- **Sort**: newest first (default), oldest first, sender, subject, size; unread first as
  an option.
- **Quick filters**: chips above the list, combinable: *Unread*, *Flagged*, *To me*
  (I'm in To, not Cc or a list), *From people* (not lists or automated senders), *VIPs*,
  *Has attachment*, *Needs reply* (I was asked something in the last message and haven't
  answered: a thread whose last message is to me and not from me), *Snoozed back*. Typing in
  the search field narrows the same list; any filter set can be saved as a smart folder
  (§11.2).
- **Grouping** (optional, off by default): section headers by *Date* (Today, Yesterday,
  This week, Earlier this month, by month after), *Sender* (all of one person's mail
  together, newest sender first), *Account* (in the unified inbox), or *Lane* (People /
  Updates / Newsletters, once lanes exist, E10). Sections fold, and a folded section keeps
  its unread count.
- **Everything is local**: filters, grouping and sort run in the engine's index, so they
  are instant and work offline; nothing is written to the server except what the person
  does to messages.

## 12. Notifications

- Through Praeco, drawn by Forum (tabula.md / praeco placement). Mail posts only sender
  name, verified-domain mark and subject, never body text, on the lock screen unless the
  person allows previews.
- **Quiet by default** (E5): notify for mail from people (not lists, not automated
  senders) and for threads the person has written in; everything else is counted, and a
  digest notification comes at the person's chosen times.
- **VIPs**: always notify (except in quiet hours, below, unless the person lets them through).
- **Sounds per account** (Shankar 2026-11-16): each account, and VIPs, can have its own
  sound, chosen from the system's set of notification sounds (or none). Mail names the sound
  in the Praeco post; Praeco/Forum play it through Lyra at the notification volume. Mail
  never ships or plays a sound file of its own, and a message can't pick one.
- **Quiet hours per persona** (Shankar 2026-11-16): a schedule per persona, for example
  Work: weekdays 9:00–19:00, so no Work mail notifies in the evening or at weekends.
  Outside the schedule mail still syncs and the badge count still updates, but nothing
  pops up or sounds; what arrived is in one summary at the start of the next working
  period. Switches: "let VIPs through" (off by default) and "let replies in my threads
  through" (off). The schedule is kept in the instance's settings and shown in Curia's
  notification page beside Atrium-wide Do Not Disturb, which always wins.
- **Actions** (Archive, Mark read, Reply) when Praeco notification actions exist
  (CLASS_BROKER delivery, not built); a press on the notification gives the press Mail needs.
- Per persona; the persona badge comes from Forum (personas.md §7), not from Mail.

## 13. Accounts across personas

### 13.1 One instance per persona

Following personas.md: Mail · Personal and Mail · Work are separate instances with
separate stores, Arca personas, routes (a Work VPN applies to Work mail) and tokens. The
unified inbox covers the accounts of one persona only. A person with only Personal sees
none of this.

### 13.2 Crossing

Forwarding a Work message to a Personal account's address is not blocked (it's mail), but
Mail shows "This goes outside Work" when the recipient isn't in the Work persona's known
domains. Dragging a Work attachment into a Personal app goes through the persona crossing
rules (off by default).

### 13.3 Private reply addresses (later)

With a custom domain that has a catch-all, Mail can make an address per sender or site
(`shop-xyz@me.example`), remember it, and reply from it. Hosted alias relays (the "hide my
email" kind) are Loculus's aliased-identity item (loculus.md), out of scope here.

## 14. Calendar invites and people

### 14.1 Invites

`text/calendar` parts (iTIP, RFC 5546) are parsed in the parse jail into a bounded card:
title, time in the person's zone, organiser, location as text, attendees count, and
conflicts with other invites Mail knows about. *Accept / Maybe / Decline* sends the iTIP
reply by mail. Adding to a calendar waits for a calendar app (CalDAV), which is its own
spec. No URL in an invite is fetched.

### 14.2 People

Mail keeps a per-persona **address book**: addresses the person wrote to or replied to,
plus imported vCards, with S/MIME certificates seen and "always load pictures" choices.
CardDAV sync and a Contacts app are a separate spec (§17 D8).

### 14.3 Structured cards (later)

schema.org JSON-LD in HTML mail (orders, flights, reservations) parsed in the parse jail
into small fixed cards, shown only when DMARC passes for the sender's domain.

## 15. Keyboard, accessibility, layout

- Three panes (folders, list, message) that collapse to two and one as the window narrows;
  all sizes in points (resolution-independence.md).
- Every action has a key; a command palette (Ctrl-K) for the rest; the common single-key
  set (j/k next/previous, e archive, r reply, a reply all, f forward, s star, / search,
  # delete, z undo) on by default with a switch.
- Undo for every list action (archive, delete, move) for 10 s.
- Pergola accessibility tree for every pane; the doc-viewer child's tree joins it through
  Limen (limen.md §9).

## 16. Pieces and build order

| piece | where |
|---|---|
| mail courier (IMAP/SMTP auth + relay, command filter, submission cap) | `arca/` (courierd, new courier kind) |
| `mail-proto`: IMAP, SMTP, JMAP client state machines (sans-I/O, fuzzed) | new crate `mail/mail-proto` |
| `mail-parse`: MIME, charsets, DKIM/DMARC/ARC, TNEF, iTIP, HTML via `navigator-normalize`, summary schema | `mail/mail-parse` + jail runner |
| `mail-engine`: store, index, ops queue, rules, search, Nuntius client | `mail/mail-engine` |
| `mail-sync` | `mail/mail-sync` |
| `atrium-mail` UI | `atrium-mail/` |
| `doc-viewer` additions (`remote`, `Provide`) | `limen/`, navigator document worker |
| `OpenAttachment` | portcullisd, atrium-open |
| autodiscovery | `mail/mail-engine` |

Build order (each step has a VM gate, `scripts/mail-vm.sh`, against a local Dovecot or
Stalwart in the cloud VM, and later a real account):

1. **M0** `mail-proto` and `mail-parse` on the host: IMAP/SMTP/JMAP codecs, MIME and the
   summary schema; fuzz targets; a corpus of real and hostile messages (the usual MIME
   torture tests, charset traps, header bombs).
2. **M1** the mail courier in Arca (auth, relay, filter), against a local IMAP/SMTP server;
   test that `LOGIN` after auth kills the session and is logged.
3. **M2** engine + sync: read-only IMAP sync into the store, the index, plain-text reading in
   a minimal Pergola window; jailed parse batches.
4. **M3** the full UI: three panes, conversations, HTML through `doc-viewer`, remote content
   bar, sender line, link bar through Monstrator, keyboard set. Mail becomes the `mailto`
   app.
5. **M4** compose and send: rich text, drafts, undo send, send later, identities, `mailto:`.
6. **M5** attachments: quick look, `OpenAttachment`, Save via picker, attachment search
   text.
7. **M6** search, smart folders, unsubscribe, newsletter lane, screener, snooze, follow-up,
   notifications.
8. **M7** S/MIME with Sigillum tokens and Arca certificates; JMAP; OAuth (Microsoft first).
9. **M8** later items: OpenPGP, lanes, cards, private addresses, big-file links, local
   assistance.

## 17. Decisions, with recommendations

| # | question | recommendation |
|---|---|---|
| D1 | Which experience ideas (§0) go into the first release | E1–E9 (E6 off by default, E7 without share links, E11 invites only); E10, E12, E13 and cards later |
| D2 | How Mail authenticates to IMAP/SMTP | A new Arca **mail courier** that logs in and relays the session (§5.2), so the password never reaches Mail. The alternative, app-held (arca.md §4.3), is simpler but makes the mail password the weakest-held secret on the machine |
| D3 | Protocols | IMAP + SMTP submission and JMAP. No POP3, no Exchange ActiveSync/EWS; Microsoft 365 over IMAP + OAuth; revisit Microsoft Graph only if Microsoft turns IMAP off |
| D4 | Google and Microsoft OAuth | Register an Atrium client with Microsoft now; start Google's verification for the mail scope (yearly security assessment) but don't wait for it: Gmail works with an app password meanwhile |
| D5 | Remote pictures | Off by default; load per message or "always for this sender" (DMARC-passing only); fetched once through Nuntius with no cookies or referrer and kept; hidden pixels never. Through a Crypta tunnel only if the person turns that on |
| D6 | Opening links from mail | Every link shows the link bar with the real destination, and the press there opens it. Not one-press for known senders in v1, since the bar is also what makes the press Mail's |
| D7 | Encryption | S/MIME in v1 (Sigillum tokens and Arca certificates already exist); OpenPGP later with `rpgp`, not Sequoia (LGPL) |
| D8 | Contacts and calendar | Mail keeps a small per-persona address book and answers invites by mail; Contacts (CardDAV) and Calendar (CalDAV) are separate apps and specs later |
