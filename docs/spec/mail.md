# Mail: Atrium's email app

**Status: accepted 2026-11-16** (Shankar: "Accept as recommended", all of §17 D1–D8 as
recommended, after review additions §6.4, §11.4, §11.7, §12, §12.1, §15). M0 built (codecs, sender checks, HTML, jail format); M1 mail courier built on the host; M2 engine, sync and a minimal window built on the host; M3 full UI built on the host; M4 compose and send built on the host; M5 attachments built on the host; M6 next.
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
| E1 | **Undo send, send later, and *Later* that never rots** | a 10 s window to take a message back; send at 8:00 tomorrow; set a thread aside for Reply / Read / Do / Waiting with a time it comes back, auto-closed when you reply or they do, a daily review so nothing flagged is forgotten | yes | §9.3, §11.4 |
| E2 | **Instant search that finds things inside attachments** | type and see results as you type, across all accounts of a persona, including text in PDFs and office files; a saved search becomes a folder | yes | §11.2 |
| E3 | **Conversations that read well, or a plain timeline** | per folder: threads or strictly chronological messages; quick filters (Unread, To me, Needs reply, Has attachment); optional grouping by date, sender or account; quoted text and signatures folded, a "new since you last looked" line, every file in the thread in one strip at the top | yes | §11.1, §11.7 |
| E4 | **One-press unsubscribe and a newsletter lane** | a real unsubscribe button (RFC 8058) on every list mail, newsletters read with the Navigator's reading settings (text size, dark, comfortable) | yes | §11.3 |
| E5 | **Quiet notifications** | alerts only for people and threads you are in; everything else waits for the times you pick ("9:00 and 17:00"); no storm of pings after a network drop or sleep, one silent catch-up summary instead; quiet hours per persona (no Work mail after 19:00); a sound per account | yes | §12 |
| E6 | **Screener for first-time senders** | mail from someone new waits in a "New senders" list until you press Accept once; nothing from them bothers you before that | yes, off by default | §11.3 |
| E7 | **Attachments as first-class things** | quick look without opening an app; "every file from Priya"; save straight into the right persona's folder; big files sent as a share link instead of 25 MB of base64 | yes (share links later) | §8 |
| E8 | **Setup with just an address, usable in seconds** | type your address, sign in, done: servers found by autodiscovery; the inbox usable in seconds while the rest fills in politely, no gigabyte downloads; import mbox/Maildir; export everything any time | yes | §5.1, §6.4, §6.5 |
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
5. The first sync starts (§6.4): the inbox is readable in seconds, and the rest fills
   in behind without slowing anything.

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
6. **What mail-sync sees first** (fixed in M0): for IMAP, the courier sends
   `* PREAUTH [CAPABILITY …] …` with the capabilities the server announced after login, so
   the session looks pre-authenticated; for SMTP, the server's post-login `EHLO` reply.
   `mail-proto` has no LOGIN, AUTHENTICATE, STARTTLS, COMPRESS, AUTH or EHLO of its own.

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
  offered. Without QRESYNC it falls back to UID ranges and flags diffs. `COMPRESS` is not
  used: the courier's command filter (§5.2) must read every command.
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
- **How much is kept offline**: by default, the list entry (headers, flags, preview) for
  every message, message text for the last 90 days, and attachments only once opened
  (then cached, within a per-account limit). *Keep everything offline* is a switch, and
  then it fills in only under the §6.4 rules. See §6.4 for how a new account fills in.
- **At rest**: in the persona's volume (personas.md §4), so a named persona's mail is
  encrypted with that persona's key once Tessera encrypts.

### 6.3 Size goals

- 1,000,000 messages in one account: list scroll at the display rate, a folder of 200,000
  opens under 100 ms, search results as you type under 50 ms for the first page.
- First sync of any account, even 50 GB on the server: the inbox's newest messages
  listed within 5 s and readable as soon as opened, after under 5 MB of download; the
  machine stays responsive throughout (§6.4).

### 6.4 First sync of a new account (Shankar 2026-11-16)

Shankar: "Normal clients download gigabytes and slow to a crawl." They do because they
treat a new account as a copy job: every folder, every message, every attachment, as fast
as possible, with the UI waiting behind it and the index rebuilt as it goes. Mail treats it
as **making the account usable first, then filling in what's worth having, politely**.

**Stage 1: usable in seconds (foreground, small).**
- Folder list, then the **inbox only**: for the newest 200 messages, flags, envelope,
  structure and size (IMAP `FETCH (FLAGS ENVELOPE BODYSTRUCTURE RFC822.SIZE)`, plus the
  server's `PREVIEW` (RFC 8970) where offered; JMAP `Email/query` + `Email/get` with
  `preview`). About 1–2 KB a message, so well under 1 MB.
- These go through a parse jail like everything else (envelopes are hostile text too).
- The list shows at once; opening a message fetches its text parts (not attachments) on a
  connection kept free for the foreground, so a click is never queued behind bulk work.

**Stage 2: headers for everything (background, light).**
- List entries for the rest of the inbox, then Sent, then other folders by recent
  activity, newest first, in pages of 500. A message's headers are a few KB; 100,000
  messages is a few hundred MB at most, usually far less.
- **Each message once**: where the server gives stable ids (IMAP `OBJECTID` `EMAILID`,
  Gmail `X-GM-MSGID`, JMAP ids), a message in several folders or labels is fetched once.
  On Gmail, Mail syncs *All Mail* once and reads labels (`X-GM-LABELS`), instead of
  downloading every message again for each label, which is where most of the gigabytes
  in other clients come from.
- Search works from the start: on local headers, plus server-side search
  (IMAP `SEARCH`, JMAP `Email/query`) merged into the results for anything not yet local,
  marked "on server".

**Stage 3: message text for the recent window (background, idle-time).**
- Text parts only (`BINARY.PEEK[part]` / `BODY.PEEK[part]` for text parts, JMAP
  `bodyValues`), last 90 days, newest first. Attachments are not fetched. Text is usually
  a few percent of an account's size; attachments are most of it.
- Indexed for search in small batches as it arrives.

**Stage 4 (only with *Keep everything offline*): the rest.**
- Older text, then attachments, only on mains power, on a network atrium-net doesn't mark
  metered, while the machine is idle; paused otherwise.

**Being polite to the machine and the network:**
- **The UI never waits on sync.** The engine serves the window from what is stored; sync
  writes in small transactions; index updates are batched and incremental.
- **Low priority everywhere**: sync and its parse jails run in the low-weight background
  class (insula.md §11), so typing, scrolling and other apps win. Bulk downloads use at
  most 2 connections (plus one for the inbox's IDLE and one kept for the foreground),
  well under provider caps (Gmail allows 15), and back off on `[THROTTLED]`, `[LIMIT]` or
  slow responses.
- **Bandwidth**: background sync yields when anything in the foreground is using the
  network, and has a ceiling (default: half of measured throughput); on a metered network
  only stages 1 and 2 run.
- **Battery**: stages 3 and 4 wait for mains unless the person says otherwise.
- **Resumable**: progress is saved per folder (UID or state checkpoints), so sleep, a
  dropped network or a restart continues where it stopped.

**What the person sees:**
- Before anything big: "This account has 48 GB on the server. Mail will keep about
  1.2 GB here: everything listed, the last 3 months' text, attachments when you open them."
  with *Change* (window, keep everything) and nothing to confirm otherwise.
- A quiet line under the folder list while it fills in: "Getting older mail: back to
  March 2021 (12,400 of 48,000)", with *Pause*. No spinner blocks anything.
- Opening an old message or attachment that isn't local fetches just that one, at once.

### 6.5 Import and export

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
- **No temporary files** (Shankar 2026-11-18): other mail clients write an opened
  attachment to a temp folder and leave it there. Mail never writes one: opening and quick
  look hand over a sealed memfd, which is gone when the app closes it. An app that edits it
  saves a copy through the picker; the memfd can't be written.
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

### 11.4 Later: flags, snooze and reminders that don't get forgotten (Shankar 2026-11-16)

Shankar: "flags, reminders, look later in a way that we won't mark them and forget about
them." The usual failure is a flag with no date: it is easy to set, nothing ever brings it
back, and after a month the flagged folder is a second inbox nobody reads. So in Mail
**nothing set aside is without a time it comes back**, and **anything set aside has to be
closed, not just left**.

**One action, *Later*** (key `l`), replaces flag, star, snooze and pin. It asks two things
in one small sheet, both with defaults so it is one press when you're in a hurry:

- **What for**: *Reply*, *Read*, *Do*, or *Waiting* (for someone else). Default: *Reply*
  when the last message is to me, *Read* for lists and newsletters.
- **When**: *This evening*, *Tomorrow morning*, *This weekend*, *Next week*, a date, or
  *Someday*. *Someday* is still a date: 30 days out, when it asks again.
  An optional one-line note ("send Priya the numbers") is shown when it comes back.

**What happens:**

- The thread leaves the inbox (unless "keep in inbox" is on for that kind) and goes to the
  **Later** list in the sidebar: *Overdue*, *Today*, *Upcoming*, *Waiting*, by date. Its
  count is always shown; overdue ones are marked.
- **At its time it comes back** to the top of the inbox with its note and when it was set
  aside ("Set aside Mon: send Priya the numbers"), and a notification if the person wants
  one. Quiet hours apply (§12): a Work item due on Saturday comes back on Monday morning.
- **Closing is automatic where Mail can tell**: a *Reply* item closes when you send a
  reply in that thread; a *Waiting* item closes when someone replies (the follow-up of
  §9, "remind me if nobody replies in N days", is a *Waiting* item set on send); a *Read*
  item closes when the message has been open and scrolled to its end. *Do* items close
  only with *Done* (key `d`).
- **New mail in a set-aside thread** brings it back at once if the new message is to me
  and from a person; otherwise it waits for its date.

**So they don't rot:**

- **Daily review** at a time the person picks (default: the start of the persona's
  working hours, or 9:00): one notification, "3 due today, 2 overdue", which opens the
  Later list with one-key answers on each row: *Done* (`d`), *Later again* (`l`, with the
  same choices), *Drop* (`x`, no longer needed). Nothing else is asked.
- **Pushed-back count**: an item moved again three times is marked "pushed back 3 times"
  and the review asks *Do it now*, *Drop*, or *Make it someday*. No silent infinite snooze.
- **Overdue items don't pile up quietly**: after 7 days overdue an item is asked about in
  the review on every day until it is answered; after 30 it moves to *Dropped* with an
  undo, so the Later list stays a list of real things.
- **Weekly look** (optional, Friday afternoon by default): everything due next week and
  everything *Waiting* for more than 7 days, so chasing people is a habit.

**Where it is stored**: in the engine, per persona, with the thread's message ids, kind,
date, note and history. The server sees `\Flagged` on the newest message (so phones and
webmail still see it as flagged) and, for snoozed threads, the `$Snoozed` keyword and a
hidden `Snoozed` folder so other clients show it gone. A thread flagged elsewhere arrives
as a *Reply* item due tomorrow morning, so even flags from another client get a date.

**Later** is per persona: the Work list shows only Work mail, and the review respects each
persona's quiet hours.

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

- Through Praeco, drawn by Forum (tabula.md §5). A post is readable without opening Mail
  (Shankar 2026-11-16, tabula.md §5.4): the whole subject as the title (wrapped, not cut),
  sender and verified mark as the subtitle, and the first 2 KiB of the new text (quoted
  text and signature removed, plain text from the parse jail) as the body, which the toast
  expands to in place. Locked: "Mail · 3 new" only. Logged out or someone else's session:
  nothing. Screen shared: no titles or bodies. Encrypted mail: "Encrypted message from …".
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

### 12.1 No storm of pings after a gap (Shankar 2026-11-16)

Shankar: "Tons of pings when new mail arrives suddenly, maybe due to a network
disconnection. Very annoying. Same with meeting reminders that suddenly show up." The
cause is a client that notifies for everything it *learns about* rather than for what is
*new to the person now*. Mail's rules:

- **Only fresh mail pings.** A message notifies individually only if the server received
  it within the last 10 minutes when Mail learns of it. Anything older (it arrived while
  the machine slept, the network dropped, or the server was slow) never pings one by one.
- **Catch-up is one summary, silent.** After a reconnect, wake from sleep, or any sync that
  brings more than a few older messages: one notification, "While you were away: 23 new,
  4 from people (Priya, Arun…)", no sound, replacing any earlier summary rather than
  stacking. The People ones are listed in it; the rest are counted.
- **Never for bulk work**: the first sync of an account (§6.4), a resync after the server
  reset its ids (`UIDVALIDITY` change), mail moved between folders, imports.
- **Bursts collapse.** More than 2 notifications within 30 s for a persona become one
  grouped notification that updates in place ("5 new messages"); at most one sound per
  minute per persona, however much arrives.
- **Already read elsewhere, no ping.** If the message is `\Seen` by the time Mail would
  notify (read on the phone), it doesn't; if it is read elsewhere after notifying, the
  notification is withdrawn from Forum's list.
- **Late reminders go quiet.** A *Later* item (§11.4) or an invite reminder (§14.1) that
  comes due while the machine was asleep or offline is shown once, silently, in the next
  summary ("Due while you were away: send Priya the numbers"). A reminder for a meeting
  that has already started more than 5 minutes ago, or has ended, never pops up; it is in
  the history only. Several overdue reminders are one summary, not one each.
- **Quiet hours win** (§12): what arrives inside them is in the next working period's
  summary, also silent.

**For every app, in Praeco** (Shankar 2026-11-16: "Yes, system wide"): these rules are
now tabula.md §5.3, so a calendar or chat app can't storm either. Mail sets the event time
on every post.

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

## 15. Layout (Shankar 2026-11-16: "what should be the UI layout to make it intuitive")

Mockup: `docs/design/mail-layout-mock.html` (an HTML sketch in the visual-language tokens,
not Pergola; screenshot in project files `mail/mail-layout-main.png`).

### 15.1 Principles

- **Three columns people already know** (places, list, reader), so nothing has to be
  learned before the first day; what is new (Later, verified senders, the link bar) sits
  where the familiar thing was.
- **One obvious next step per message**: Reply, Later, or Archive ("done"). Archive is the
  primary action, not Delete, so the inbox empties without fear; everything is undoable for
  10 s from an in-window undo bar.
- **Search is the main way to find things**, so it is the widest control at the top, over
  the list and reader, with operator hints as you type.
- **Trust is shown in Mail's chrome, never inside the message**: the verified domain on
  every sender, warnings in a band above the body, the remote-pictures bar, the link bar.
  The message body is a framed slot (Limen) that can't draw over any of them.
- **The persona is the system's**: frescod's strip shows the persona chip and forum-wm the
  coloured frame (personas.md §7); Mail never draws them.

### 15.2 The main window

```
┌ strip: Mail · [Work] ─────────────────────────────────────────── (frescod) ┐
│ [Write]   [ Search mail and attachments…   from: has:attachment ]   Ctrl-K │
├──────────────┬──────────────────────────┬──────────────────────────────────┤
│ Inbox     12 │ Inbox  [Conversations|Msgs]│ Subject, in full                │
│ Later 2!· 3  │ (Unread)(To me)(Needs reply)│ people · 4 messages · 2 files   │
│ Waiting    4 │ TODAY                     │ [Reply][Reply all][Fwd][Later][Archive][⋯]│
│ Drafts     1 │ ▌Arun, Priya ✓acme   09:41│ [files strip]                    │
│ Sent         │  Q4 numbers for the…      │ older messages, one line each    │
│ VIEWS        │  preview line…            │ ── new since you last looked ──  │
│ Newsletters  │ Lena ✓partner  08:15 📎   │ Arun ✓acme.example → Priya, you  │
│ New senders  │ …                         │ [3 pictures not loaded · Load]   │
│ Files        │ YESTERDAY                 │ ┌ body (jailed doc-viewer) ────┐ │
│ SAVED        │ Sam  ✓acme  Later·today   │ └──────────────────────────────┘ │
│ ACCOUNTS ▸   │ IT Helpdesk  not verified │ [link bar: Opens dash.acme… Open]│
│ ✓ up to date │                           │ [Reply to Arun and Priya…      ] │
└──────────────┴──────────────────────────┴──────────────────────────────────┘
```

- **Sidebar (left, 224 pt, collapsible to icons):** what needs the person first: *Inbox*
  (all accounts of this persona), *Later* (with the overdue count in red), *Waiting*,
  *Drafts*, *Sent*. Then *Views* (Newsletters, New senders when the screener is on, Files,
  later the lanes), *Saved searches*, and *Accounts* with their server folders, folded by
  default because most people never need them. At the bottom one quiet sync line (§6.4)
  with *Pause*.
- **List (middle, 400 pt):** the folder name, the Conversations/Messages toggle (§11.7) and
  the filter chips; date group headers; rows of three lines: sender(s) with the verified
  domain and count, time and any Later chip; the subject in full (wrapping to two lines,
  not cut at one); one preview line. Unread is bold, the selected row has the accent bar.
  Hovering a row shows Archive and Later at its right edge; a two-finger swipe does the
  same (right = Archive, left = Later).
- **Reader (right, the rest):** the subject in full and who is in the thread; one toolbar
  with keys shown on the buttons; the thread's files; older messages collapsed to one line
  each; the "new since you last looked" line; the newest message open with its sender line,
  trust notes, pictures bar and body; the link bar appears at the bottom when a link is
  pressed (§7.6); a reply box always at the bottom ("Reply to Arun and Priya…"), which grows
  into the full composer in place, so replying never opens a new window.

### 15.3 Other screens

- **Compose a new message** opens a separate window (so it can sit beside the mail it
  draws on); a reply stays inline and can pop out. To, Cc/Bcc on demand, Subject, the
  editor, attachments as chips, and *Send* (with *Send later* on its arrow). The From line
  shows the identity and persona.
- **First run**: one field, "Your email address", then the sign-in (Arca's field or the
  provider's page), then straight into the inbox while §6.4 fills in. Server settings only
  behind "Set up manually".
- **Later list**: grouped *Overdue / Today / Upcoming / Waiting*, each row with its note and
  `d` / `l` / `x` (§11.4). The daily review opens here.
- **Empty inbox**: says so plainly, with what is due in Later next; no illustrations.
- **Settings** in a sheet inside Mail (accounts, signatures, notifications, quiet hours,
  pictures, the screener); Atrium-wide things (Do Not Disturb, default mail app) stay in
  Curia.

### 15.4 Sizes

All in points (resolution-independence.md). Wide (≥ 1200 pt): three columns. Medium
(800–1200): the sidebar folds to icons. Narrow (< 800): one column at a time, list then
reader, with Back. Reading width in the reader is capped (about 80 characters) and
centred, so wide windows don't make long lines.

### 15.5 Keyboard and accessibility

- Every action has a key, shown on its button; a command palette (Ctrl-K) for the rest;
  the common single-key set (j/k next/previous, e archive, r reply, a reply all, f forward,
  l later, d done, / search, # delete, z undo) on by default with a switch.
- Undo for every list action (archive, delete, move, Later) for 10 s.
- Pergola accessibility tree for every pane; the doc-viewer child's tree joins it through
  Limen (limen.md §9). Focus order follows the columns; screen readers hear the verified
  domain and any warning before the body.

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
   *Built 2026-11-16 (host-tested):* `mail/mail-parse`: own RFC 5322/2045–2047/2231
   parser into a bounded `Summary` (headers, encoded words, tolerant address lists with a
   "display name looks like an address" flag, MIME tree with IMAP part paths and body
   ranges, base64/QP, WHATWG charsets via `encoding_rs`, UTF-7 never decoded, dates,
   list/unsubscribe/receipt/automated flags, previews without quotes and signatures;
   bidi, zero-width and control characters dropped from every shown string; hard limits
   refuse, soft limits cut); 20 tests, fuzz 1.3 M runs clean. `mail/mail-proto`: IMAP
   decoder (literals across any split, typed responses and codes incl. QRESYNC
   `VANISHED`, `THROTTLED`, Gmail `X-GM-*`, `EMAILID`, `PREVIEW`, modified UTF-7 names)
   and typed command encoder (QRESYNC `SELECT`, `UID FETCH`/`STORE`/`MOVE`/`EXPUNGE`,
   `APPEND` with LITERAL+ or synchronising literals, `SEARCH` whose text is always a
   string or literal, flags validated), SMTP reply decoder, EHLO caps and a submission
   state machine (SIZE, 8BITMIME, SMTPUTF8, FUTURERELEASE `HOLDFOR`, partial recipient
   refusal, RSET on failure, dot-stuffing); 12 tests, fuzz IMAP 0.5 M and SMTP 0.7 M
   runs clean. *Rest of M0 built 2026-11-16:* DKIM (rsa-sha256, ed25519-sha256; rsa-sha1
   and `l=` partial bodies never verify), DMARC alignment by the PSL, the provider's
   topmost `Authentication-Results`, verdict Verified/NotVerified/Pending; iTIP invitations;
   TNEF unpacking; HTML through navigator-normalize (`mail-parse::html`: everything that
   acts or fetches stripped, every picture and CSS `url()` an opaque resource id with its
   source, hidden or tiny remote pictures found on the resolved cascade, removed and never
   loadable, fonts never loadable; output renders with zero profile refusals); the fixed
   engine ↔ jail format `mail/mail-schema` (postcard frames ≤ 65 MiB, batches ≤ 64
   messages/32 MiB, `Query`/`Answer` for TXT lookups limited to `_domainkey`/`_dmarc` and
   6 per message, and `check()` the engine runs on every frame: lengths, counts, part
   ranges inside the message, a Verified domain that must be From's); the runner
   `mail-parse-jail` (loads the pinned fonts, `cap_enter`, serves stdin/stdout). Tests:
   lib 8, parse 20, DKIM 4, HTML 6, jail end to end 3; fuzz `html` clean. JMAP codecs
   move to M7.
2. **M1** the mail courier in Arca (auth, relay, filter), against a local IMAP/SMTP server;
   test that `LOGIN` after auth kills the session and is logged.
   *Built 2026-11-17 (host-tested):* Arca learned the mail schemes (`imaps`, `submissions`,
   and `imap`/`submission` that must STARTTLS; never with a partition) in recipients,
   courier labels and courierd requests; arcad `courier_op::LOGIN` (account + secret,
   Password/OAuth items only, only to a mail-scheme courier for exactly that endpoint)
   and `ANOMALY` (logged as `ANOMALY courier:imaps://host:port/… refused-command LOGIN`,
   first token only, never a value). `arca-courier` runs a mail session when its label is
   a mail scheme: client hello `u32 LE 17 | u8 1 | item[16]`; connect, TLS (STARTTLS
   with anything sent in the clear before it refused and pre-TLS capabilities ignored),
   then `LOGIN` from Arca, `AUTHENTICATE PLAIN` (SASL-IR or continuation) / `AUTH PLAIN`,
   greeting `* PREAUTH [CAPABILITY …]` or the post-login EHLO reply without AUTH,
   STARTTLS, SASL-IR, COMPRESS, CHUNKING, BINARYMIME, LOGINDISABLED; EHLO name is an
   address literal. The relay filter (`mail_filter`) refuses `LOGIN`, `AUTHENTICATE`,
   `STARTTLS`, `UNAUTHENTICATE`, `COMPRESS` / `AUTH`, `STARTTLS`, `BDAT`; holds client
   bytes after a synchronising literal until the server's `+` (or its answer, after
   which they are commands again) and after `DATA` until the 354 (pipelined replies
   counted); lines ≤ 64 KiB, literals ≤ 80 MiB, held ≤ 64 KiB. Tests: arcad 3, courier
   8 against scripted TLS IMAP/SMTP servers (LOGIN/STARTTLS/AUTH/BDAT after login
   refused and logged and never reach the server; smuggling through a refused literal or
   refused DATA caught; wrong password, locked Arca, wrong certificate with Arca never
   asked); fuzz `mail_filter` 4 M runs clean (forwarded bytes always a prefix of the
   client's). Not yet: XOAUTH2/OAUTHBEARER (with §5.3), a VM run against Dovecot.
3. **M2** engine + sync: read-only IMAP sync into the store, the index, plain-text reading in
   a minimal Pergola window; jailed parse batches.
   *Built 2026-11-17 (host-tested):* `mail/mail-store`: the index in redb (accounts,
   folders with their sync checkpoints, messages, (folder, UID) locations with flags, a
   newest-first list key, server-id dedupe), message bytes as SHA-256-named blobs written
   to a temporary file and renamed, checked on every read. `mail/mail-sync`: the session
   over the courier's PREAUTH socket, `ENABLE QRESYNC`, folders with SPECIAL-USE roles
   (Gmail: only Inbox, Sent and All Mail walked), every folder opened with `EXAMINE`; the
   staged first sync of §6.4 (the inbox's newest 200 headers, then all headers in pages of
   500 newest first, then whole messages ≤ 2 MiB in the 90-day window), resumable from
   checkpoints after every page; resync by QRESYNC (`VANISHED` and changed flags in
   `SELECT`), else CONDSTORE `CHANGEDSINCE`, else all flags and a UID diff; UIDVALIDITY
   change resets the folder. `mail/mail-engine`: sync's sink, which parses each page in
   parse jails in batches (≤ 64 messages / 32 MiB, every frame checked) and writes each
   batch in one transaction; a headers-only batch makes a signed message's sender check
   *Pending* until its body comes, not a failure; `open` parses a message alone in its own
   jail for reading. `mail/atrium-mail`: the window of §15.2 at M2's size: places (each
   account's folders with counts), the list (sender and time, subject, preview; unread
   bold; the selected row with the accent bar; rows read from the index a page at a time
   as they come into view), and the reader (subject, sender, recipients, date, the plain
   text wrapped to an 80-character column; notes for headers-only and HTML-only
   messages); j/k and the arrows move and open. Tests: store 2, sync 1, window 2+1
   headless (a press on a row, j/k, an empty folder), and end to end against a real
   Dovecot over TLS through the real courier and real parse jails: 305 messages in 5
   folders, stage 1 first, 50 texts in the window, resync after a delete, an arrival
   and a flag change, then a pass that changes nothing. Not yet: DKIM/DMARC lookups (the
   engine answers every `Query` with nothing, so sender checks stay *Pending*; see the
   privacy note on lookups at arrival), sync and engine as separate jailed processes over
   Aqueduct with the window reading through the engine, jails started by jaild, fetching a
   text on open and fetching only the text part, IDLE in a loop, the narrow layouts of
   §15.4, a VM run with frescod.
4. **M3** the full UI: three panes, conversations, HTML through `doc-viewer`, remote content
   bar, sender line, link bar through Monstrator, keyboard set. Mail becomes the `mailto`
   app.
   *Built 2026-11-17 (host-tested):* **Conversations** (§11.1, §11.7): threads in the store,
   joined by Message-ID, References and In-Reply-To in any arrival order (merging when a
   late message links two), else by base subject within 14 days for a Re:/Fwd: only; each
   folder lists them under their newest message; Conversations or Messages per folder,
   remembered, messages by default for Sent, Drafts, Trash and Junk. The reader opens the
   newest and the unread, folds the rest to one line, marks where the new ones start, and
   dedupes a message that is in two folders. **Plain text**: quote levels, a conservative
   linkifier (https, http, mailto), the quoted tail and the signature folded, format=flowed
   undone. **Sender line** (§7.5) from the checks, never the message: the verified domain
   with ✓, else the address and "not verified"; a display name that is an address or a
   domain is replaced by the address; "Replies go to …". **HTML** (§7.3): the jail's
   document is kept with the message (opening again parses nothing) and shown in a
   `doc-viewer` slot under the message's header lines, one message at a time; the
   message's own pictures (cid:, data:) go to the child by Limen's new `Provide { id }`
   (limen.md §2.1) with their real sizes read in the jail. **Remote pictures** (§7.4): the
   bar "N pictures not loaded · Load pictures", "Always for this sender" only for a
   verified sender; Mail has no network, the press is fetched by Mail's own Nuntius
   fetcher (`net-fetch`) on a thread, kept with the message, and provided to the open
   viewer; the jail reads a loaded picture's size from its first 64 KiB, so a remote
   background is drawn once loaded. Hidden-picture fixes found on the way: the first rule
   of the normalizer's style block was skipped, and an `<img>` the normalizer drops is now
   never fetched. **Link bar** (§7.6): a press shows "Opens https://dash.**acme.example**/q4"
   with Open (Enter, Mail's press for Monstrator, with its activation token), Copy and
   Escape; Safe Links, URL Defense v2/v3, Google /url and the common social redirectors
   are peeled (Mimecast keeps its target server-side, so the bar says so); `utm_*` and
   click ids are removed from what opens; the jail lists each link's visible text so a
   text naming another site is shown beside the destination. **Keyboard set**: j/k, the
   arrows, Home/End, e archive, # delete, z undo, Enter and Escape on the link bar;
   opening reads. List actions apply to the index at once and become ops sync carries to
   the server after the 10 s undo window (UID STORE, UID MOVE, or COPY and a UIDPLUS
   expunge of that UID only); a refused op puts the index back; a leaving message stays
   known to sync so it isn't fetched again. **`mailto`**: Mail's `atrium.toml` declares
   `[[links]] schemes = ["mailto"]`, `net-fetch` and no network of its own. Tests: store
   5, parse jail and HTML, engine pictures with a fake Nuntius, the Dovecot end-to-end
   test (now with read, archive, delete and undo reaching the server), window 5 headless
   (conversations, HTML framing and the pictures bar, the link bar opening through a fake
   Monstrator, archive/delete/undo), links 4. Not yet: r/a/f (compose is M4), l/d and /
   (Later, Done and search are M6), the Ctrl-K palette, the narrow layouts of §15.4,
   persona-specific link scanning (§7.6), first-contact and look-alike notes (§7.5), and a
   VM run with frescod (frescod doesn't build on the Linux host).
5. **M4** compose and send: rich text, drafts, undo send, send later, identities, `mailto:`.
   **Built 2026-11-17 on the host** (`7db04ce1`, `7914ac9b`, `8f000be4`, `4ef936c4`,
   `641c2e88`, `478acadc`, `af5c213a`). **Writer** (`mail-compose`): RFC 5322 messages from a
   draft: folded headers, RFC 2047 words for names and subjects, IDNA domains, plain text as
   quoted-printable, an HTML alternative, attachments, a Message-ID from 16 random bytes, the
   date in UTC, no `User-Agent`, no Bcc header; every typed header field cleaned of line
   breaks and control characters. Replies (Reply-To first, "Re:" once, never to oneself, a
   reply to one's own sent message to its recipients), forwards, quotes, threading headers.
   `mailto:` links (RFC 6068): only to/cc/bcc/subject/body/in-reply-to, notes for what was
   dropped, a hidden copy, hidden characters (removed) and a long body. The Drafts copy
   (`write_draft`) keeps Bcc and leaves out half-typed addresses. **Sending** (§9.3):
   `mail-sync::submit` sends over a courier submission session (the courier's greeting is
   the post-login EHLO reply), pipelined, with a FUTURERELEASE hold when the server offers
   one; the engine's outbox holds the written message and its draft, waits out the undo
   window (10 s, 5–30), is claimed so Undo can't reach it after, retries transient failures
   (1, 2, 4 … minutes, at most an hour), stops for the person on a permanent failure or
   when Mail stopped mid-send (it may have gone: never sent twice), holds Send later itself
   when the server can't, and asks about scheduled sends that came due while the machine
   was off. After sending, a copy goes to Sent (`\Seen`) unless the provider files it.
   **Drafts**: saved by the window every 3 s, copied to the server's Drafts (`\Draft`)
   after 30 s at rest, each copy replacing the last (the old one deleted once sync has seen
   it); sending or discarding removes every copy. **Identities** (§9.2): per account From
   addresses with signatures; a reply comes from the address it was sent to (E12);
   changing From swaps the signature. **Compose window**: a Pergola window per message
   (From, To, Cc, Bcc, Subject, the text), Send only with a press's activation token
   (Ctrl+Enter counts), Send later (in an hour, tomorrow 08:00, Monday 08:00; UTC until
   apps get time zones), Discard, the forgotten-attachment question once; the main window
   has Write (c), Reply (r), Reply all (a), Forward (f) and a "Sending… Undo" toast whose
   Undo opens the message again; `mailto:` links arrive on the document channel; drafts
   whose windows were open reopen at start. **Rich text**: `RichTextEditor` (one page as
   wide as the window, growing with the text) kept to a mail profile (paragraphs,
   headings, B/I/U/strike, web and mail links, lists, quotes), sent as
   `multipart/alternative` with a plain part from the same paragraphs; drafts keep Mail's
   HTML and reopen as rich text; a Plain text switch. **Quoted originals** (`4d27ed1d`):
   a reply or forward of an HTML message shows the original under the text in a jailed
   doc-viewer slot, as the reader does (the Navigator's `DocumentView`), with Remove; it is
   sent in a cited blockquote with its formatting, kept to a safe set of elements (no
   pictures, styles, forms or attributes but web and mail links), and as "> " lines in the
   plain part; switching to plain text folds the quote into the text. Tests: compose 6 + 4 round trips
   through Mail's own parser, sync submit (scripted server), store outbox and drafts, engine
   compose starts, the Dovecot end-to-end test now with Dovecot's submission service
   through the real courier to a test relay (every recipient, no Bcc header, filed in Sent,
   drafts copied and replaced, Send later held locally), window compose 2 headless.
   **Not yet**: the app runs no sync or send loop of its own (no account setup §5.1 yet,
   so deliver/copy_drafts are called by tests), so the press's token isn't spent by a
   trusted spender (§9.4 needs the separate engine process) and the courier's hourly cap
   isn't reached; attachments in compose (M5 picker), address completion (§14.2), per
   recipient plain text, inline images and tables in the profile, a link button, time
   zones, and a VM run with frescod.
6. **M5** attachments: quick look, `OpenAttachment`, Save via picker, attachment search
   text. **Built 2026-11-18 on the host.** The parse jail lists a message's attachments
   (a `winmail.dat` replaced by its files, pictures no HTML shows, forwarded messages) with
   their first 4 KiB, and a new `Attachment` op decodes one alone, with its text when it is
   text; the engine keeps the list with the converted document, keeps names and text per
   message for M6's search, and gives a saved copy's origin. The reader shows a row per
   attachment: type icon, name, kind and size from the bytes (atrium-types), a name that
   lies or a program said in warning colour with no Open; Quick look draws text itself and
   embeds `doc-viewer` (HTML, pictures), `pdf-viewer` and `media-player` in a slot under
   the list; nothing is ever written to a temporary file: viewers and apps get a sealed,
   read-only memfd. **`OpenAttachment`** (portcullisd): only for an app whose installed
   manifest holds the new `open-attachments` capability (Mail's does); the descriptor must
   be a regular file sealed against write, grow and shrink; portcullisd decides the type
   from its bytes and name, refuses programs, shortcuts and names that lie, narrows it
   read-only, spends the press last and hands it to the person's default app (or the
   first that opens the type). **Save**: Limen's picker gained a Save target
   (`Kind::Save`, a name and an origin); Scrinium makes a new file in the picked folder
   (never over another: "q4 (2).csv"; never through a link; 0644), writes
   `atrium.origin`, and hands it over for writing; Mail writes the bytes. **Compose**:
   Attach… opens the picker over the window (several files), each file's type from its
   bytes, a row each with Remove, a warning over 20 MB; a forward carries the original's
   files. Tests: jail listing and decoding, engine list/decode/index/origin, picker save
   filter, Scrinium save target and `create_new`, window strip and quick look, compose
   attach and forward. **Not yet**: Open with… (another app than the default), office
   files in quick look (Officina converters), encrypted ZIP/PDF passwords via Arca, "all
   files in a thread / from a person" and the Files view (M6 search), the picker over home
   places (homes.md W4: today it shows sticks and shares), attachment text from PDFs and
   office files, and a VM run with limend, Scrinium and portcullisd.
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
