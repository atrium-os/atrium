# Printing: Prelum

> *Prelum* (Latin): a press. The wine and olive press first; from the fifteenth century the
> printing press (*sub prelo*, "in the press"). Atrium's print system.

**Status:** accepted 2026-11-17 (Shankar: all §12 decisions as recommended; D16: a printer
simulator first, his own printer for R6 and R8). R0 and R1 built 2026-11-17 (§11.1).
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
- **HTTP/1.1 and TLS:** rustls, and the bounded response reader shared with Nuntius
  (`navigator-fetch::http`: headers, lengths, chunked bodies); the POST with the
  document as a chunked body is Prelum's own, since Nuntius only fetches (§11.2).
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

`prelumd` runs as `_prelum`, one for the machine, on a new Aqueduct class `CLASS_PRINT` (20).
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

### 11.1 R0 and R1 as built (2026-11-17)

Crates under `prelum/` (one workspace, Rust, `#![forbid(unsafe_code)]`):

- **`prelum-ipp-codec`**: IPP/2.x encode and decode (RFC 8010), every value
  type including nested collections; decoding bounded by `Limits` (bytes,
  attributes, values, depth, groups) before anything is allocated, unknown
  tags kept as opaque bytes, the extension tag refused. `ops` builds the
  requests of §6.3 (Get-Printer-Attributes, Validate-Job, Create-Job,
  Send-Document, Print-Job, Get-Job-Attributes, Cancel-Job, Identify-Printer)
  with the ticket of §6.3 (copies, sides, colour, media, quality, resolution;
  page ranges, scaling and layout stay in `prelum-render`) and the job
  identity of decision 14 (`atrium`, title or "Document", PIN). `printer`
  turns a printer's answer into checked values: keywords that aren't
  keywords dropped, lists capped at 256, free text cut to 200 bytes with
  control and bidi characters removed, state reasons mapped onto a fixed set
  with their severity, `printer-uuid` accepted only as a UUID, the format
  choice of §6.2, URF resolutions and back-side rule, and `printer-resolution`
  sent only when the printer lists it.
- **`prelum-dnssd`**: the mDNS query (unicast-response bit) and a response
  parser: compression pointers followed only backwards, at most 16 jumps,
  names under 255 bytes, at most 256 records; queries refused (the browser
  answers nobody). `Browser` keeps PTR, SRV, TXT and address records with
  their TTL (capped at 75 minutes, goodbyes honoured), at most 256 services,
  256 hosts, 8 addresses each, and gives complete services with the TXT keys
  of §7.1 cleaned.
- **`prelum-raster`**: PWG Raster and URF writers that take one row at a time
  (line repeats up to 256, repeat and literal packets), with the PWG 5102.4
  header (PageSize, colour space, transforms, PrintQuality, page size name)
  and the 32-byte URF header; bounded readers for round trips and fuzzing.
- **`prelum-render`** (R1): the conversion step outside its jail.
  `render` reads the PDF with Folium, lays the selected pages out on the
  printer's paper (1, 2 or 4 per sheet; fit, actual size shrunk to fit, or a
  percentage; a page turned counter-clockwise when its shape matches the
  paper better, as IPP's `landscape`) and draws each sheet in bands of 256
  rows with Folium's CPU renderer straight into the PWG or URF writer; the
  back of a two-sided sheet is drawn mirrored as the printer's
  `pwg-raster-document-sheet-back` (or URF `DM`) asks and the transform is
  recorded in the header. Time budget per page; cancellable. `pdf::rewrite`
  writes the clean PDF of §6.2 with Folium's object writer: one new page per
  sheet placing the source pages as Form XObjects (the same layout as the
  raster path), every content stream re-tokenised and written back keeping
  only PDF's content operators, resources deep-copied with actions,
  JavaScript, annotations, forms, XFA, attachments, metadata and structure
  removed (pages, catalogs and the page tree can't be reached through a
  resource), every stream decoded and written again (Flate; JPEG kept),
  optional-content settings kept so the same layers print. What it can't
  carry yet returns `NeedsRaster` and the job is sent as raster: inline
  images, Type 3 fonts, JPEG 2000, JBIG2 and CCITT images. Font programs are
  copied after Folium has read them, not re-subset (later).

Measured (release build, one core of the cloud container): an A4 colour
page with text, shapes and fine lines at 600 dpi takes about 250 ms
(19.7 MB of PWG Raster for 6 pages); at 300 dpi gray two pages per sheet,
130 ms a sheet. Rewriting the same 6 pages as PDF: under 10 ms.

Checks:

- Unit tests: 11 (IPP), 7 (mDNS), 6 (raster), 13 (render and rewrite), all
  passing: round trips, every prefix of a message failing cleanly, bounds,
  layout positions to the pixel, back-side transforms for each rule, a
  hostile PDF (open action with JavaScript, attached file, link annotation,
  page actions, a launch action in a font, an unknown operator) rewritten
  with none of it left and drawing the same, the rewrite laid out like the
  raster path for 1, 2 and 4 per sheet.
- `scripts/prelum-r0-oracle.sh` (RUNBOOK §13.8b) against CUPS's
  `ippeveprinter`, an independent IPP implementation used only as a test
  oracle: every operation above accepted with plain successful-ok; its own
  raster reader (`ippeveps`) decodes our 2-page sRGB PWG Raster and 1-page
  sGray URF and the pixels are identical to what we drew; Avahi's mDNS
  answers about both test printers read by our parser into complete
  services with TXT keys. poppler (`pdftoppm`) reads the rewritten PDF and
  draws the same pages, the landscape page turned.
- Fuzzing (cargo-fuzz, one hour each, 2026-11-17, no crashes): IPP decode
  with a re-encode round trip, 46 M runs; mDNS packets through the browser
  cache, 11 M; raster write/read round trip, 6.7 M; raster read, 0.96 M;
  PDF rewrite read back by Folium (seeded with Folium's test PDFs), 0.87 M.
  The raster-write harness's first run stopped on a bug in the harness
  itself (it asked for pages past the writer's 100,000-row limit, which the
  writer rightly refused); fixed and rerun.

Found along the way:

- **Folium draws text in a different typeface when a page is turned a
  quarter** (`View.rotate` 90 or 270): the same standard-14 text that draws
  as Liberation Sans/Serif upright comes out in another face turned. Printing
  a landscape page on portrait paper by raster hits it; the PDF path doesn't
  (the printer draws the text). For Folium to fix.
- **Annotations, including filled form fields, are printed by neither path**
  until Folium draws them (folium.md P3/P5).
- The URF header's byte layout and the `DM1`–`DM4` back-side reading follow
  what other AirPrint clients do; `ippeveps` accepted the header, and a real
  AirPrint printer confirms both at R8.

### 11.2 R2 as built (2026-11-17)

New crates in `prelum/`:

- **`prelum-proto`**: the messages. Workers talk to prelumd over one socket
  (their stdin and stdout) in length-prefixed postcard frames, at most 1 MiB,
  the length checked before anything is allocated. `ipp`: a `Start` (the
  printer's URI, the address prelumd resolved, IPPS or not, the pinned key
  and UUID or none to learn them), then `Query`, `Identify`, `Submit` +
  `Data` + `End`, `Job`, `Cancel`, answered by typed events (`Printer`,
  `IdentityChanged`, `Unreachable`, `Refused`, `Failed`, `JobCreated`,
  `Sent`, `Job`, `Done`). `render`: `Started` (sheets and the format it
  chose), `Data`, `Sheet`, `Done`, `Failed`. `print`: **`CLASS_PRINT`, Aqueduct
  class 20** (19 is vpn.md's): printers, add, remove, rename, repin, identify,
  refresh, print (the PDF as the message's descriptor), jobs, cancel, release,
  with a client.
- **`prelum-ipp`**: the printer connection, library and jailed binary. One
  HTTP/1.1 POST per IPP request on a new connection, `Connection: close`;
  the answer's head and body read with the Navigator fetcher's bounded HTTP
  reader (`navigator-fetch::http`, the code Nuntius uses; it is GET-only, so
  the request side, with the document as a chunked body, is Prelum's own).
  TLS is rustls with a verifier that accepts exactly the pinned SPKI SHA-256
  (or, when adding, anything, and records it); the handshake is finished
  before the request is written, so a changed key stops the connection
  with no request byte sent (unit test: a TLS server counts zero bytes).
  `Submit` asks `Get-Printer-Attributes` first and refuses unless the
  `printer-uuid` is the pinned one and the printer lists the format; then
  `Create-Job` + `Send-Document`, or `Print-Job` for printers without them.
  A job that fails ends the worker, so a half-sent document is never closed
  as whole.
- **`prelum-render`** gained its binary: stdin the job's PDF (read with
  pread), stdout the socket, the settings as arguments; Capsicum on FreeBSD
  before the first byte is read. `--format pdf` holds the rewritten PDF
  until it is complete (up to 256 MB) and falls back to the printer's raster
  format when the rewrite says `NeedsRaster`.
- **`prelumd`** (and **`prelumctl`**): saved printers per uid in
  `<state>/<uid>/printers.toml` (0600), with the pin, the network, and the
  printer's last answer; one runner thread per printer with work, one job at
  a time. A job: `Query` with the pin (identity changed: **held**; no answer:
  **pending, "offline"**, tried again after 5 s, doubling to 30 s; not on the
  network it was added on: pending), then the plan (format of §6.2,
  resolution from the printer's list for the quality, colour only if the
  printer has it, media, sides, back-side rule, ticket), then prelum-render
  into a spool file (`<spool>/<uid>/<job>.spool`, 0600, 2 GB cap) while a
  second loop sends what is there to prelum-ipp as it grows, then polling
  `Get-Job-Attributes` every 2 s for a minute and every 10 s after, until the
  printer says completed, canceled or aborted. Cancel stops the workers and
  sends `Cancel-Job` once the printer has an id for the job. "Use the new
  identity" (`repin`) learns the printer again at its saved address and
  releases its held jobs. A printer's answer is cut again in prelumd (text,
  lists, keywords), since prelum-ipp is what a hostile printer would attack.
  Adding by a bare host tries IPPS on 631, then plain IPP; the sheet shows
  which (`NOT ENCRYPTED`). Workers run through jaild (`prelumd::jail`), or
  with `--direct BIN_DIR` as plain children for development off FreeBSD.
- **jaild `[print_workers]`**: `system-prelum-<uid>` at the instance root,
  uids 62000–62499, exactly `/usr/local/bin/atrium-prelum-render` (network
  `Disable` or `Isolated`) or `/usr/local/bin/atrium-prelum-ipp` (`Routed`
  whose grants are TCP to `/32` addresses on stated ports with
  `no_resolver`, nothing else; `no_resolver` is new in jaild's grants and
  leaves out the host-resolver pass a restricted outbound otherwise gets,
  since the worker is handed an address and DNS would only be a way out);
  the binaries are `atrium-`-prefixed because jaild runs nothing else from
  `/usr/local/bin`; both take the caller's stdio;
  no devices, FUSE, children, locked memory. Root: `/lib`, `/libexec`,
  `/usr/lib`, `/usr/local/bin` read-only, nothing else. System call classes
  `renderer` and `networked`. Optional rctl for render (`--rctl`: 1 GB, one
  CPU).

Checks:

- Unit tests: prelum-proto (frames: cut anywhere fails, oversize refused),
  prelum-ipp (the pin before any byte, learning, unreachable, URIs), prelumd
  (store round trip and mode, typed addresses, media names, the plan),
  jaild's validator (a print worker and only a print worker, 11 refusals).
- `scripts/prelum-r2-check.sh` on the host, workers unjailed, against three
  `ippeveprinter`s: 23 of 23. An IPPS printer added by URI with its key
  pinned (the same SHA-256 openssl reads from its certificate) and its UUID,
  a PDF job sent as the rewritten PDF (pdfinfo reads what it received); a
  plain-IPP raster printer marked unencrypted, the job arriving as PWG
  Raster; cancel while a slow printer prints, which gets `Cancel-Job`; the
  printer behind a relay that is pulled (job pending "offline", printer
  shown offline) and put back (job printed); the printer restarted with a
  new key (refresh shows the change, the job is held and nothing reaches the
  printer, repin pins the new key and the job prints); another printer at
  the plain printer's address (a new UUID) likewise.
- A 13-page document at 600 dpi to a raster printer: 20 MB of PWG Raster
  streamed from the spool as it was drawn, in 20 s including the printer's
  own simulated printing.
- `scripts/prelum-r2-vm.sh` in the arm64 FreeBSD 16-CURRENT cloud VM, workers
  jailed by jaild, printers on the host: 16 of 16. An IPPS printer added
  from an ipp jail with its key pinned; a PDF job and a raster job (the PWG
  Raster drawn in a render jail) printed; render jails seen with
  `ip4=disable`, ipp jails in their own vnet, each `atrium/<jail>` anchor
  passing exactly one rule, TCP to its printer's address and port (the
  first run found a pass to the host's resolver as well, which is why
  `no_resolver` exists); no worker jail left after its job; a new key held
  the job with nothing reaching the printer, and repin printed it; cancel
  while printing reached the printer as `Cancel-Job`. jaild's own tests:
  91 pass in the guest.

Not yet: the network pin uses a name given to prelumd (`--network`) until
atrium-net says which network this is; jobs and history are kept in
memory only (history is R5); IPv6 printers can't be granted to a worker
jail yet (jaild's grants are IPv4); CA-signed printer certificates are
pinned by key like the rest.

### 11.3 R3 as built (2026-11-18)

- **atrium-net** (built in the network thread, bdabbc7b): `CLASS_NETCONF`
  `PRINT_DISCOVERY` (0x50) on atrium-netconf's `print-discovery` socket
  (directory 0700, socket 0600, the peer must be root: prelumd). atrium-net's
  spawner, which is outside capability mode, makes two IPv4 sockets on the
  active interface and returns the interface name. The receive socket is
  bound to 5353, joined to 224.0.0.251, and shut for writing. The send
  socket is bound to 5353 and `connect`ed to 224.0.0.251:5353 (TTL 255,
  loop off), so it can't reach any other address. IPv4 only for now.
- **`prelum-browse`** (new crate and jailed binary): prelumd hands it both
  sockets over its stdio socket (one byte, `SCM_RIGHTS`). It then enters
  capability mode, so it can only `send` on the one, `recv` on the other,
  and talk to prelumd. QU questions for `_ipp._tcp` and `_ipps._tcp`, one per
  datagram, at 0, 1, 3, 7 … s, doubling to 20 s. Follow-up questions (SRV, TXT,
  A) go out at most once a second for instances that came without them,
  at most 16 per datagram. Answers are read by `prelum-dnssd`, whose cache
  is bounded (256 services). Complete services go to prelumd at most four
  times a second, and only when they change. Its jail is a print worker
  with no network (jaild's `[print_workers]` `browse_exec`), syscall class
  `renderer`, optional rctl 64 MB and a quarter of a CPU.
- **prelumd** `NEARBY` (`CLASS_PRINT` 0x07, `prelumctl nearby [--wait S]`):
  the first ask starts the browser, and later asks make it query again.
  Nobody asking for 60 s stops it. Each new service is confirmed by a
  `prelum-ipp` that learns its key, as adding does. At most 4 run at once,
  64 per session, and each gets 8 s. Only printers that answered are
  listed, under their own `printer-info` or model, with the announced name
  beside it, the key fingerprint, and "saved as N" when the UUID and key
  match a saved printer. A printer offering IPPS and IPP (same UUID and
  address) is listed once, by IPPS. The URI listed is what `ADD` takes.
  Addresses are IPv4 only. Loopback addresses, a path other than plain
  `[A-Za-z0-9/_.-]`, and port 0 are refused.
- `--mdns-legacy ADDR:PORT` (development, Linux): one socket connected to
  a responder (RFC 6762 §6.7). Avahi answers this way, though it fits a
  legacy answer into 512 bytes by leaving instances out, without setting TC.

Checks:

- Unit tests: `prelum-dnssd` follow-up questions and their bound;
  `prelum-browse` on loopback, where it finds a printer and asks for what a
  bare PTR left out. Under 3000 hostile datagrams (garbage, compression
  loops, 9.5 KB datagrams, 750 distinct printers) it holds at 256 services,
  keeps the first printer and still answers prelumd. `prelumd` checks where
  it asks a service; jaild's validator test has a browse worker and refuses
  one with a network.
- `scripts/prelum-r3-check.sh` on the host, against two ippeveprinters
  through Avahi: all pass. The checks: found; confirmed (the key is the
  one openssl reads); described by the printer itself; one printer offering
  IPPS and IPP listed once; a plain-IPP printer shown unencrypted; added by
  the listed URI and printed to; shown as saved afterwards; a printer that
  stops drops off the list.
- `scripts/prelum-r3-vm.sh` in the FreeBSD VM, with atrium-net on the fake
  LAN (`lan.py`'s `mdns-printer`, pointing at ippeveprinter on the host):
  18 of 18. atrium-net's sockets handed over. The printer is found on
  vtnet1 and confirmed over IPPS with the right key, listed by its own name,
  added, printed to, then shown as saved. The browse jail has
  `ip4=disable`, the process is in capability mode, and its only inet
  sockets are atrium-net's two. After `mdns-flood` (tens of thousands of
  hostile answers) the browser is alive at about 3.5 MB resident, the
  printer is still listed, and no junk is listed (none answered over IPP).
  Nobody looking for a minute stops the browser, and no jail is left.

Not yet: the network pin still uses `--network` (atrium-net doesn't name
networks to prelumd yet); IPv6 discovery and IPv6 printers; the VPN-only
persona rule (§7.1) waits for personas in R7; `_uscan` for scanners.

Next: R4, the `print` role, `prelum-sheet` and the apps.

### 11.4 R4 as built (2026-11-18)

- **Limen `print` role** (`limen::print`): a system role served by `_prelum-sheet`
  (uid 65046, `LIMEN_PRELUM_UID`), in a **sheet** slot (760 × 540) rather than §5.2's
  plain slot, so frescod centres it over the app, dims the app and frames it as a
  system sheet. `takes_fd` and `returns_fd`. Messages as in §5.1, plus `SavePdf { name }`
  and `Saved` (child to parent, below). A third-party app names `print` in its
  manifest's `embeds`; platform apps may embed any role. limend's rate rule as §5.2:
  one sheet per window, and after three opens in a minute one each 2 s (`RATE`).
  A sealed memfd (`F_SEAL_WRITE|GROW|SHRINK`) counts as read-only, so an app can hand
  over a PDF it made in memory without a file.
- **prelumd**: `PREVIEW` (small RGB images of the sheets asked for, from a
  `prelum-render` jail, at most 8 at a time, 400 px on the long side), `SAVE_PDF` (the
  §6.2 rewrite into an unlinked spool file, handed back as the reply's descriptor;
  layouts the rewrite can't keep are refused with "try all pages, one per sheet") and
  `FOR app-uid`: only `--sheet-uid` may send it, once per connection, and prelumd then
  acts for the app's person from the launch registry (`--registry`). Printers stay
  keyed by the person; per-persona printers are R7.
- **`atrium-prelum-sheet`**: one process for every open sheet, with a thread per
  sheet (§5.2 said a process per sheet; the sheet parses nothing, so one process is
  enough). Each sheet has its own window and its own prelumd connection after `FOR`.
  Left: previews from prelumd, with Previous and Next. Right: the destinations (saved
  printers, then nearby ones, then Save as PDF), copies, pages (all or a range), paper,
  pages per sheet, scale, with the printer's status line. Escape is Cancel.
  **Print** counts only a press frescod vouches for: the sheet guards the button's
  rect (`window_guard`) and spends the press as `PROMPT` on frescod's activation
  socket, which refuses a token that is unknown, old, or for a guard that was covered,
  moved or newly shown in the last 500 ms.
- **Save as PDF** goes back through the app: the sheet sends `SavePdf` with the
  rewritten PDF, then `Saved`, and `pergola::print` opens the picker's Save in the same
  slot and copies the PDF into the file the person picks. The app still never learns
  where it went.
- **`pergola::print`**: `Printing::start` (a file) or `start_bytes` (bytes, sealed in
  memory), then `handle` on every Embeds event, which gives `Relayout`, or the end:
  queued, saved (the name), cancelled or failed.
- **Apps**: Write (Print… and Ctrl+P: officina-pdf in memory, the document's own paper as
  the hint). Mail (Print and ⌃P in the reader: the message's subject, sender, recipients,
  date and text through `officina_pdf::plain`, laid out again when the sheet's paper
  changes; the HTML stays in the doc-viewer's jail, so pictures and formatting don't
  print yet. Ctrl+P in a PDF attachment's quick look prints the attachment). Atrium
  PDF (the `pdf-viewer` child sends `PrintAsked` on Ctrl+P and the parent prints the
  file it handed over; standalone, Ctrl+P prints the file). `prelum-print-demo` is the
  test app.

Checks:

- Unit tests: limen (the role, sealed PDFs, Save as PDF through the parent, one sheet
  per window and the rate), prelum-render previews, prelumd's layout checks and unnamed
  spool files, the sheet's state and view, `pergola::print` with a picker, Mail's ⌃P
  (in its window test with real parse jails), `officina_pdf::plain`.
- `scripts/prelum-r4-vm.sh` in the FreeBSD VM, with frescod headless, limend, the sheet
  as `_prelum-sheet`, prelumd's jailed workers and ippeveprinter on the host:
  25/25 pass (press rule, print, cancel, Save as PDF, rate, Write, Atrium PDF).

Sheets and Slides print (2026-11-18): `officina_pdf::sheet` pages a sheet down, then
across, each cell as the grid draws it (formatted text, fill, borders, data bars, "###"
for numbers that don't fit, "…" for text), at the screen's 96 px to the inch; a merged area prints as one cell, as the grid draws it, cut where it crosses a page, with its text on the page that has its first cell; Center Across Selection text is centred across its run with no lines inside, as on screen; frozen rows and columns repeat on every page as print titles; and
`officina_pdf::slides` draws a page per slide from the display list the editor draws
(hidden slides left out), fitted on the paper. Gradients are bands of solid colour,
since Folium (and so the render jail) skips shadings. Both answer the sheet's Relayout
with a new layout for the paper and orientation chosen. Gates in
`officina-pdf/tests/print.rs` draw the PDFs back with Folium; the VM check has
`sheets` and `slides` stages (`ONLY="sheets slides"` runs just those).

The Navigator prints too (2026-11-18, navigator-ui.md §7 "Printing"): the page's own
jailed child lays it out on the paper and writes the PDF, handed back to the window as a
sealed memory file, and it answers Relayout the same way.

Not yet: Sheets prints the current
sheet's used range only (no print areas, titles, charts or pictures yet); Mail prints text only;
colour, two-sided and tray options wait for R5's attributes; Scrinium's
quick look doesn't pass `PrintAsked` on yet; `print_image` and `print_text` helpers in
Pergola (`officina_pdf::plain` is the text half).

Next: R5.

### 11.5 R5 as built (2026-11-18)

- **Who may ask prelumd what.** prelumd looks each caller up in the launch registry
  (`person_of(uid)` now gives the app id too) and gives it a role: the **person**
  (not an app: `prelumctl`, tests) may do everything; **Curia** (`org.atrium.curia`)
  everything but `PRINT`, `PREVIEW` and `SAVE_PDF`; **prelum-status**
  (`org.atrium.prelum-status`) only `PRINTERS`, `JOBS`, `WATCH`, `CANCEL` and `RELEASE`;
  any other app is refused ("apps print through the print sheet"). The sheet keeps
  `FOR`. Each connection carries a persona, which `ADD` and `PRINT` record.
- **New ops:** `SHARE` 0x08 (`Share { printer, shared }`: a printer for every persona,
  or back to the persona it was added on), `HISTORY` 0x09 (ended jobs, newest first),
  `CLEAR_HISTORY` 0x0A, and `WATCH` 0x16 (give the last generation seen; the answer
  comes when the jobs change, or after 25 s, as `Watched { generation, jobs }`).
  `Printer` gains `persona` and `shared`; `Job` gains `alert` and `alert_since`.
- **Alerts.** A job that can't go on says why: `OutOfPaper`, `Jam`, `Open` (a door or
  tray), `Offline`, `IdentityChanged` (not the printer saved) or `Stopped` (any other
  reason). They come from the printer's `printer-state-reasons` when it is stopped
  before sending, or when the job stops while printing (prelumd then asks the printer
  why), from the connection failing (offline) and from the pin check. The alert goes
  when the job moves on; `alert_since` stays while it is the same alert.
- **History**, per person in `<state>/<uid>/history.toml`: title, printer, persona,
  sheets, how it ended, when. Never the document. Kept 30 days and at most 2000 jobs.
  A printer with titles off records "Document".
- **`prelum-status`**, a session component (one per person, started by ostiarius, keep
  alive, no network, `status-item` and `notify`): it follows `WATCH` and
  - shows a Forum status item while a job is active: a panel with each job, where it
    is ("Printing page 2 of 3"), a progress bar and a Cancel button; gone when none is;
  - posts a Praeco notice as itself when the person must act (HIGH): out of paper, a
    jam, a door open, not the printer saved, and offline once it has lasted 2 minutes
    (`PRELUM_OFFLINE_AFTER`); withdrawn when the job moves on;
  - posts "didn't print" (NORMAL) when a job fails and "Printed" (LOW, which the shelf
    keeps without a toast) when it prints; nothing for a cancel; jobs that ended before
    it started are not told again.
- **Curia's Printers page** (eighth in the sidebar, after Personas): each printer as a
  card with its address, state and whether it is shared or which persona it is on, and
  Identify, Rename (a field opens under the card), Share / Stop sharing and Remove…
  (pressed twice); Add by address and Find printers nearby; History with Clear
  history… (pressed twice). The page refreshes every 5 s while it is shown.
- **Capability `printers-configure`** (system only): mounts the print socket in the
  app's jail. Only Curia has it.

Checks: unit tests in prelumd (store round trip with history), prelum-status (notices,
the panel) and Curia (the page). Run `scripts/prelum-r5-vm.sh` against a running
FreeBSD guest with jaild. It stages its own frescod, forum-wm, praecod, Curia and
prelum-status processes. Two instances of `scripts/prelum-fake-printer.py` serve
IPPS and plain IPP. A state file makes the plain-IPP printer run out of paper.
The gate checks roles, printer management in Curia, notices and the status
item, cancellation, and document-free history. A test-only Sigillum endpoint
checks the account, printer name and minimum verification strength. It refuses
the first share request and approves the retry, and the gate checks that the
saved printer becomes shared only after approval. The R7 desktop check in §11.6
covers the real Sigillum sheet and PAM passcode verification.

The “Printed” notice still cannot be turned off, and the status item has no Release
button for held jobs.

### 11.6 R7 in progress

The picker lists saved printers owned by its calling persona and shared printers.
Discovery reports saved IDs only for printers the picker can use. prelumd checks
ownership again on submit and at the start of each attempt, including retries.
Curia lists every printer for the person. It asks for confirmation before
requesting that a printer be shared with every persona. Prelumd uses the
authenticated person's identity to ask Sigillum to verify the change, with
`SharePrinter` naming the saved printer and a minimum strength of
`BiometricSecure`. A refusal, unavailable verifier, or cancelled request
leaves the printer private. Prelumd persists approval only while the original
request and printer configuration are still current. Stop sharing removes
cross-persona access and cancels any pending approval without a ceremony.
The print sheet cannot change sharing or other printer administration settings.

For a saved IPPS printer, **Use PIN** opens Arca's credential field inside the
print sheet. Prelumd supplies the authenticated person, persona, TLS key and
UUID through `PIN_BINDING`. The PIN field accepts digits and shows bullets.
After the sheet counts the Print press, Arca stages the PIN and returns the
one-use handle. An empty or unavailable field leaves the job unsubmitted.

A PIN job enters via `CLASS_PRINT/PRINT_PIN` with the PDF and an opaque handle
from Arca. prelumd derives the person and persona from the connection and passes
the handle to the IPP worker. The PIN worker has a read-only mount of the Arca
socket directory. It redeems the handle only after an IPPS query verifies the
pinned TLS key and printer UUID and confirms support for PINs and split job
submission. It sends the PIN in `Create-Job` with attribute fidelity and sends
the document only after the printer accepts the request. The worker discards its
PIN copies after the request. Offline failures after submission begins require a
fresh handle if retried.

The host gate `prelum/prelum-ipp/tests/pin_ipps.rs` feeds the worker framed
requests, serves real arcad handlers over a Unix socket, and checks the printer's
TLS and IPP traffic. It covers changed TLS key and UUID, acceptance before
document upload, replay refusal, and cancellation without upload if the printer
reports it ignored or substituted attributes. It supplies the Arca peer roles in the
harness. The desktop gate drove Fresco input on the arm64 FreeBSD guest. A print-demo
window opened the sheet, an empty PIN queued no job, and a typed synthetic PIN
was staged in Arca and redeemed by the jailed worker. The IPPS simulator checked
the PIN in Create-Job before it accepted the PDF in Send-Document. The app
reported queued and Prelum reported Done. The fixture worker used an alternate
socket filename in the usual Arca mount so the resident Arca service stayed up.

**Printer login…** lets the owning persona set a username and password for a
saved IPPS printer. The username goes to Arca with a counted Save login press.
The password is typed inside Arca’s credential surface and stored while unlocked
under a printer recipient keyed by its TLS SPKI and IPP UUID. Saving again
updates the existing encrypted item.

Prelumd gets only an opaque one-use grant from Arca. The IPP worker makes an
unauthenticated identity query over pinned TLS, checks the UUID, and then redeems
the grant. It uses HTTP Basic for job requests over the same pinned IPPS
destination. Discovery must allow the unauthenticated identity query. A changed
identity cannot receive the password or the document.

The authenticated simulator gate passed on arm64 FreeBSD. A job without a
saved login got HTTP 401. The desktop stored a synthetic login in Arca. With
the simulator UUID changed, Prelum held the job before the worker redeemed
the grant. After the UUID was restored, Arca released the login to the jailed
worker and the simulator authenticated Create-Job and Send-Document. The
desktop job reached Done. The fixture used an alternate Arca socket in the
usual mount directory.

HTTP Digest, saved job PINs, Sigillum verification for sharing, and per-persona
routing and crossing policy remain open.

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
