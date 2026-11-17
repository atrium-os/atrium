# Printing: Prelum

> *Prelum* (Latin): a press. The wine and olive press first; from the fifteenth century the
> printing press (*sub prelo*, "in the press"). Atrium's print system.

**Status:** proposed 2026-11-17, waiting for Shankar's review (§12 decisions 1–16). Nothing
is built.
**Scope:** how an app prints, how printers are found and added, how a document becomes
what a printer accepts, how it reaches the printer over the network or USB, what the user
sees while it prints, and where printer passwords live.
**Background:** Shankar 2026-11-17: "a printing subsystem based on Atrium's design
philosophy; obviously CUPS will not work". Officina (officina.md §14, §17 D12) and Folium
(folium.md §9 P7) already said a print service takes PDF and drives IPP Everywhere
printers through a jailed driver.

Related: [folium.md](folium.md) (the PDF engine that reads and draws the document),
[officina.md](officina.md) §14 (PDF export), [limen.md](limen.md) §2 (system roles),
[device-assignment.md](device-assignment.md) §4 (USB devices to jailed drivers),
[network-shares.md](network-shares.md) (the same shape for a LAN server holding a
password), [nuntius.md](nuntius.md) §4 (LAN reach as a grant), [arca.md](arca.md) §5
(courier rule), [personas.md](personas.md) §5.4, [vpn.md](vpn.md) §5.3 (routes per
persona), [curia.md](curia.md), [atrium-net.md](atrium-net.md).

## 1. Short answer

- **Driverless printers only.** Nearly every printer sold since about 2013 speaks IPP
  and takes a standard page format: IPP Everywhere (PWG Raster), AirPrint (Apple raster,
  URF) or Mopria, and most take PDF too. Atrium speaks those and nothing else: no PPDs, no
  vendor drivers, no PostScript interpreter, no Ghostscript. Older printers that only take
  PCL or PostScript from a vendor driver are not supported in the first version (§10).
- **Apps never see printers.** An app hands the system a PDF descriptor through a new
  Limen system role, `print`. The print sheet (drawn by Prelum, not the app) shows the
  preview, the printers and the options; the job starts only when the person presses
  Print there. The app learns "queued" or "cancelled", never which printer or what is on
  the LAN.
- **The app's PDF never reaches the printer.** A conversion jail reads it with Folium and
  writes a fresh document: either a clean PDF that holds only drawing (no scripts,
  forms, attachments, actions or links) or page rasters (PWG Raster, URF). Printers
  run old interpreters; a hostile file aimed at the printer, or using the printer to
  reach the LAN, stops at our own renderer.
- **Small jailed Rust pieces, one job each.** `prelumd` keeps the queues and the saved
  printers and parses nothing. `prelum-render` (per job, no network) turns PDF into what
  the printer takes. `prelum-ipp` (per printer connection) speaks IPP over HTTP to one
  pinned address and nothing else. `prelum-browse` (only while someone is looking for a
  printer) reads mDNS answers. `prelum-usb` (per USB printer, from dispensatord) carries
  IPP over USB. The print sheet `prelum-sheet` is the `print` role's provider.
- **Finding printers:** DNS-SD over mDNS for `_ipp._tcp` and `_ipps._tcp`, only while the
  print sheet or Curia's Printers page is open, query-only (Atrium announces nothing);
  plus "add by address" and USB plug-in. A discovered name never inherits a saved
  printer's trust: a saved printer is pinned by its UUID, its TLS key when it has one,
  and the network it was added on.
- **Encrypted where the printer allows.** IPPS (IPP over TLS) is used whenever the
  printer offers it, with its certificate pinned on first use, as SSH host keys are.
  Plain IPP still works for printers without TLS, and the sheet says the job crosses the
  network unencrypted.
- **Passwords in Arca.** Office printers that ask for a user name and password, and PIN
  ("secure release") jobs, get their secret from Arca, released to `prelum-ipp` only after
  it has checked the printer's pinned identity (arca.md §5).
- **Personas:** a saved printer belongs to one persona or is shared by the person's
  personas. A Work window's sheet shows Work's printers and shared ones, and the job
  travels on Work's route (through Work's VPN). Printing a Work document on a printer
  that is not Work's or shared is a crossing (personas.md §6), off by default.
- **What it costs us:** an IPP codec, a DNS-SD reader, three raster encoders, a PDF
  rewriter on top of Folium's writer, an HTTP-over-USB bridge, a sheet, a Curia page. All
  small; the large piece (reading and drawing PDF) Folium already is.

## 2. Why not CUPS, and what we keep from the standards

CUPS is a root-ish scheduler that runs filters as external programs, parses PPDs and
config files, listens on port 631, shares printers, and leans on Ghostscript (AGPL),
cups-filters (GPL and others), Avahi (LGPL) and vendor driver binaries. Its own direction
(CUPS 3, PAPPL, libcups3) is also driverless IPP; the stack around it is what Atrium does
not want: big, C, unconfined filters, a listening daemon, and licences the project avoids
(LICENSING-POLICY.md).

What the standards give us, all public PWG documents, implemented from the specification
(no code from CUPS or others):

| Standard | What we use |
|---|---|
| IPP/2.0, RFC 8010/8011 (PWG 5100.12) | the encoding (tagged attributes) and the operations of §6.3 |
| IPP Everywhere, PWG 5100.14 | the driverless contract: which attributes a printer must report, required formats |
| PWG Raster, PWG 5102.4 | the baseline page format every IPP Everywhere printer takes |
| IPP over USB, USB class 7/1/4 | HTTP over USB bulk endpoints |
| DNS-SD, RFC 6763; mDNS, RFC 6762 | `_ipp._tcp`, `_ipps._tcp`, subtypes `_print`, `_universal`; TXT keys |
| PWG 5100.13 (JPS3) | `job-password` (PIN printing), `printer-strings-uri`, finishings |
| IPP Job Extensions | `job-name`, `requesting-user-name` handling, `job-state-reasons` |
| URF ("Apple raster") | the format AirPrint printers take, documented in the open (PWG ipp-everywhere drafts, libcups3 under Apache 2.0, read as a specification only) |
| PCLm (Mopria) | later, only if a printer we meet needs it (§12 D6) |

## 3. What is at stake

| Asset | Where it is | Who must not get it |
|---|---|---|
| The document being printed | the app; the job spool; on the wire; in the printer | other apps, the LAN, a printer posing as the chosen one |
| Which printers exist, their names and places ("HR-3rd-floor") | the LAN; saved printers | apps (it fingerprints the person's home or employer) |
| A printer's password, PIN-print code | Arca | every app, prelumd, a printer posing as this one |
| Paper, toner, the printer itself | the printer | an app printing without the person; a document attacking the printer's firmware |
| The machine | kernel, other jails | a hostile printer, a hostile mDNS answer, a hostile USB device, a hostile PDF |
| Print history | prelumd's state | someone at the unlocked machine later; other personas |

## 4. Threats

| Attacker | Wants | What stops it (section) |
|---|---|---|
| **Malicious app** | print without the person (paper waste, sending data to a LAN device); learn the printers on the LAN; read other jobs | only the `print` sheet starts a job, with a press checked like other system sheets (§5.2); the app gets no printer names, no discovery, no status beyond its own job's outcome (§5.1); it never talks to prelumd |
| **Hostile document** (a PDF from the web or mail) | exploit the renderer; attack the printer through its interpreter (PostScript, PJL, PDF bugs); turn the printer into a LAN pivot | Folium in a per-job jail with no network and budgets (§6.1); the printer only ever receives a document we wrote (§6.2); no PJL, no PostScript, nothing passed through |
| **Hostile or spoofed printer on the LAN** | code execution from crafted IPP replies; steal documents by posing as "Office Printer"; steal its password | IPP parsing only in `prelum-ipp`, Rust, fuzzed, bounded (§6.3); a saved printer is pinned (UUID, TLS key, network) and a mismatch stops the job before any byte of it is sent (§7.3); passwords released only after the pin checks (§8) |
| **Spoofed discovery** | the person picks the attacker's printer | discovered printers are marked new and show their address; a new printer can be asked to identify itself (beep or flash, `Identify-Printer`) before the first job (§7.2) |
| **LAN eavesdropper** | read documents in flight | IPPS when offered, pinned (§7.3); plain IPP marked in the sheet; Work's persona can require IPPS (§9) |
| **Hostile mDNS traffic** | crash or exploit the browser; flood it | `prelum-browse` alone parses it, in a jail with only a multicast socket, bounded caches, only while a sheet is open (§7.1) |
| **Hostile USB device claiming to be a printer** | exploit the host through USB | `prelum-usb` in a per-device jail on `ugen` (device-assignment.md §4.1), only HTTP framing parsed there; IPP parsed in `prelum-ipp` as for the network (§7.4) |
| **Compromised `prelum-render`** | the network, other jobs, the user's files | it has one job's input fd and one output pipe; no network, no files, rctl CPU and memory caps; it dies with the job (§6.1) |
| **Compromised `prelum-ipp`** | other printers, credentials, the LAN | one printer's address in its pf anchor, one printer's credential at most; no files (§6.3) |
| **Someone at the machine later** | what was printed | history holds titles, printer, pages and time only, per persona, clearable; spooled pages are deleted when the job ends (§6.5) |

Out of scope: what a printer keeps on its own disk (many keep job copies), a printer the
person trusts misusing what it was sent, and an attacker who already has root here.

## 5. Apps and the print sheet

### 5.1 The `print` role

A new **system** role in limend's catalogue, provider `prelum-sheet`
(`org.atrium.prelum`). No app can declare it (limen.md §2.0). Its catalogue row says
`takes_fd`, like `doc-viewer` and `pdf-viewer`, so the parent may send one descriptor with
`Print`; limend checks it is a regular file or memory descriptor opened read-only and
narrows it with Capsicum (read, seek, fstat).

| direction | message | payload |
|---|---|---|
| parent → child | `Print` | with the PDF fd: `title` (at most 200 bytes), `page_size_hint`, `can_relayout: bool` |
| child → parent | `Relayout` | `paper: {w_pt, h_pt}, orientation, margins_pt`; only if `can_relayout` |
| parent → child | `Document` | with a new PDF fd, the answer to `Relayout` |
| child → parent | `Queued` | none (no printer name, no job id) |
| child → parent | `Cancelled` | none |
| child → parent | `Failed` | `why`, a fixed set: `not_pdf`, `empty`, `too_large` |

- **Why PDF:** it is already the one page format every app here can make (officina-pdf,
  Folium's writer, the Navigator's paged view), the person can see exactly what will
  print, and "Save as PDF" is then just another destination.
- **Relayout** lets an app that flows text (Write, Mail, the Navigator) lay itself out
  again when the person picks another paper size or orientation, instead of the sheet
  scaling the page. It tells the app only the paper size. Apps that can't, don't set the
  flag, and the sheet scales to fit.
- **Pergola helper:** `pergola::print(fd, title)` opens the slot and handles the replies,
  so most apps need one call.
- **Apps with nothing paged** (a picture viewer, a terminal) make a one-page PDF with the
  helper `pergola::print_image` / `print_text`, built on officina-pdf.

### 5.2 The sheet

`prelum-sheet` is one Pergola process per open sheet, jailed as `_prelum-sheet`, the
`plain` slot of limen.md §5 pinned over the parent window.

- **Left:** page previews. The sheet never parses the PDF itself: it asks prelumd for a
  preview job, and a `prelum-render` jail returns small page images (§6.1), so a hostile
  file can't reach the sheet.
- **Right:** printer (a list: saved printers of the caller's persona and shared ones,
  then "Nearby", from discovery, then USB, then Save as PDF), copies, pages (all, range,
  current), colour or black and white, two-sided, paper size and tray, orientation, scale
  (fit, 100 %, custom), pages per sheet, quality. Only options the chosen printer
  reports are shown (its `*-supported` attributes); nothing vendor-specific beyond what
  IPP describes.
- **Status under the printer:** ready, out of paper, low toner, offline, "not on this
  network", and for plain IPP: "Sent unencrypted on this network".
- **Print** needs a real press: the same rule as other system sheets (the button fully
  visible, not moved or newly shown in the last 500 ms, the window focused). Enter
  presses it only after the sheet has had focus for that long.
- **Save as PDF** writes the rewritten PDF (§6.2) through the Scrinium picker (save
  mode), so the app still never learns where it went.
- **Rate:** limend allows one open `print` slot per parent window; a parent that opens
  and closes the sheet repeatedly is slowed (one per 2 s after the third in a minute).

## 6. The job

```
 app ──fd──► limend ──► prelum-sheet ──(fd, ticket)──► prelumd ──fd──► prelum-render (per job, no net)
                                                         ▲   ◄──pipe──  pages / clean PDF
                                                         │
                                                         └──pipe──► prelum-ipp ──IPPS/IPP──► printer
                                                                      │  (or ── socketpair ──► prelum-usb ──ugen──► USB printer)
                                                                      └──► Arca (only after pin check)
```

### 6.1 Rendering: `prelum-render`

One process per job (and one per preview), started by prelumd through jaild:

- **Input** the job's PDF fd (read-only); **output** a pipe to prelumd; nothing else. No
  network, no filesystem, Capsicum after start, a sysfil profile like Folium's page
  jail, rctl caps (CPU time, 1 GB memory by default).
- **Folium** (`folium-syntax`, `folium-content`) reads the document under folium.md §6's
  budgets; a page over its time budget fails the job with "page N could not be
  prepared", it is never sent half drawn.
- **Raster** pages are drawn on the CPU with tiny-skia (BSD-3, already used by
  `folium-render`'s fallback) in **bands** (for example 256 rows), so a page at
  600 dpi on A3 never needs the whole bitmap in memory. Colour space sRGB 8-bit or
  sGray 8-bit, the IPP Everywhere baseline; no colour profiles from the document beyond
  what Folium already maps to sRGB.
- **Encoders** (each its own small crate, fuzz- and round-trip-tested): PWG Raster
  (PackBits-style line compression), URF, and later PCLm.
- **N-up, scaling, page ranges and two-sided order** are done here, so every printer
  gets the same result whatever its own support.
- **Preview** jobs return page images at sheet size, the first pages first.

### 6.2 What the printer receives

The printer's `document-format-supported` decides, in this order:

1. **`application/pdf`, rewritten.** `prelum-render` writes a new PDF with Folium's object
   writer (the one shared with officina-pdf): only page content streams, images and
   embedded font subsets, after Folium has parsed them; no JavaScript, forms, actions,
   links, annotations other than those flattened into the page, attachments, XFA,
   incremental updates or object streams from the original. Small files, the printer's
   own resolution, and text stays sharp.
2. **`image/pwg-raster`** (IPP Everywhere) at the printer's best supported resolution
   for the chosen quality.
3. **`image/urf`** (AirPrint) the same way.
4. **`application/PCLm`**: later (D6).

A printer known to print our rewritten PDF wrongly can be switched to raster in Curia
(per printer "Send pages as images"), and a short shipped quirk list does the same by
`printer-make-and-model` (§12 D5).

Never sent: the app's original PDF bytes, PostScript, PJL or vendor languages.

### 6.3 Talking to the printer: `prelum-ipp`

One process per printer connection, started by prelumd through jaild when a job or a
status query needs that printer, gone after an idle minute.

- **Network:** its own vnet (`Routed`), its epair in the caller's persona's routing table
  (vpn.md §5.3), a pf anchor passing only the printer's pinned address and port (631 or
  what the printer advertised). The address is resolved once before start (no DNS
  rebinding), the network-shares.md §4.3 rule. For a USB printer it has no network at
  all, only a socketpair to `prelum-usb`.
- **HTTP/1.1 and TLS:** the HTTP client code shared with Nuntius (`nuntius` crate:
  rustls, bounded headers, chunked bodies), POST with a streamed body.
- **IPP:** our own codec (`prelum-ipp-codec`), every length and count bounded before it is
  allocated, unknown attributes kept as opaque bounded values, a fuzz target. Operations
  used: `Get-Printer-Attributes`, `Validate-Job`, `Create-Job` + `Send-Document` (or
  `Print-Job` for printers without `Create-Job`), `Get-Job-Attributes`, `Cancel-Job`,
  `Identify-Printer`. No `Get-Jobs` across users, no subscriptions in the first version
  (polling while a job is active, every 2 s, then every 10 s).
- **What it tells a printer about us:** `requesting-user-name` = `atrium` unless the
  printer needs a real user name (set per printer, or from the Arca credential);
  `job-name` = the document title, or "Document" when the person turns titles off for
  that printer (office printers show job names to everyone at the panel); no host name.
- **Replies** become typed values for prelumd (state, state reasons, marker levels,
  media ready, job state), each checked against a fixed list; free text (`printer-info`,
  `printer-location`, `job-state-message`) is cut to 200 bytes of printable UTF-8 and
  shown only as plain text.

### 6.4 prelumd

`prelumd` runs as `_prelum`, one for the machine, on a new Aqueduct class `CLASS_PRINT`.
It holds:

- **saved printers** per person (`/var/db/atrium/<person>/printers.toml`, 0600): name,
  persona or shared, pin (§7.3), network, last attributes, per-printer settings;
- **queues**: one per printer, jobs in order, one active at a time per printer;
- **the state machine**: pending, preparing (render running), sending, printing (printer
  reports processing), done, cancelled, failed, held (needs the person: a PIN, a changed
  identity, paper);
- **clients:** `prelum-sheet` (submit, preview, list printers), Curia (`printers-configure`,
  restricted to Curia, the curia.md pattern), the Forum status item (read jobs, cancel),
  `prelumctl` for tests and the console.

It parses no document, no IPP and no mDNS: every outside byte reaches it only as a typed
message from a jail. It moves page data from `prelum-render`'s pipe to `prelum-ipp`'s
without looking at it.

### 6.5 Spool and history

- Pages are streamed render → send when the printer takes `Send-Document` while
  rendering continues; when they must be buffered (the printer is busy, a job is held),
  they go to a per-job file in the session's encrypted area (the place persona and
  session data use), capped (2 GB per person by default), and deleted when the job ends.
- A job left held at logout is cancelled and its spool deleted; nothing prints after the
  person has left.
- History: title (unless titles are off for that printer), printer, pages, time, result,
  per persona, kept 30 days, cleared from Curia. No document content.

### 6.6 What the person sees while it prints

- A **Forum status item** (forum-status-items.md) while any job is active: printer,
  progress (page N of M once rendering knows), Cancel.
- **Praeco notifications** only when the person must act or a job ends badly: out of
  paper, jam, offline for 2 minutes, "printer changed identity" (§7.3), PIN needed,
  failed. "Printed" is a quiet notification, shown in history but not as a toast,
  unless the person turns it on.

## 7. Printers

### 7.1 Finding them: `prelum-browse`

- **Runs only while someone is looking:** the print sheet's printer list is open, or
  Curia's Printers page is. Not at boot, not in the background. Saved printers are not
  rediscovered constantly: prelumd checks a saved printer with one
  `Get-Printer-Attributes` when the sheet opens.
- **Query-only.** It sends DNS-SD queries for `_ipp._tcp.local`, `_ipps._tcp.local`
  (and `_uscan._tcp` later for scanners) with the unicast-response bit and listens for
  answers; it never answers queries and Atrium announces nothing.
- **Socket from atrium-net.** The jail has no network of its own: atrium-net hands it a
  UDP socket already joined to 224.0.0.251 / ff02::fb on the active interface (a new
  `CLASS_NETCONF` call, restricted to prelumd), so the browser can't open other sockets
  or reach other interfaces. On a VPN-only persona there is no LAN discovery; printers
  there are added by address (or later by unicast DNS-SD in the office domain, §10).
- **Parsing:** our own DNS message parser, bounded (names, records per answer, TXT
  keys, total cache of 256 services, entries expire with their TTL), fuzzed. TXT keys
  read: `ty`, `note`, `pdl`, `URF`, `UUID`, `TLS`, `Color`, `Duplex`, `rp`, `kind`,
  `adminurl` (shown, never fetched automatically). Values are cut and checked as in §6.3.
- **Output** to prelumd: instance name, host, addresses, port, the TXT values above.
  prelumd then asks a `prelum-ipp` for `Get-Printer-Attributes` before the printer is
  offered, so what the sheet shows comes from the printer, not just from multicast.

### 7.2 Adding a printer

From the sheet (pick a nearby printer and print: it is saved, marked new) or from Curia
(nearby, by address `ipps://host[:port]/path`, `ipp://…` or a bare host name, or a USB
printer when plugged in).

- **New printers say so.** The first job to a printer shows its name, address, whether it
  is encrypted and, if it has TLS, its certificate fingerprint, with **Identify**
  (`Identify-Printer`: the printer beeps, flashes or prints a page, whatever it supports)
  so the person can tell the real printer from a spoof with the same name.
- **A name never inherits trust.** A discovered "Office Printer" that doesn't match the
  pin of the saved "Office Printer" is a new printer, shown as such, and never gets the
  saved one's credentials.
- **Persona:** a printer is added to the persona of the window it was added from, or to
  the persona chosen in Curia; Curia can make it shared (§9).

### 7.3 Identity and the pin

A saved network printer is pinned by:

- **its `printer-uuid`** (IPP Everywhere requires it; for older AirPrint printers the
  TXT `UUID`);
- **its TLS public key** when it speaks IPPS (most printers have a self-signed
  certificate; pinning the key works for those and survives certificate renewal if the
  key stays; a CA-signed name match is also accepted for printers that have one);
- **the network it was added on**, as atrium-net names the network (saved Wi-Fi network
  or wired link). On another network the printer shows "not on this network" and is not
  contacted: the same address in a café is someone else's machine.

Before any job byte or credential is sent, `prelum-ipp` checks the pin. A different key
or UUID holds the job and asks the person: "Office Printer's identity changed. If the
printer was replaced or reset this is expected." **Use the new identity** (re-pin) or
**Cancel**. Plain-IPP printers have only the UUID and the network, which a LAN attacker
can copy; that is why the sheet marks them unencrypted and why Work can require IPPS.

### 7.4 USB printers: `prelum-usb`

- **dispensatord** matches USB interface class 7, subclass 1, protocol 4 (IPP over USB)
  with a `devices.d` rule shipped in Prelum's system package, detaching `ulpt` from that
  device if it attached (device-assignment.md §4.3), and starts `prelum-usb` in a
  per-device jail on the device's `ugen` nodes, uid `_prelum-usb`, sysfil
  `device-ugen`, no network.
- **`prelum-usb`** claims the IPP-USB interfaces (a printer has at least two, one HTTP
  connection each) and bridges HTTP/1.1 between them and socketpairs prelumd hands to
  `prelum-ipp`. It parses only HTTP message framing (lengths, chunks), to know where one
  message ends on a stream that has no connection close.
- **Identity:** USB vendor, product and serial, plus `printer-uuid`. A USB printer is the
  session person's while it is plugged in, in the persona of whoever adds it (or shared).
- **Printers with only class 7/1/1–2** (the old unidirectional and bidirectional
  interfaces, PCL or PostScript inside) are listed in Curia's Devices page as "printer,
  not supported (needs a driver)" and get no owner.

## 8. Credentials

- **What printers ask for:** HTTP Basic or Digest over IPPS (office printers and print
  servers), a `job-password` for PIN or secure-release jobs, sometimes a user name for
  accounting.
- **Where they live:** Arca, under the persona the printer belongs to, recipient bound
  to the printer's pin (arca.md §3). The person types them into Arca's credential sheet
  when the printer first asks (`401` or `job-password-supported`), never into the print
  sheet or Curia.
- **Release:** `prelum-ipp` asks Arca only after TLS is up and the pin checked, Arca
  checks its label against the item's recipient, and the item is wiped after the
  request: the courier rule of arca.md §5, with `prelum-ipp` acting as that printer's
  courier. Plain-IPP printers never get a password (Basic over plain HTTP would hand it
  to the LAN); the sheet says why.
- **PIN print:** the PIN is per job by default (typed in Arca's sheet for that job,
  never stored); a person can save one per printer.

## 9. Personas

personas.md §5.4 left printers "following the instance that asked". With Work printers on
a company network and the home printer at home, that is not quite enough:

- **A saved printer belongs to one persona or is shared.** A printer added from a Work
  window is Work's; the home printer added from a Personal window is Personal's. Curia's
  Printers page can make a printer **shared** (one switch, with user verification, as for
  persona switches), which is what most people with a single home printer want.
- **The sheet shows** the caller's persona's printers, shared printers, and nearby ones
  (marked). Adding a nearby printer from a Work window makes it Work's.
- **Printing out of a persona** onto a printer that belongs to another persona is a
  crossing (personas.md §6): off by default, refused with the sheet saying which switch
  would allow it. Printing on a shared printer is not a crossing.
- **Route:** `prelum-ipp` for a Work job sits in Work's routing table, so an office
  printer reached through Work's VPN is reached only that way, and a Personal job never
  uses Work's VPN.
- **Work can require IPPS** (a per-persona switch, on by default for named personas once
  this is built): unencrypted printers are then shown greyed with the reason.
- **History** is per persona; removing a persona removes its printers, history and
  credentials with it.

## 10. Not in the first version

- **Printers that need drivers** (PCL-only, PostScript-only, GDI/host-based). A later
  "driver jail" could produce PCL 5/6 or PostScript from our own raster or from Folium's
  display list, written by us; vendor binaries and Gutenprint (GPL) are not an option.
  Most printers sold in the last ten years don't need this.
- **Scanning** (eSCL/AirScan, `_uscan._tcp`): same discovery, the same IPP-USB transport
  (eSCL is HTTP too), the same jails. Its own spec when asked.
- **Sharing a printer** from Atrium to other machines: nothing listens.
- **Wide-area DNS-SD** (company printers listed in the office DNS domain), WS-Discovery
  (Windows), LPD and raw port 9100, IPP subscriptions and notifications pushed by the
  printer, printer icons (`printer-icons` would need a decoder jail; a generic icon is
  shown), fax.
- **Booklets, posters, watermarks**: later sheet options built in `prelum-render`.

## 11. Build order

| Step | What | Done when |
|---|---|---|
| R0 | `prelum-ipp-codec`, `prelum-dnssd` parser, PWG Raster and URF encoders, as crates with fuzz targets and round-trip tests | fuzzers clean for an hour each; encoders round-trip against our own decoders and the PWG sample files |
| R1 | `prelum-render`: Folium → bands → PWG Raster / URF; PDF rewrite on Folium's writer; n-up, ranges, scale | a set of test PDFs (Officina exports, the folium corpus) render to stable golden images; the rewritten PDF reopens in Folium identical to the original's drawing |
| R2 | `prelumd`, `prelum-ipp`, `prelumctl print file.pdf --printer ipps://…`, add by address, pin, queues, spool | in the VM, jobs print to a test IPP Everywhere printer on the host (an IPP printer simulator, used only as a test oracle, not shipped), including identity change, cancel, offline |
| R3 | `prelum-browse` and atrium-net's multicast socket | the VM finds the simulated printer by mDNS; a flood of junk answers leaves it bounded |
| R4 | the `print` role in limend, `prelum-sheet` with previews, `pergola::print`; Officina, Atrium PDF, Mail and the Navigator wired | printing from each app in the VM; the press rule checked the way Curia's Apps page is |
| R5 | Curia Printers page, Forum status item, Praeco notifications, history | add, identify, share, remove, rename; out-of-paper and offline reported |
| R6 | `prelum-usb` and the dispensatord rule | a real IPP-USB printer prints from the Mac VM through USB passthrough, or on bare metal |
| R7 | Arca credentials and PIN jobs; personas (§9) | a password-protected IPPS printer prints with the password from Arca; a Work job refused on a Personal printer by default |
| R8 | a real printer on the network, by Shankar | a Wi-Fi printer prints from the VM over IPPS, found by mDNS |

R0–R2 need no hardware. R6 and R8 need a real printer (§12 D16).

## 12. Decisions, with recommendations

1. **Driverless only** (IPP Everywhere, AirPrint, Mopria; network and IPP over USB); no
   driver-based printers in the first version. *Recommend yes.*
2. **Name: Prelum** (`prelumd`, `prelum-render`, `prelum-ipp`, `prelum-browse`,
   `prelum-usb`, `prelum-sheet`), added to NAMING.md. *Recommend yes.* Alternative:
   *Typographeum* (longer, Greek-Latin).
3. **Apps print through a Limen system role `print`** carrying a PDF fd, the sheet drawn
   by Prelum, and apps learn only queued or cancelled. *Recommend yes.*
4. **PDF as the only job format from apps**, with optional `Relayout` for apps that flow
   text. *Recommend yes.*
5. **The app's PDF is never sent as is:** rewritten PDF first, raster when the printer
   takes no PDF or a per-printer or shipped quirk says so. *Recommend yes.*
   Alternative: raster always (simplest and safest for the printer, but larger jobs,
   slower, and text no sharper than the raster's resolution).
6. **Formats to build:** PDF rewrite, PWG Raster, URF; PCLm only if a printer we meet
   needs it. *Recommend yes.*
7. **A conversion jail per job, an IPP jail per printer connection, a browse jail only
   while looking, a USB jail per device; prelumd parses nothing.** *Recommend yes.*
8. **Own IPP codec and DNS-SD reader in Rust**, HTTP and TLS shared with Nuntius; no
   libcups, Avahi or mDNSResponder. *Recommend yes.*
9. **Discovery only while the sheet or Curia's page is open, query-only, through a
   multicast socket handed in by atrium-net.** *Recommend yes.*
10. **Saved printers pinned by UUID, TLS key and network; an identity change holds the
    job and asks.** *Recommend yes.*
11. **IPPS whenever offered; plain IPP allowed but marked, and never given a password.**
    *Recommend yes.* Alternative: refuse plain IPP everywhere (many cheap home printers
    then couldn't be used).
12. **Credentials and PINs in Arca, released only after the pin check (courier rule).**
    *Recommend yes.*
13. **Printers belong to a persona or are shared; printing onto another persona's
    printer is a crossing, off by default; Work may require IPPS (on by default).**
    *Recommend yes.* This amends personas.md §5.4.
14. **What we tell printers:** user name `atrium` unless configured, document title as
    the job name with a per-printer switch to hide it. *Recommend yes.*
15. **Spool in the session's encrypted area, deleted at job end; held jobs cancelled at
    logout; history without content, 30 days.** *Recommend yes.*
16. **Test hardware:** R2–R5 against an IPP printer simulator on the host (test only);
    for R6 and R8, which printer do you have (model, Wi-Fi or USB)? *Recommend*
    *simulator first, then your printer.*
