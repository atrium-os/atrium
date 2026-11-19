# Mail: Atrium's email app

**Status: accepted 2026-11-16** (Shankar: "Accept as recommended", all of §17 D1–D8 as
recommended, after review additions §6.4, §11.4, §11.7, §12, §12.1, §15). M0 built (codecs, sender checks, HTML, jail format); M1 mail courier built on the host; M2 engine, sync and a minimal window built on the host; M3 full UI built on the host; M4 compose and send built on the host; M5 attachments built on the host; M6 search, lists, screener, Later and notifications built on the host; M7 S/MIME, JMAP and OAuth (Microsoft) built on the host; account setup, background sync and send, and a settings window built on the host; M8 lanes, cards, private addresses and OpenPGP built on the host (big-file links and local assistance wait on platform pieces); DKIM/DMARC lookups through Nuntius built on the host; push by IMAP IDLE and JMAP EventSource built on the host; IMAP and JMAP sync and sending run in a jailed worker; recipient completion from an address book built on the host; quick filters, grouping, and Later's overdue rules, Dropped list, weekly look, flags from other clients and `$Snoozed` built on the host; grouping kept per folder, by lane, and filters saved as smart folders built on the host; server-side search built on the host; the rest of the notification settings (weekend quiet, replies through, the digest, the review's time) built on the host; first-contact and look-alike notes built on the host; notification buttons built on the host; the Ctrl-K command palette built on the host; the medium and narrow window sizes built on the host; the address book's People section built on the host; a scrolling Settings window and the single-key switch built on the host; recipients outside the company pointed out in Work built on the host; company link scanning kept in Work built on the host; export as mbox built on the host; import from mbox built on the host; Junk and Not junk teaching the server built on the host; the person's own rules built on the host; automatic replies for JMAP accounts built on the host; rules and automatic replies on IMAP servers through ManageSieve built on the host; the automatic reply's first day and multi-line text built on the host; a Junk button on the reader, "Always…" rules from the reader and rules run over mail already here built on the host; the palette in compose windows, chips on search results, and other small leftovers built on the host; Open with… for attachments and Report phishing built on the host; icons on the medium window's rail, phishing reports to the company in Work, and files across mail built on the host; search operators sent to the server and rules on JMAP servers through JMAP Sieve built on the host; filters changed on the server kept (with Replace) and the automatic reply through JMAP Sieve built on the host; company domains, import progress and given-up uploads, and the changed filters shown built on the host; ManageSieve passed against Pigeonhole on the host and on the FreeBSD VM; key hints hidden with single keys off, the company's own link scanner and the Files view's counts built on the host; OpenPGP keys listed with Forget, and expired and revoked keys never encrypted to, built on the host; unread counts per lane, S/MIME CAs you add and the certificates list built on the host.
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
| notifications | Praeco + Forum toasts | notification actions (tabula.md §5.5) for Archive / Reply |
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
channel (EventSource, RFC 8620 §7.3) is one long-lived courier request, which the
courier passes on one event at a time (arca.md §5.5).

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
Vacation replies through Sieve or JMAP `VacationResponse`. (Built: §16, "Filters on IMAP
servers through ManageSieve".)

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
- **Actions** (Archive, Mark read, Reply) on a one-message notification, through Praeco's
  notification actions (tabula.md §5.5): each action id names its message, so a press
  that starts Mail, or comes after a restart, still acts on the right one.
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
   and a flag change, then a pass that changes nothing. Not yet (DKIM/DMARC lookups came
   in M8, through Nuntius): sync and engine as separate jailed processes over
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
   (Ctrl+Enter counts), Send later (in an hour, tomorrow 08:00, Monday 08:00; local time,
   see M8), Discard, the forgotten-attachment question once; the main window
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
   isn't reached; attachments in compose (M5 picker), address completion (§14.2; built later, see M8), per
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
   files in a thread / from a person" and the Files view (built 2026-11-19), the picker over home
   places (homes.md W4: today it shows sticks and shares), attachment text from PDFs and
   office files, and a VM run with limend, Scrinium and portcullisd.
7. **M6** search, smart folders, unsubscribe, newsletter lane, screener, snooze, follow-up,
   notifications. **Built 2026-11-18 on the host.** **Search** (§11.2): our own inverted
   index in the store's redb (no tantivy), over from, to, subject, text, file names and
   attachment text, each term with the fields it is in; operators `from:` `to:`
   `subject:` `filename:` `has:attachment` `is:unread` `is:flagged` `in:` `before:`
   `after:` `larger:` `smaller:`; the last word matches word starts while typing. In the
   window `/` (or the sidebar's search box) takes the keys, results from every folder come
   as you type, Enter or Down go to them, Escape clears then closes; a result is read and
   archived where it was found; *Save* keeps it as a place under *Saved searches*.
   **Lists** (§11.3): the best unsubscribe from `List-Unsubscribe` (RFC 8058 one press,
   else `mailto:`, else web); one press is a POST through a Nuntius fetcher of its own;
   `mailto:` opens a filled message to send; web goes to the link bar. Then "Block future
   mail from this list": later ones go to Trash on arrival. **Arrival rules** in the
   engine run on mail new to the index: addresses written to (from Sent), list mail into
   the newsletter lane, blocked lists, the screener, Later. **Newsletters** view: list
   mail from senders not written to, every folder. **Screener**: when on, someone new
   (not written to, accepted, or heard from before it was on) is held under *New
   senders*, local only; *Accept* releases their mail and lets later mail through,
   *Refuse* keeps it under *Screened out*; off releases everything. **Later** (§11.4): `l`
   opens a sheet (Reply, Read, Do, Waiting; this evening, tomorrow morning, this weekend,
   next week, someday, keys 1–5); the conversation leaves the inbox (held locally),
   `\Flagged` goes to the server, and it comes back at its time; a reply sent closes a
   *Reply* item, an answer closes a *Waiting* one, mail to the person from a person
   brings it back at once and list mail waits with it; setting it again counts as pushed
   back (shown from 3); `d` is Done; the *Later* view lists items soonest first and
   overdue ones colour the sidebar count. **Follow-up**: *Remind me* in compose (3 or 7
   days) makes a *Waiting* item when the sent copy reaches Sent. **Notifications**
   (§12, §12.1) through Praeco's v0 post: fresh mail (10 min) from a person or in a
   conversation the person wrote in, one by one; lists and automated mail counted only;
   more than 2 in 30 s grouped; older mail after a gap one silent summary replacing the
   last; nothing for a first sync; withdrawn when read elsewhere; the Later review once a
   day after 09:00 ("1 due today, 2 overdue"). Store schema 6. Tests: index and query
   language, saved searches, list rules and screener, Later rules and follow-up,
   notification rules, and window tests for search, unsubscribe/block/screener and Later.
   **Not yet**: a settings page (screener switch, review time, quiet hours, sounds,
   VIPs), times in the person's time zone (built later, see M8), Later notes typed in the sheet,
   the weekly look and the 7/30-day overdue rules, `$Snoozed` and a server *Snoozed*
   folder, (quick filters and grouping, §11.7, built later, see M8), server-side search outside the offline
   window, Praeco's longer posts and event times (tabula.md §5.3–5.4, so freshness uses
   the message's date), notification actions, and a VM run.
8. **M7** S/MIME with Sigillum tokens and Arca certificates; JMAP; OAuth (Microsoft first).
   **S/MIME built on the host (2026-11-17)**: new crate `mail/mail-smime` (RustCrypto
   `cms`/`x509-cert`/`der` and ring; no OpenSSL; holds no private key): a detached
   signature checked with its chain to the roots given (RSA PKCS#1/PSS 2048–8192, ECDSA
   P-256/P-384, SHA-2 only; SHA-1 refused; issuers must be CAs; validity at the signing
   time when that is not in the future; extended key usage must allow email; From must be
   one of the certificate's addresses), detached signing around an outside signer (ECDSA
   `r||s` made DER; the signature is checked before it is sent), `EnvelopedData` with
   AES-256-CBC and RSA PKCS#1 v1.5 key transport (EC recipients refused by name), reading
   AES-128/256-CBC, and BER from streaming senders. Checked against OpenSSL both ways
   (its streamed signature and envelope verify and decrypt here; ours verify and decrypt
   with `openssl cms`). The parse jail checks `multipart/signed` against the roots the
   engine sends (the system's `/etc/ssl/cert.pem`, same as Nuntius; bare-LF copies are
   checked as CRLF) and reports signer, issuers and state; the engine holds a jail's claim
   to From and to the certificate's hash (mail-schema). The reader shows "Signed by Grace
   Hopper <grace@…> · certificate from …", or why not, and the signature part is never
   listed as a file. Valid signers' certificates are kept per address (store schema 7,
   4 per address) and used to encrypt. Compose has **Encrypt** (on only when every
   recipient's certificate is known; the person's own certificate is added so the Sent
   copy stays readable) and **Sign** (when sigillumd is there): Sigillum gained
   `CertPurpose::MailSign { from }` (sigillum.md §10.2; only apps in
   `SIGILLUM_MAIL_SIGNERS`), the certificate is picked and the token signs the digest of
   the CMS signed attributes on Sigillum's sheets. Decrypting: the jail says which of the
   person's certificates the message is for and hands out the wrapped key; whatever holds
   the key unwraps it; the jail decrypts and parses the inside. The content key is kept in
   memory for the session only, and nothing of an encrypted message's inside is stored or
   indexed (stricter than the setting above, until persona volumes are encrypted). Tests:
   mail-smime 5, jail 4 more (signed, claims, encrypted), engine 3, window 2, compose 2,
   sigillumd 1. **Not yet**: unwrapping with the token (Sigillum needs a decrypt op and the
   PKCS#11 host's allow-list a decrypt mechanism) or with an Arca-held key (`arca_decrypt`,
   `arca_sign`), so the reader says it can't decrypt yet; the person's added CAs (built 2026-11-19, below); opaque
   signed-data (`smime-type=signed-data`) and AuthEnvelopedData (AES-GCM); revocation
   (OCSP/CRL); a certificates page (seen certificates, forgetting one; built 2026-11-19, below); Bcc recipients
   show in the envelope's recipient list (as in other clients).
   **JMAP built on the host (2026-11-18)**: `mail-proto::jmap` (session resource,
   request builders, bounded readers of every answer, RFC 3339 dates) and
   `mail-sync::jmap::Client` over the caller's HTTP (the HTTP courier, which adds the
   `Authorization`, so sync holds no credential). It fills the same `Sink` as IMAP:
   each mailbox is a folder named by its path, a message gets a local UID in each
   mailbox it is in, and the JMAP id is its server id, so one message in two mailboxes
   is one message. First sync is headers first (§6.4): each mailbox's newest messages
   (up to 5,000, pages of 256) listed from the `headers` property with no download,
   then a text pass downloads whole messages under 2 MiB for the text window, inbox and
   Sent first, 200 a pass. Resync is `Email/changes` (moves, flags, new, destroyed);
   `cannotCalculateChanges` reads everything again without doubling. The person's
   changes go as one `Email/set` (several changes to one message are one patch); delete
   from the last mailbox destroys. Send: upload, `Email/import` into Sent (seen),
   `EmailSubmission/set` as the matching identity, with `HOLDUNTIL` when the server's
   `maxDelayedSend` covers a send-later time, else the engine holds it
   (`Engine::deliver_jmap`). The store keeps the Email state per account. Test: the
   engine with real jails against an in-process JMAP server (first sync, one message in
   two mailboxes, a large one by headers only, archive + read made on the server, changes
   from elsewhere, a too-old state, send with Bcc only in the envelope, refused sender).
   **OAuth built on the host (2026-11-18)**: `arca-courier::oauth` with Microsoft
   (authorization code + PKCE S256, `response_mode=query`, redirect to Microsoft's
   `nativeclient` page for the sign-in sheet to catch, state checked, the provider's
   error code only). An OAuth item's value is a grant record (provider, client id,
   refresh token). The mail courier, after TLS to the mail server, gets it with `LOGIN`
   (which now says password or OAuth), refreshes at the provider's fixed token endpoint
   through the fetch rules, gives a rotated refresh token back to Arca (new arcad
   `REFRESHED` op: only an OAuth item of that courier's endpoint, and only the token may
   change, not the provider or client id), and logs in with `XOAUTH2` (IMAP with or
   without SASL-IR, SMTP `AUTH XOAUTH2`; a failure's JSON challenge answered empty). A
   refresh token the provider refuses (`invalid_grant`) ends the session with "Sign in
   again" before any login. Tests: courier OAuth end to end over TLS (token endpoint,
   IMAP, SMTP STARTTLS, rotation, refused grant, unknown provider), arcad 1, unit 3.
   **Not yet** for JMAP and OAuth: the sign-in sheet in Navigator that opens the URL and
   catches the redirect, and storing the first grant (an arcad op for the sign-in, with
   the PKCE verifier kept in Arca); Atrium's Microsoft app registration (client id);
   pf rules letting a mail courier reach its provider's token host; account setup in
   the app; JMAP push (EventSource), `Mailbox/changes`, fetch on open for large JMAP
   messages, removing messages destroyed while a state was too old; JMAP over the HTTP
   courier for real (only the in-process server so far); OAUTHBEARER; Google (§17 D4).
   **Day-to-day use built on the host (2026-11-18)**, before M8. *Account setup*
   (`mail-sync::setup`): settings from the address, in order a known provider (Google,
   Microsoft, iCloud, Fastmail, Yahoo), JMAP's well-known session (checked as a JMAP
   session), autoconfig (`autoconfig.<domain>`, then the domain's `.well-known`), SRV,
   the MX's provider (shown as "handled by …", which needs a press), else a guess
   (`imap.`/`smtp.`, said to be one); lookups through Mail's own Nuntius fetcher. The
   password is typed into Arca's own field (Limen `credential` role in the settings
   window) and a new arcad app op `KEEP_FOR` redeems that field's handle into Password
   items for the account's servers (IMAP and SMTP, or the JMAP host) in Mail's persona,
   available after first unlock; Mail gets only the item ids, kept with the account's
   settings in the store (`account_config`). *Sync and send loop*: `Engine::pass` runs
   one pass for an account (IMAP: sync, drafts to the server after 30 s, due sends,
   copies to Sent unless the provider files them itself; JMAP: sync and sends over the
   HTTP courier with the item as a bearer) and keeps how it went (up to date, sign in
   again, failed and why). The app runs passes on a thread of its own with its own
   engine over the same store, at the next due send or every 5 minutes, and at once
   after a send, `g`, or a new account. *Settings window* (`,`, the sidebar, and by
   itself when there is no account): accounts with their state, add, remove (second
   press; local mail goes), screener per account; notifications on/off, quiet hours
   (local time; what came in is summed up afterwards), VIPs (notify through quiet hours
   if chosen, and always count as a person). Tests: a configured account syncs and sends
   in one pass against Dovecot through the couriers; arcad keeps a typed password for
   IMAP and SMTP without the app seeing it; settings window without a display; quiet
   hours, notifications off and VIPs in the notifier. **Not yet**: DNS SRV/MX from the
   app (Nuntius is HTTP only, so those steps find nothing), removing an account's Arca
   items with it (built later: arcad `DELETE_FOR`, see M8), the OAuth sign-in sheet (Microsoft accounts are found but can't be
   added; SRV and MX came later with Nuntius lookups), JMAP needing an API token as its password (bearer only; an API host other
   than the session's is refused; uploads up to 1 MiB), IMAP IDLE (built later, see M8).
9. **M8** later items: OpenPGP, lanes, cards, private addresses, big-file links, local
   assistance.
   **Built on the host (2026-11-18)**: lanes, cards, private addresses and OpenPGP.
   *Lanes* (E10, `19720979`): People, Updates and Newsletters as views, sorted by the
   person's own rules first (`from:@github.com updates`, `list:… newsletters`,
   `subject:…`; up to 500, readable in Settings), then VIPs, people they wrote to,
   lists, automated mail and machine-looking senders; the last message of a
   conversation says which lane and why, with "Always …" to make a rule. *Cards*
   (§14.3, `7c4acfbe`): the jail reads schema.org JSON-LD for orders, deliveries,
   flights, trains, buses, hotels, tables, events and rental cars into a bounded card
   (no URLs kept; 16 fields of 200 characters), shown above the text only when the
   sender is verified (so in practice once DKIM lookups are wired). *Private
   addresses* (§13.3, E12, `ba95fba5`): with a catch-all domain of the person's own,
   Settings makes `site-k3qx@domain` per site (copied for pasting), remembers addresses
   first seen in new mail with their sender, replies come from the address written
   to, the reader says which address it came to and warns when it came from someone
   else, and one turned off sends its mail to Trash. *OpenPGP* (§10.2, `f6bb3e47`):
   new crate `mail/mail-pgp` over rpgp (MIT/Apache; all 153 dependencies' licences
   checked). The jail takes one `Autocrypt:` header per message, only for a key whose
   address is From, and sends it in its own frame; the engine keeps 3 keys per address
   (a key for the person's own address only from their Sent folder). The jail checks a
   PGP/MIME signature at opening with the sender's keys (bare-LF copies as CRLF); a
   message read before its key arrived is parsed again once it is there. The reader
   says "Signed with OpenPGP by … · key 89AB CDEF …", or no key, wrong address, or not
   good. Encrypt uses S/MIME when everyone has a certificate, else PGP/MIME (RFC 3156,
   SEIPD AES-256) when everyone has a key, with the person's own key for the Sent copy
   (learned from their sent mail, or pasted in Settings). Tests: jail 2, engine 2,
   mail-pgp 1, schema 1, window and settings. **Not yet**: reading PGP/MIME encrypted
   mail and signing with OpenPGP (the private key belongs in Arca, which has no OpenPGP
   ops; the reader says so), inline PGP, Autocrypt's prefer-encrypt and gossip, key
   expiry and revocation and a keys page (built 2026-11-19, below); cards need DKIM lookups to show;
   lane counts (built 2026-11-19, below) and notifications per lane. *Big-file links* need a share-link service
   (Loculus or Crypta) and *local assistance* a local model service, neither of which
   exists yet.
   **DKIM and DMARC lookups built on the host (2026-11-18)** (Shankar: "DNS lookups through
   Nuntius"): Nuntius gained a `Lookup` request for TXT, SRV and MX (nuntius.md §3.6;
   `nuntiusd` asks the system resolver over sockets connected before its sandbox). The
   engine answers the jail's `Query` through it (`mail_engine::dns`): the DKIM record with
   a key, or the `v=DMARC1` record; a name with none is answered empty, so the sender is
   *not verified* rather than pending forever; a failed lookup leaves it *pending*, and a
   pending message is checked again when opened (the new verdict is kept). Answers are
   kept in memory for 6 hours (1 hour when there is none), so a sender's DNS sees at most
   one lookup per key every few hours, from the resolver, never one per message; mail
   fetched headers-only asks nothing. Both the sync thread and the window have a fetcher
   for this, opened when first needed. Account setup now gets SRV and MX through the same
   request. Tests: engine 1 (verified with one lookup for two messages, no key published,
   a resolver that doesn't answer then a recheck on opening), fetcher 3, wire 1. **Not
   yet**: a VM run against local-unbound, keeping answers across restarts, the provider's
   own verdict (Authentication-Results) for accounts at known providers.
   **Push by IMAP IDLE built on the host (2026-11-18)**: each IMAP account that signs in
   gets a watcher on a courier connection of its own (`mail_engine::accounts::watch_inbox`),
   waiting in `IDLE` on the inbox and renewing it every 25 minutes (RFC 2177's 29, the
   courier's 31-minute limit). A change the server reports wakes the sync thread for that
   account alone; several wakes at once are one pass. A server without `IDLE` is polled
   every 5 minutes as before; a failed connection is retried after 1, 2, 4, 8 then 15
   minutes; a watch is restarted when the account's servers or sign-in change, and
   stopped when the account is removed or needs signing in again. With push for every
   account, a full pass (sends, drafts, other folders) runs every 30 minutes instead of 5.
   Tests: Dovecot 2 (a new message is reported within seconds; shutting the connection
   ends the watch), app 2 (a watcher wakes its account and stops; a server without IDLE
   is polled). The window's "Get mail" still runs a pass at once. Microsoft sign-in waits
   for Navigator's sign-in sheet (Shankar, 2026-11-18).
   **JMAP push built on the host (2026-11-18)**: the HTTP courier passes an event
   stream on one event at a time, secrets masked (arca.md §5.5). A JMAP account's
   watcher listens on the session's `eventSourceUrl` for `Email`, `Mailbox` and
   `EmailDelivery` with a ping a minute; when the account's states move it wakes a pass
   for that account (once on connecting too, since the first states may be news). A
   stream the server ends is opened again, unless it ended within 30 seconds, which
   backs off like a failure; a session without an event source is polled. Tests:
   courier 1 (events split across chunks, the token masked inside one), engine 1 (state
   changes only, other accounts ignored, no event source, a stream ended), app 1. **Not
   yet**: IDLE on folders other than the inbox, mains/battery intervals, a VM run of
   either push against real servers.
   **Removing an account removes its sign-in (2026-11-18)**: arcad has a new app op,
   `DELETE_FOR` (`endpoint | id`), for the app's own items bound to a server (Arca's
   plain `DELETE` reaches only items whose recipient is the app). When an account is
   removed, Mail asks Arca to delete each of its items, IMAP's and SMTP's (or JMAP's),
   off the window's thread, and says whether the password went too; if Arca is locked or
   not running, the account and its mail are still removed and the note says to remove
   the password in Arca. Tests: arcad 1 (own endpoint only, not another app's, plain
   DELETE refused), app 2.
   **IMAP sync in its own process (2026-11-18)**: `mail-sync-worker` is Mail's `sync`
   worker (`[[workers]]`, `lifetime = "session"`, app-composition.md §4): a jail of its
   own as a uid of its own with no capability, one per IMAP session. It runs the
   session and parses every byte the server sends. The engine keeps the courier and
   relays its bytes to the worker over the worker's standard input and output, and the
   worker asks the engine for what sync needs from the store, call by call
   (`mail_sync::remote`: framed postcard, 64 MiB at most; a quiet server's wait is the
   engine's, passed on so an `IDLE` renews). Sync, the draft copies, the Sent copy and
   the IDLE watch all go through it. Off Atrium, Mail runs `mail-sync-worker` beside
   itself as a plain child, and with neither, the session runs in Mail's process as
   before. Tests: the wire 3 (the same store calls as in-process, APPEND, IDLE with news
   and with a passed wait, a dead worker is an error), Dovecot 1 (two passes with sync
   in the worker; the whole Dovecot suite passes that way too). **Not yet**: one uid per
   account (workers have one uid per app worker, not per account); the engine is not yet a process of its own apart from
   the window. *Sending* went the same way the same day: each message goes out over a
   submission courier whose replies the worker reads (`Call::Submit`, one message per
   worker, which then ends); a refusal comes back as the same error as before. Tests:
   the wire 1 (sent, and a refused sender), and the Dovecot sends through the worker. *JMAP* followed: the worker runs the JMAP client and reads every JSON answer,
   and asks Mail for each HTTP exchange (`SinkCall::Get`/`Post`), which Mail makes
   through Arca's courier as before, so the worker still has no network and no token.
   Sync and sending go through it, and so does the EventSource watch: Mail opens the
   stream through the courier and hands each event to the worker
   (`SinkCall::Events`, `SinkAnswer::Event`), which parses it and says whether it was
   news (`SinkCall::Pushed`); Mail answers whether to keep watching. So Mail's own
   process reads no mail server's replies at all. Tests: the wire 2 (the same session,
   errors and HTTP exchanges as in-process; the same verdicts on a scripted stream,
   stopped and ended), and the engine's JMAP suite, push included, passes through the
   worker.

   **Address book and recipient completion (2026-11-18, §14.2)**: the store keeps the
   persona's address book (`mail_store::people`): each address the person wrote to, the
   name it was last written with, how many messages and when last. A message counts once
   by its Message-ID, so sending it and later seeing its copy in Sent doesn't count
   twice. It fills when a send is handed over (names from the draft), from new mail in
   Sent folders as sync stores it, and once at start from the Sent mail already synced
   when the book is empty (a store from before). Completion matches the start of the
   address, its domain, a part of its local name, or a word of the name, and ranks by
   messages halved for each month of quiet. In compose, the To, Cc and Bcc fields show up
   to five people under the field with the caret, for the address being typed at its
   end. The arrows pick, Enter or Tab or a click takes one (a comma follows for the
   next), and Escape puts them away for what was typed. Tests: store 1 (counting, names,
   matching, ranking, forgetting), the engine's JMAP send (in the book once, Bcc too), and
   the compose window 1 (typing, arrows, Enter, Tab, Escape, no suggestions without the
   caret). **Not yet**: imported vCards and CardDAV (§17 D8), and a page to see and
   forget people (built later, see the People section below).

   **Platform SDK (2026-11-18)**: the manifest names the roles Mail embeds
   (`embeds = ["picker", "doc-viewer", "pdf-viewer", "media-player", "credential"]`), so it
   no longer keeps every role as an unlisted first-party app. `cargo atrium check` lists
   nine parsers linked into the core; none of them runs on received bytes. They arrive with
   the compose editor (pergola-richtext, officina-text). html5ever and pulldown-cmark come
   in through navigator-render and navigator-dom, which the editor uses only for the font
   set and image limits; Mail never asks them for HTML or Markdown. Its own drafts' HTML is
   read back by `rich::from_html`, and a received HTML body is only ever text-scanned
   after the parse jail has normalized it. usvg and roxmltree come with navigator-image,
   which decodes only pictures in a document, and Mail's mail profile has none. Paste is
   plain text. allsorts, rustybuzz and ttf-parser read the system's pinned fonts, never a
   message's. flate2 and brotli serve those fonts' WOFF tables, and flate2 also serves
   OpenPGP encryption of outgoing mail. Signatures are checked in the parse jail, and
   decrypting isn't built. One thing to watch: `give_pgp_key`, a key the person pastes,
   would parse in the core once the UI calls it. It belongs in the parse worker when that
   comes. Mail was then installed from its signed bundle on the arm64
   VM (2026-11-18): `cargo atrium bundle` (37 MB: atrium-mail, mail-parse-jail,
   mail-sync-worker), `opifex side-load` (trusted publisher). On kernel.atrium with the
   filter in deny mode, Mail started from the bundle, and parse and sync spawned as uids
   of their own in jails with no addresses and only unix sockets. Start time was the
   same before and after. Not yet: an amd64 bundle (no amd64 sysroot on the Mac), and
   `vm-workers-check.sh` as written (amd64 cloud VM).

   **Local time (2026-11-18)**: Mail's times follow the time zone the system gives the app
   (`mail_engine::clock`: localtime(3), so `TZ`, else `/etc/localtime`; with neither,
   UTC as before). That covers the list's and reader's dates, Send later's "tomorrow at
   08:00" and "Monday at 08:00", Later's evening, morning, weekend and next-week times,
   the daily review's day and hour, quiet hours, and the days of `before:` and `after:`.
   A wall-clock time becomes a moment with the offset of that moment, so a
   daylight-saving change in between is right. Tests: the date and choice functions on
   wall-clock times, and a test that what is chosen is what is shown; both suites pass
   under UTC, Asia/Kolkata and America/New_York. On Atrium, app and worker jails get
   the machine's zone file as `/etc/localtime` (portcullis-jail, 9e07795); a changed zone
   reaches Mail on its next launch.

   **Quick filters (2026-11-18, §11.7)**: chips under the list's heading, combinable,
   wrapping to the list's width: *Unread*, *Flagged*, *To me* (the person in To, not
   list mail), *From people* (not lists or automated senders, or a VIP), *VIPs*, *Has
   attachment* (any message of a conversation), *Needs reply* (the conversation's last
   message is to the person, not from them, not automated and not answered), *Snoozed
   back* (Later items come back). They run on the local index (`mail_engine::quick`), in
   Conversations or Messages, looking through at most 5,000 rows; the count follows, and
   "Nothing matches these filters" says why a list is empty. Tests: the window 1 (each
   chip, two combined, none matching, and a reply in Sent ending *Needs reply*). **Grouping** (the same day): a control after the chips
   moves through no grouping, *By date* (Today, Yesterday, This week, Earlier this month,
   then by month, in local time) and *By sender* (one section per first sender, the
   newest sender first). Headers are lines of the list: a press folds one, a folded one
   keeps its row and unread count, and j/k pass over headers. Grouping runs after the
   filters. Tests: the window 1 (date sections, folding, j over a header, by sender, with
   a filter). Later the same day: **the grouping is kept per folder** (with Conversations
   or Messages, in the same store entry) and comes back after a restart; **by lane**
   (People, then Updates, then Newsletters) is offered in inboxes; and **Save as
   folder**, shown while filters are on, keeps them as a saved search named "Inbox ·
   Unread, To me" with the query `in:"INBOX" is:unread is:to-me`, then shows it. Search
   learned the filters that had no operator: `is:to-me`, `is:from-people`, `is:vip`,
   `is:needs-reply`, `is:back`. Tests: window (by lane, kept per folder and after a
   restart, Save as folder), engine 1 (the filters as operators and back). **Not yet**:
   grouping by account (no list spans accounts yet; it comes with a unified inbox), and
   the chips in a search's results.

   **Server-side search built on the host (2026-11-18, §11.2, §6.4).** A search's results
   have *Search server*: its words go to the sync thread, which asks each account's
   server, IMAP with `UID SEARCH TEXT …` in every folder a pass walks, JMAP with
   `Email/query` filtered by text, newest first, at most 50 messages. What the server
   finds that isn't here with its text is fetched in the sync worker (headers, then the
   whole message up to 2 MiB; JMAP places a message it never listed) and indexed, so the
   local search finds it; the results are run again with "3 more found on the server" or
   "Nothing more on the server". Once per query; typing a new one offers it again. The
   worker asks the store whether a message's text is here (a new `HasText` sink call).
   Tests: Dovecot through the worker ("body 250" not found here, found on the server, then
   here), the JMAP suite (a message not yet synced found and placed once), the worker's
   JMAP parity test, the window (the button, the hand-off, the note). **Not yet**:
   results from the server shown before they are fetched (the "on server" mark of §6.4),
   and operators other than words sent to the server (built 2026-11-19, below).

   **Later's remaining rules built on the host (2026-11-18, §11.4).** The daily review
   names every item a week or more overdue and every item pushed back three times or
   more ("Over a week overdue: send the numbers"), every day until it is answered. At 30
   days overdue an item moves to **Dropped** on the next refresh, with "1 item 30 days
   overdue dropped · Undo"; `x` drops an item by hand too. Dropped items are a sidebar
   view while there are any ("Dropped 09:21 · was Reply · note") and `u` (*Restore*) opens
   one again, back in the inbox with its flag; a drop is kept 90 days. An item pushed back
   three times shows *Do it now*, *Drop* and *Someday* in the reader in place of Done and
   Later. The **weekly look** posts once on Friday after 15:00 local time: what is due next
   week (Monday to Sunday) and *Waiting* items set aside more than 7 days ago; a switch in
   Settings turns it off. The Later sheet has a one-line **note** (`n`, typed in place;
   Enter ends it), shown when the item comes back. Tests: engine 1 (asked, dropped, undo,
   forgotten after 90 days, review and weekly texts, the setting), window 1 (note typed,
   `x`, Dropped view, `u`, the 30-day drop and its Undo). The same day: **a flag set
   elsewhere** (a message arriving flagged, or a flags report turning `\Flagged` on)
   becomes a *Reply* item due tomorrow morning, except in Sent, Drafts, Junk and Trash,
   when the conversation has an item, or while an op of Mail's own on that message is
   still to be sent (so a Done's unflag isn't undone by a stale report). A **Read item
   closes** once its conversation has been open 5 s with the reader at its end (all on
   screen or scrolled to the bottom), where Mail draws the message itself. Tests: engine 1
   (arrives flagged, flagged later, a stale report after Done, Trash), the Dovecot suite
   (a message flagged on the server leaves the inbox as a Later item), window 1. Then
   **`$Snoozed`**: setting a conversation aside puts `\Flagged` and `$Snoozed` on its
   newest message in one op, and coming back (its time, mail to the person, Do it now,
   Done, Drop) takes `$Snoozed` off everywhere in the conversation; the store keeps it as
   a flag bit, IMAP and JMAP carry it as a keyword. **Unflagging elsewhere** closes the
   item as Done once nothing in the conversation is flagged (again not while Mail's own op
   is pending). Tests: engine 1 (only `$Snoozed` sent for a message already flagged, the
   phone's unflag closing it and `$Snoozed` coming off), the JMAP suite (the keyword
   reaches the server, an unflag on the server brings Plan back), the Dovecot suite
   (Dovecot takes the keyword). **Not yet**: an HTML message read to its end (the
   doc-viewer doesn't report how far it was scrolled), the review's time as a setting,
   and the hidden server `Snoozed` folder (moving messages out of the server's inbox
   needs folder creation and a move back at the due time; `$Snoozed` alone covers
   clients that know the keyword).

   **The rest of the notification settings built on the host (2026-11-18, §12, E5).**
   **Quiet all weekend**: Saturday and Sunday are quiet all day, alone or with quiet
   hours, which together make the working week of §12 ("weekdays 9:00–19:00"); what
   comes in is summed up when the quiet ends, as before. **Replies in my conversations
   notify too**: beside "VIPs notify in quiet hours too", off by default, it lets mail in
   a conversation the person wrote in through quiet hours. **The digest**: mail that
   isn't announced (lists, automated senders) is kept and said in one silent post,
   "Other mail · 4 messages, from …", at the person's times, 9:00 and 17:00 unless they
   type others ("08:30, 13:00, 18:00", at most six; empty: only counted); only what is
   still unread then is in it, and a time that falls in quiet hours waits for them to
   end. **The review's time**: the Later review comes at a typed time instead of 9:00.
   Settings has the two switches and the two time fields; a typed time takes effect once
   it reads as one. One Mail instance is one persona, so these are the persona's.
   Tests: engine 1 (the digest at 17:00 with only the unread and only once, no times
   meaning counted only, a Saturday held back but for a reply let through and summed up
   on Monday at 9:00, the review at 10:30, the settings kept), the window's time
   parsing. **Not yet**: Archive,
   Mark read and Reply on the notification (built later, below), sounds per account (a Praeco post has no sound yet), and
   showing the schedule in Curia's notification page.

   **First-contact and look-alike notes built on the host (2026-11-18, §7.5, §9.1).**
   Under an open message's sender line: **"First message from this address"** when the
   person has never written to it (the address book) and no other message here is from it
   (the search index's From terms, stopping at the first), not for the person's own
   addresses, and not on a message the screener holds (its line already says so). **"Looks
   like example.com, but is examp1e.com"**, in the danger colour, when the sender's domain
   isn't one the person corresponds with (domains written to, and the accounts' own) but
   has the same UTS #39 skeleton as one (`unicode-security`, MIT/Apache-2.0: `examp1e`,
   `rnicrosoft`, Cyrillic `а`) or is one edit from one (`exmaple.com`, `example.co`;
   domains under 6 characters aren't compared by edits); a look-alike wins over a first
   contact. Domains are shown as Monstrator shows them (`atrium_links::shown_host`: another
   script's letters as `xn--`). **Replying** (Reply, Reply all, not Forward) to such a
   sender puts the same line in the compose window's notes. Only what is here counts: a
   sender last heard from before the offline window began is new again. Tests: engine 2
   (edits and skeletons; a store with Priya written to: a stranger, `examp1e.com`,
   `exmaple.com`, a Cyrillic look-alike shown as `xn--`, the person's own domain looked
   like, the reply notes, a second message ending a first contact, the person's own mail),
   window 1 (the line drawn in a conversation). **Not yet**: the persona-specific link
   scanning of §7.6 and the recipient colouring for Work.

   **Notification buttons built on the host (2026-11-18, §12, tabula.md §5.5).** A
   one-message notification carries **Archive**, **Mark read** and **Reply** (grouped and
   summary posts carry none), posted with `praeco::client::post_with_actions` at
   `praeco::client::socket()`, so a jailed Mail finds praecod at its notify mount. Each
   action id is the verb and the message, `archive.42` (at most 28 of Praeco's 32
   characters), so Mail keeps no table of posts: a press that starts Mail (Praeco holds it
   30 s) or comes after a restart acts on the right message. A listener opens when Mail
   starts and reconnects every 5 s if praecod goes away. Archive takes the message out of
   the inbox and Mark read reads it everywhere it is, both with "Message archived · Undo" /
   "Marked read · Undo" in the window and on the server after the undo window, as from
   the keyboard; an archived message being read leaves the reader. Reply opens a compose
   window with the reply. A message gone since, or a verb Mail never offered, does
   nothing. Tests: engine 1 (ids within Praeco's rules and back, archive, read, reply, a
   gone message), window 1 (archive while reading it, its Undo, read, reply). The reply
   window is **raised** with the notification's press: once it is open and drawn, Mail
   calls `praeco::client::raise` with the notification's id, and forum-wm brings it to
   the front (praecod takes it once per press, within 110 s). **Not yet**: a run through
   praecod and Forum in the VM.

   **The command palette built on the host (2026-11-18, §15.5, E9).** **Ctrl-K** opens a
   box over the window: "Type a command or a place", then the entries that match, each
   with its key where it has one ("Archive · e"). It lists what makes sense now: Reply,
   Reply all, Forward, Archive, Delete, Later…, Print while reading, Done and Drop for a
   Later item, Restore for a dropped one, "Undo: Message archived" while there is one;
   Write, Search, Search the server (after a search), Get mail, Settings; in a folder,
   messages or conversations, each quick filter, grouping and Save filters as a folder;
   then "Go to …" every folder (with its account where two share a name), view and saved
   search. Typing matches letters in order, ignoring case, word starts and runs first
   (`ra` finds Reply all, `go inb` the inbox); nine are shown. Up and Down choose, Enter or
   a press does it and closes the box, Escape or Ctrl-K again closes it; while it is up,
   every key is the palette's, so letters that are keys elsewhere are typed. The search
   field shows "/   Ctrl-K". Tests: the matching (unit), window 1 (open, go to a folder,
   archive from it with its key shown, Undo from it, a filter, nothing matching, Escape,
   Ctrl-K twice, a press on Write). **Not yet**: the palette in compose windows, the
   switch for single-key shortcuts, and screen-reader names for its rows.

   **Window sizes built on the host (2026-11-18, §15.4).** **Wide** (≥ 1200 pt) is the
   three columns as before. **Medium** (800–1200 pt): the places fold to a rail of short
   names ("In", "Se", initials for names of two words) with a dot where there is unread
   mail, under ≡, *+* (Write) and */* (Search); ≡ shows the places in full over the list
   (Escape, or a choice, folds them), and they show while a search is typed so the box is
   seen. **Narrow** (< 800 pt): one column at a time. The list fills the window with ≡ at
   its header's left; opening a message gives the reader the whole window with *‹ Back*,
   and only Archive and Reply (and Delete and Later from 560 pt), the rest by key or
   Ctrl-K; Back or Escape returns to the list with the row still chosen, and j/k read on
   from there. The undo toast follows the column shown. The reader's text stays capped at
   about 80 characters and centred at every size (as before). The window can now be made
   as small as 320 pt wide. Tests: window 1 (the three sizes: what is drawn, ≡ and Escape,
   the search box over the list, Back, j after Back). Icons on the rail: built (see
   the 2026-11-19 entries). **Not yet**: a look at the narrow layout on
   a real display in the VM.

   **The address book in Settings built on the host (2026-11-18, §14.2).** Settings has
   a **People** section once anyone is in the book: "12 people you've written to,
   completed as you type", a *Find someone* field (matching as completion does: the
   address, its domain, a word of the name), and the likeliest four, each with its
   messages count and **Forget**. Forgetting takes the address out of completion, and out
   of what makes a sender known (§7.5), so its next message is a first contact again unless
   other mail from it is here. A forgotten person **stays forgotten** when a message already
   counted is seen again (a resync of Sent); only a new message to them puts them back.
   Tests: store 1 (forgotten, a resync, a new message), settings 1 (the section, finding,
   nothing found, Forget). **Not yet**: imported vCards and CardDAV (§17 D8), and a
   scrolling Settings window (built next, below).

   **Settings scrolls, and single keys can be turned off (2026-11-19, §15.5).** Everything
   under Settings' heading is in a Pergola scroll view (the heading and Done stay), so no
   list is cut off any more: every private address, VIP and section is reachable by wheel,
   touchpad or keys; Arca's password field scrolls with it (Pergola hides a credential slot
   a scroll view clips, so a field half out of view can't be typed into). A new
   **Keyboard** section has "Single-key shortcuts (j, k, e, r, …)", on by default as §15.5
   says; off, a letter or sign is no shortcut in the main window (so nothing happens by a
   stray key), while Ctrl-K and the palette, the arrows, Enter, Escape and Delete still work,
   and the Later sheet keeps its own keys. Kept in the instance's settings like the rest.
   Tests: engine 1 (kept), settings 1 (twelve private addresses and every section drawn
   with nothing cut off, the switch), window 1 (e ignored when off, Down and the palette
   still archive). Keys on buttons with single keys off: see below.

   **Recipients outside the company, in Work (2026-11-19, §9.1).** Mail asks portcullisd
   which persona it runs in at start (`Request::Persona`, by its uid from the launch
   registry, as the Navigator does). In a named persona (Work), a compose window says
   who is outside the From address's domain, under the fields in the warning colour:
   "Outside example.com: ada@other.org, sam@notexample.com", from To, Cc and Bcc as typed,
   each once; an address at a subdomain (`eu.example.com`) is inside, and a look-alike
   domain (`notexample.com`) is not. It follows the From address chosen and goes when
   everyone is inside. Personal says nothing. A compose window open before portcullisd
   answered is told when it does. The spec's wording was "coloured"; a line naming them
   is used because a text field can't colour single addresses in what is typed. Tests:
   engine 1 (inside, subdomain, look-alike, duplicates), compose window 1 (Personal
   silent, Work's line, all inside). **Not yet**: a VM run in a Work instance (company
   domains: see below).

   **Company link scanning kept in Work (2026-11-19, §7.6).** Settings has a LINKS
   section with a "Keep company link scanning" switch (stored as a setting; absent means
   the default, which is on in a named persona and off in Personal). When it is on, a
   link wrapped by Microsoft Safe Links, Proofpoint URL Defense or Mimecast opens as it
   came, so the company's scanner still sees the click; the link bar still shows the real
   destination and its domain, and its second line says "Opens through Microsoft Safe
   Links, your company's link scanning" (the text-mismatch warning, when there is one,
   still comes first). Turned off, or in Personal, such links are peeled as before.
   Mail learns its persona from portcullisd as in §9.1. Tests: links 1 (scanner named,
   original kept), prefs 1 (default by persona), window 1 (Work keeps the Safe Links
   link and names it, turning it off peels it). **Not yet**: a VM run in a Work
   instance (a company's own scanner: see below, typed in Settings).

   **Export as mbox (2026-11-19, §6.5).** The command palette has "Export Inbox as mbox"
   (the folder shown), "Export these results as mbox" (a search, view or saved search)
   and "Export all mail as mbox" (every folder of every account, each message once). The
   picker's Save opens with a suggested name (`Inbox.mbox`, `All mail.mbox`); Mail writes
   the file it hands back on a thread, oldest message first, as mboxrd (`From ` lines,
   body `From ` lines quoted with one more `>`, LF endings), which Thunderbird, mutt,
   Apple Mail and Dovecot read and which reads back byte for byte. Only messages whose
   whole text is on the machine are written; the rest are counted ("2 messages aren't
   downloaded yet, export again later for those"). `mail_engine::export` (`What`,
   `messages`, `mbox`, `split`). Tests: engine lib 3 (`From ` line date, quoting and
   reading back, names), engine 1 (folder, some messages, everything with a shared All
   Mail and a headers-only message), window 1 (palette entries, the ask, none while the
   picker is up). **Not yet**: Maildir (the picker hands one file, not a folder),
   messages the screener is holding, and import (next).

   **Import from mbox (2026-11-19, §6.5).** The palette's "Import an mbox file into Inbox"
   (the folder shown; not from search results) opens the picker for one file, read-only.
   Mail reads it on a thread as it goes (one message held at a time, so a file of
   gigabytes is fine), mboxrd or mboxo, CRLF or LF, text before the first `From ` line
   ignored, `>From ` unquoted; each message's headers are checked in a parse jail (refused
   ones, and any over 64 MiB, are counted as "weren't mail"), and one the account already
   has by Message-ID, or that came earlier in the file, is skipped, so importing the same
   file twice adds nothing. The rest are kept as blobs in a new `imports` table and go up
   to that folder on the server in the next sync pass, seen (IMAP `APPEND`; JMAP upload +
   `Email/import`, new `Jmap::import` and worker call `JmapImport`), at most 200 a pass;
   a pass that uploaded any syncs again so they list at once. A failed upload is kept
   with its reason and tried on later passes up to 5 times. The window says "Adding 120
   messages to Inbox; they upload as mail syncs; 3 messages already there" and asks for a
   pass. Mail never shows imported mail before the server has it: it comes back through
   sync like any other mail. Tests: engine lib 2 (splitting what export writes, CRLF and
   leading junk, a message over the limit), JMAP 1 (Lunch already there and a duplicate
   skipped, upload, synced back seen and byte for byte, a second import adds nothing, a
   folder the server lacks gives up after 5 with its reason), Dovecot 1 (an import into
   Sent goes up by `APPEND` in a real pass and is listed seen), window 1 (palette entry,
   not from search, not while the picker is up). **Not yet**: Maildir (the picker's folder
   kind could carry it), local-only folders (every import goes to a server); progress
   and given-up uploads: see below.

   **Junk and Not junk teach the server (2026-11-19, §7.7).** `!` (or "Move to Junk" in
   the palette) moves the message or conversation to the account's Junk folder and marks
   it `$Junk`, removing `$NotJunk`; read in Junk, the same key ("Not junk") moves it to
   the inbox and marks it `$NotJunk`, removing `$Junk`. Both wait out the undo window like
   Archive ("Message moved to Junk", z undoes). On IMAP the keyword change goes before the
   move in the same pass; on JMAP both are one `Email/set` patch. Servers whose filters
   read these keywords, or learn from moves into Junk, are taught either way. An account without a Junk
   folder says so. The store now keeps `$Junk`/`$NotJunk` as flag bits (64, 128) as it
   keeps `$Snoozed`. Mail still runs no spam classifier of its own. Tests: Dovecot 1 (to
   Junk with `$Junk` and back with `$NotJunk`, read back from the server; the test server
   gained a `\Junk` folder), JMAP 1 (mailbox and keywords on the server both ways),
   window 1 (`!`, undo, "Not junk" offered in Junk, the move queued), store (flags).
   **Not yet**: a Junk button on the reader's bar (key and palette only), and Report
   phishing.

   **The person's own rules (2026-11-19, §11.6).** Settings has a RULES section: one rule
   per line as typed, shown in words ("Mail from anyone at github.com goes to GitHub"),
   newest first, each with Remove. A rule is `from:` (an address or `@domain`, subdomains
   too), `to:` (an address or domain in To or Cc), `list:` (a `List-Id`) or `subject:`
   (words), then `move <folder>`, `flag`, `read` or `quiet`. On mail new to an inbox the
   first rule that fits is made at once (out of the list, flagged or read) and reaches
   the server at the next pass; `quiet` keeps it from notifying. Mail the screener holds,
   blocked lists and turned-off private addresses are left alone; other folders, and mail
   already here when a rule is added, aren't touched. The folder is found by name in the
   message's own account (exact, then any case); adding a rule whose folder no account
   has says so, and it moves nothing until one does. Rules are kept per instance (each
   persona has its own). `mail_engine::rules` (`parse_rule`, `Rule::words`,
   `Engine::rules`/`add_rule`/`set_rules`/`missing_folder`/`rule_quiet`), run from the
   arrival rules; notifications check `quiet`. Tests: engine lib 1 (reading and writing
   back, a subject with "move" in it, refusals), engine 1 (move, flag, read, quiet, a
   missing folder, new mail only, inbox only), settings 1 (add, words, a rule that doesn't
   read, remove). **Not yet**: rules on the server (ManageSieve, JMAP Sieve) so they run
   when the machine is off, vacation replies, "Always…" from the reader, and a rule run
   over mail already here.

   **Automatic reply while away (2026-11-19, §11.6).** Settings has an AWAY section (when
   there is an account): "Reply automatically", the last day (`2026-11-30` or empty), a
   subject and the reply, and Save; under it each account's line ("replying until 30 Nov
   2026", "set at the next sync", "off (its end date passed)", or why not). The server
   sends the reply, so it works with the machine off. JMAP accounts: `VacationResponse`
   (RFC 8621 §8, when the account has the capability), set and read back in each pass
   through the sync worker (`Jmap::vacation`, worker call `JmapVacation`); what is saved
   waits in the store (`away` table) until a pass sets it, and a failure is kept with its
   reason and tried again. The last day ends at local midnight (`clock::end_of_day`).
   IMAP accounts say their server takes automatic replies through ManageSieve, which Mail
   doesn't do yet, and nothing is asked of them. The reply is one line here (the form has
   no multi-line field); the server keeps what it is given. Tests: clock 1 (days in any
   zone, refusals), JMAP 1 (read off, set with a last day and a control character taken
   out, read back, the lines, turned off), Dovecot 1 (an IMAP account says ManageSieve and
   isn't asked), settings 1 (no text, a bad day, saved, the line). **Not yet**: ManageSieve
   (RFC 5804) through a courier for IMAP accounts, which also carries the rules to the
   server, a start date, and a multi-line reply.

   **Filters on IMAP servers through ManageSieve (2026-11-19, §11.6).** An IMAP account's
   automatic reply and the person's rules now go to its server as one Sieve script
   (RFC 5228) named `atrium-mail`, put there through ManageSieve (RFC 5804), so both run
   with the machine off. Arca learned a `sieve` scheme (port 4190): its mail courier
   starts TLS (a server that doesn't offer it is refused), signs in with SASL PLAIN or
   XOAUTH2 and hands Mail the post-login capabilities and its own `OK`; its relay passes
   only the script commands (list, put, check, activate, rename, delete, space, log out),
   refuses a sign-in or a synchronising literal, and holds a script to 1 MiB. Adding an IMAP
   account has Arca keep the password for a third endpoint, the IMAP host's port 4190,
   except where the provider is known to have no ManageSieve (Gmail, Outlook, Yahoo,
   iCloud, AOL, GMX) or signs in by OAuth; the item is kept in a new store table (`sieve`)
   beside the account's settings and deleted from Arca with the account. `mail-proto`'s
   `sieve` reads responses (strings, literals, response codes) and writes the commands;
   `mail-sync`'s `sieve` writes the script for the server's extensions and puts it, in the
   sync worker (worker call `Sieve`, like a submission). The reply is `vacation :days 1`
   with the subject and the account's addresses, inside `currentdate` checks in the
   person's zone when the server has `date` and `relational`; otherwise Mail writes it in
   or leaves it out by the day. Each rule is an `if` that ends in `stop`, newest first as
   here: `fileinto` the folder's server name, `addflag \Flagged` or `\Seen`; `quiet`, a
   folder the account lacks, or an action the server can't do only stops (Mail still does
   those itself, as before). In each pass, after mail, Mail sends the script only when it
   would differ from what the server last got; a script someone else made active (in
   webmail, say) is never replaced, and the account's line says so; a refusal is kept with
   its reason and tried again after an hour or when the reply or rules change; a server
   whose Sieve has no `vacation` says it can't send automatic replies, and the rules still
   go. Accounts added before this have no sign-in for the filters and say to add the
   account again. Tests: Arca courier 2 (signed in after STARTTLS and scripts pass; a wrong
   password, and a server without STARTTLS refused) and relay filter 1, `mail-proto` 2
   (responses a byte at a time, commands, refusals), `mail-sync` 2 (the reply by date or by
   day, rules) and worker 1, engine 1 (nothing asked with nothing to say, the script made
   active, unchanged not sent, someone else's script left alone, a refusal retried, no
   `vacation`, removal), app 1 (the third endpoint kept, known providers and OAuth
   without, all three deleted). Since built below: a run against Dovecot with Pigeonhole
   (host and VM), JMAP Sieve (RFC 9661) for rules on JMAP accounts, and reading back a
   script changed on the server.

   **The automatic reply's first day and its own lines (2026-11-19, §11.6).** AWAY in
   Settings gained a first day (`2026-11-23`, or empty: from now), checked like the last
   and refused when it comes after it, and the reply is now a box of five lines that keeps
   its line breaks (the compose window's text area, now `compose::text_area`, shared).
   The first day goes to JMAP as `fromDate` and into the Sieve script's `currentdate`
   check (or the day-by-day choice); the account's line says "replying from 23 Nov 2026
   until 30 Nov 2026" while the first day is still to come. `clock::start_of_day` beside
   `end_of_day`. Tests: clock (a day's first second), JMAP 1 (fromDate set, the line
   before and after it), settings 1 (a bad first day, one after the last, two lines kept).

   **Junk on the reader, "Always…", and a rule run over mail here (2026-11-19, §7.7,
   §11.6).** The reader's bar has *Junk* (or *Not junk* in Junk), key `!`, between Delete
   and Later. The palette, while a message is open, offers "Always: …" rules for it: for
   its list (`List-Id`) if it has one, else its sender; moved to each of its account's own
   folders and Archive, flagged, marked read, or quiet, each in words ("Always: Mail from
   lena@partner.example is flagged"). Choosing one keeps the rule (first, as from Settings)
   and runs it over the mail already in every inbox, with Undo for what that did ("Rule
   kept; 2 messages moved"); the reader closes if its message moved. Settings' rules each
   have *Run on the inbox* beside Remove. `Engine::run_rule` queues one op per message
   that fits (screener-held mail left alone, a flag already there not set again) due after
   the undo window; `Engine::always_for` gives the choices. Tests: engine 1 (a rule added
   leaves mail here alone until run; moved, flagged once, quiet does nothing; the choices
   for a sender), window 1 (an Always entry kept and run, with Undo), settings 1 (Run on
   the inbox with nothing that fits). **Not yet**: Report phishing.

   **Small leftovers (2026-11-19, §15.5, §11.7, §9.1, §6.5).** *The palette in compose
   windows*: Ctrl-K over a compose window offers Send (Ctrl-Enter), Send later…, Attach a
   file…, rich or plain text, Show Bcc, Save draft and Discard draft; it takes every key
   while up, so nothing typed reaches the fields; the palette's keys and drawing are now
   shared (`palette::key`, `view::draw_palette`). *Keys hidden with single keys off*: the
   reader's buttons and the palette no longer show single-key shortcuts when they are
   turned off (Ctrl-P still shows). *Chips on search results*: the quick-filter chips sit
   over a search's results too, each the search operator it stands for (`is:unread`,
   `has:attachment`…) added to or taken out of the words, so a saved search keeps them
   (`quick::toggle_in`, `bits_of`). *Recipients outside the company, in Work*: To, Cc or
   Bcc naming someone outside has its label in the warning colour with a dot ("To •"),
   beside the line that names them. *Export takes held mail*: a folder's export and "all
   mail" include what the screener holds. Tests: window 2 (chips toggled into a search's
   words and out; keys gone with single keys off), compose 2 (the palette: narrowing, Show
   Bcc, Escape, Send later, Discard; the marked labels), export 1 (a held message
   exported), quick 1. **Not yet**: screen-reader names for the palette's rows, and
   Maildir.

   **Open with… and Report phishing (2026-11-19, §8.2, §7.7).** An attachment Mail may
   open has *Open with…* beside *Open*: Mail asks portcullisd which apps open its type
   (`OpenApps`, which portcullisd now answers for an app holding `open-attachments`, not
   only the file manager; opening files stays the file manager's), shows them in a small
   sheet over the reader with the default marked, and a press on one sends the sealed
   memory file as before with `OpenAttachment`'s new `app`; portcullisd still decides the
   type from the bytes and refuses an app that doesn't open that type. Escape or Cancel
   closes the sheet. *Report phishing* (palette, while reading outside Junk) moves the
   message to Junk marked `$Junk` and `$Phishing` (flag bit 256, read back from IMAP and
   JMAP), with Undo. Tests: window 2 (the sheet: asked, finding, the choices and default,
   a press opening in the chosen app, none for a file whose name lies, closed; phishing
   reported with both keywords and undone, not offered in Junk), store (the keyword). The
   portcullisd change is checked for FreeBSD (`cargo check`); its run waits for the VM.
   Report phishing to the company in Work: built below.

   **Rail icons, phishing reports in Work, and files across mail (2026-11-19, §15.4,
   §7.7, §8.1).** The medium window's rail draws the folders every account has as icons
   (inbox, sent, drafts, archive, junk, trash, all mail, flagged: Phosphor tray, paper
   plane, note, archive, warning octagon, trash, stack, star); the person's own folders
   keep their two-letter short names, since one shared folder icon couldn't tell them
   apart. In a named persona Settings asks for the company's phishing report address
   (*Also send reports to*, kept once it reads as one address, lower case; empty turns it
   off). With one set, *Report phishing* also sends the conversation's messages there,
   each attached as it arrived (`message/rfc822`, 7bit as RFC 2046 asks; a message that
   isn't 7bit goes as an `.eml` file), with a line saying so, through the outbox; Undo
   takes the report back unsent along with the move. A message whose body isn't here
   yet is said, not sent. Files: a *Files* view (every message with files, each row
   listing them with sizes in place of the preview; any search with `has:attachment`
   shows rows the same way), *Files from <sender>* in the palette while reading (that
   search for the sender), and at the top of a conversation whose folded messages hold
   files, "N files in this conversation: …" with *Show them*, which opens those messages
   so their strips show (also *Show this conversation's files* in the palette). Tests:
   compose 1 (a message attached as it is, and as a file when not 7bit), engine report 1
   (sent from the account, headers kept, nothing for a headers-only message, refused
   for a non-address, undone), prefs (the address kept, bad ones refused), files 1,
   window 2 (rail icons; the report sent and undone; the conversation's files, Files from,
   the Files view), settings (the field, asked in Work). **Not yet**: a grid of
   thumbnails for the Files view (rows for now). Its counts: see below.

   **Search operators on the server, and rules on JMAP servers (2026-11-19, §11.2,
   §11.6).** *Search server* now sends the search's operators too, not only its words:
   `from:`, `to:` and `subject:` words in their fields, `before:`/`after:`, `is:unread` or
   `is:read`, `is:flagged`, `larger:`/`smaller:` (a `ServerQuery` the worker carries).
   IMAP gets them as `UID SEARCH` keys (FROM, TO, SUBJECT, SINCE/BEFORE on the local day,
   UNSEEN/SEEN, FLAGGED, LARGER/SMALLER), JMAP as an `Email/query` filter (an AND of
   conditions, `after`/`before`, `hasKeyword`/`notKeyword`, `minSize`/`maxSize`).
   `in:`, `has:attachment` and the quick filters stay local: the results are run again
   here once the server's finds are fetched. A JMAP account whose server keeps Sieve
   scripts (RFC 9661, `urn:ietf:params:jmap:sieve`) gets Mail's rules there as the script
   "atrium-mail" (written by the same `mail_sync::sieve` code as ManageSieve, for the
   server's own extensions), uploaded and made active in the pass when they change; the
   automatic reply stays the server's `VacationResponse`, so the script carries rules
   only. Another active script is left alone and said; a refusal (with the server's
   reason) is tried again in an hour or when a rule changes. Settings says under RULES,
   per account, when the rules aren't all on its server (another script, a refusal, rules
   the server's Sieve can't do, or a JMAP server without scripts, where they run only while
   Mail is open). Also fixed: the worker relay no longer fails a ManageSieve or send call
   when the server's last bytes arrive after the worker has answered (a flaky test).
   Tests: sync 1 (the query as IMAP keys and a JMAP filter), Dovecot (subject, read or
   unread, size and date sent to a real server), JMAP (an AND filter, a date; rules
   through Sieve: put, unchanged not put, updated, refused and retried, another script
   left alone; a server without Sieve), the window's server search. Reading back a script
   changed on the server, and the vacation reply through JMAP Sieve on a server with
   Sieve but no `VacationResponse`: built below.

   **Filters changed on the server, and the automatic reply through JMAP Sieve
   (2026-11-19, §11.6).** Before putting its script again, Mail reads its own script
   back (ManageSieve `GETSCRIPT`; JMAP `SieveScript/get` and the blob) and compares it
   with a fingerprint of what it last put (line ends aside). One changed on the server
   since (in webmail, say) is left as it is, and Settings says so under RULES with a
   *Replace* button, which puts Mail's script over it at the next pass; until then Mail
   looks again at most hourly. A JMAP server that keeps Sieve scripts but has no
   `VacationResponse` now gets the automatic reply in Mail's script, as an IMAP server
   does (the worker tells the engine which of the two a server has). Tests: sync 1 (the
   fingerprint), engine sieve (edited on the server: kept, said, not asked again within
   the hour, replaced, then Mail's own changes go through), JMAP 2 (edited, kept,
   replaced; the reply put in the script and taken out when turned off on a server
   without `VacationResponse`). What was changed is shown since: see below.

   **Company domains, import progress and the server's filters shown (2026-11-19,
   §§7.6, 6.5, 11.6).** Work's Settings has a COMPANY section (with the phishing
   address) where the company's other mail domains are typed, comma separated; a list
   that doesn't parse yet isn't saved. The outside-recipient line treats them, and their
   subdomains, as inside and then says "outside the company". An import says how far it
   has got every 500 messages ("Reading old.mbox: 1000 messages so far, 1000 new"), and
   only the last note wakes sync. Uploads that gave up after 5 tries are said under their
   account in Settings, with the last reason, *Try again* (back to 0 tries) and *Clear*
   (dropped; they stay in the file they came from). When Mail's filters were changed on
   the server, the worker brings back what is there (control characters out, at most
   16 KiB), the engine keeps it until the next put or *Replace*, and Settings shows it
   under RULES with a *Show*/*Hide* button beside *Replace*. Tests: engine compose 1
   (company domains), JMAP (stuck uploads tried again and cleared; progress at 500 and
   1000; the changed script kept and dropped on replace), sieve (the same over
   ManageSieve), Settings window (half-typed list not saved; stuck line with both
   buttons; Show and Hide), compose window (the company label).

   **ManageSieve against Pigeonhole, and on FreeBSD (2026-11-19, §11.6).** The Dovecot
   fixture turns on Pigeonhole's ManageSieve when it is installed. `filters_go_to_pigeonhole`
   puts a rule and the automatic reply through the real courier (STARTTLS, PLAIN), and
   Pigeonhole compiles and activates the script. `dovecot-lda` then files an invoice in
   Archive with it. An edit made on the server is kept and shown, and *Replace* puts
   Mail's script back. `scripts/mail-vm-check.sh` cross-builds the Dovecot tests with the
   parse jail and sync worker, and runs them on the dev VM against FreeBSD's
   dovecot 2.3.21.1 and dovecot-pigeonhole 0.5.21.1. All 8 passed on the Mac VM on
   2026-11-19, none skipped.

   **Key hints, the company's own link scanner, and the Files view's counts
   (2026-11-19, §§15.5, 7.6, 8.1).** With single-key shortcuts off, no single key is
   drawn anywhere they no longer work: the search box says only "Ctrl-K", the undo bar's
   *Undo* has no "z", and the Later bar's *Done*, *Later*, *Drop* and *Restore* have no
   keys (the Later sheet keeps its keys, since they still work there). Work's LINKS
   section has "Your company's own scanner": hosts, comma separated, each covering the
   hosts under it. With scanning kept, a link through one opens as it came. The bar
   shows the link it carries (in `url`, `u`, `target`, `dest`, `destination` or `q`)
   when it has one, else the scanner's own link, and says "Opens through your company's
   link scanner". The Files view's header counts what it lists: "12 files, 24 MB"
   (with a "+" past the newest 500 messages). Tests: links 1 (own scanner kept, named, carried link shown, opaque
   link, a look-alike host isn't one, off peels nothing), prefs (hosts kept, bad ones
   dropped), settings (the field), window (no "z" and Ctrl-K alone with single keys off;
   the Files header).

   **OpenPGP keys you have, expired and revoked (2026-11-19, §10.2).** mail-pgp reads
   when a key expires (from its newest self-signature), whether its owner revoked it, and
   which encryption subkeys are still live. Mail never encrypts to an expired or revoked
   key, or to an expired subkey. A recipient whose only key is stale counts as having no
   key, and sending says "(their key has expired or was revoked)". A pasted key that is
   already expired or revoked is refused with the reason. Settings' OPENPGP section lists
   every address Mail has a key for: the newest key's short fingerprint, "Pasted by you"
   or "From their mail", and when it expires, or "revoked by its owner" (stale keys in
   the warning colour). Each has *Forget*, and a new key comes with the person's next
   mail. Up to 50 are listed (pasted first, then most recently seen), with "and N more"
   after. Tests: pgp 1 (keys GnuPG 2.4 made: one expired, one lasting two years, one
   revoked; expiry, revocation, never encrypted to), engine pgp (pasting refused, a
   learned revoked key not encrypted to, the list's lines, Forget), settings (the list,
   Forget).

   **Lane counts, CAs you add, and the certificates Mail has (2026-11-19, §§15.2,
   10.1).** People, Updates and Newsletters show how many unread messages each has in
   the inbox, across every account. They are counted when the views are drawn, and only
   unread rows are classified, so a big inbox that has been read costs one listing.
   Settings has an S/MIME section. *Paste a CA* takes a PEM certificate authority (your
   company's, say). A certificate that isn't a CA, has expired or is already there is
   refused, and at most 20 are kept, in Mail's settings. Signatures are then checked
   against the system's roots and these, and a message whose issuer wasn't trusted is
   checked again when it is next opened. The same section lists every address with a
   certificate seen on its signed mail (name, issuer, until when, expired in the warning
   colour). Each CA and each certificate has *Forget*. Autocrypt's prefer-encrypt was
   left out: Mail can't read encrypted replies yet (Arca has no OpenPGP ops), so asking
   for them would only hurt. Tests: engine smime 1 (with no system roots a signature
   isn't trusted; a leaf refused as a CA; the fixture CA added, then the same signature
   valid; the seen certificate's line, Forget; the CA forgotten, untrusted again),
   settings (the CA pasted, listed, forgotten), window (counts under Newsletters and
   People).

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
