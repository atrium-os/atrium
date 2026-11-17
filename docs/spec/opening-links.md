# Opening links: Monstrator, the link opener

Status: accepted 2026-11-15 (Shankar: "accept as recommended", all of §9 D1–D8). Building L1–L4.
Asked by Shankar (2026-11-15): the better long-term design for the Navigator's hand-offs
(navigator-ui.md §2.3a J4). Chosen: a separate, unprivileged, jailed service rather than
link handling inside portcullisd.

Related: [opening-files.md](opening-files.md) (the same rules for files; this is its twin
for links), [portcullis.md](portcullis.md) §3 (`open-uri`), [tabula.md](tabula.md) §2
(activation tokens), [navigator-ui.md](navigator-ui.md) §2.3a and §14 step 6 (hand-offs),
[atrium-navigator-threat-model.md](atrium-navigator-threat-model.md) §2.10,
[NAMING.md](../NAMING.md) (Nomenclator, `atrium-app://`).

## 1. Short answer

- One system service, **Monstrator** (`monstratord`, "the one who shows the way"), takes
  every request to open a link in another app: `mailto:` from a page, `https:` from a mail
  app, `atrium-app://` from anywhere.
- It runs **jailed, as its own system uid** (`_monstrator`, 65038), with no network, no
  files of the person's and no authority to launch anything. It parses the link (untrusted
  text stays out of root), picks the app, asks the person when that is a choice, and asks
  portcullisd to deliver it.
- **portcullisd stays the only launcher.** It checks the press, that the app declares the
  scheme and that the person's policy lets it run, then starts or finds the app and gives it
  the link on the app's existing document channel (opening-files.md §6.2).
- **Every open needs a press** in the asking app's window (a frescod activation token,
  new purpose `LINK`). An app in the background can't open anything.
- Apps **declare** the schemes they take in their signed manifest, like `[[opens]]` for
  types. The **default per scheme** is the person's choice; installing an app changes no
  default. `http`/`https` are the Navigator's alone.
- The Navigator's hand-offs (J4) become one call to Monstrator; `NAVIGATORD_OPENER` goes.

## 2. Why a separate service

| | in portcullisd | Monstrator (chosen) |
|---|---|---|
| Who parses link text from any app | root | a jailed uid with nothing to lose |
| Who draws the "Open with" sheet | root, or a UI process it trusts | Monstrator's own window, its own presses |
| What a bug in the opener gets | everything portcullisd can do | asking portcullisd to deliver a link the person pressed for, to an app that declares that scheme |
| portcullisd's code | grows a URL parser, a chooser and Nomenclator lookups | grows three small messages |

The second column keeps portcullisd as small as Atrium can make it, and keeps the rule
"root never reads untrusted text" true for links as it is for files (where Scrinium, not
portcullisd, reads the directory).

## 3. Threats

1. **An app opens links on its own** (spam windows, drive-by launches of a handler with a
   bug). Every open needs a `LINK` press in the asker's focused window, spent by portcullisd.
2. **An app makes itself the handler** for `https`, `mailto` or a scheme another app uses.
   Declaring only puts it on the list; defaults change only in Monstrator's sheet or Curia,
   each on a press; `http`/`https` can be declared by system-signed bundles only.
3. **A link that lies** (look-alike hosts, a long `mailto:` with a hidden body, control
   characters). Monstrator shows scheme and host only, the host in its ASCII (punycode) form
   when it mixes scripts, and refuses control characters, embedded newlines and links over
   8 KiB. (As built: a host is shown in its own letters only when every label is Latin
   script; anything else, including a whole label of Cyrillic look-alikes, is shown in its
   `xn--` form.)
4. **A link that is a command** (`file:`, `javascript:`, `data:`, `vbscript:`, `about:`,
   custom schemes that name programs). Those schemes are never taken (§4.2); no handler runs
   a program named by a link.
5. **A compromised Monstrator.** It can't launch, can't read files, has no network. Without
   a press it can do nothing; with one it can send that link to a wrong app among those that
   declare the scheme. Accepted.
6. **Learning what the person has installed.** The asker learns only "opened" or "not
   opened" (no app names, no list).

## 4. Schemes

### 4.1 The manifest

```toml
[[links]]
schemes = ["mailto"]

[[links]]
schemes = ["org.example.notes"]   # its own scheme
```

Validation (portcullis-toml): schemes are `[a-z][a-z0-9+.-]{0,31}`, at most 16 per app.
`http` and `https` only in system-signed bundles (the Navigator). The install screen says
"Opens links: email (mailto)". An update that adds schemes shows them again.

### 4.2 Never taken

`file`, `javascript`, `data`, `vbscript`, `about`, `blob`, `filesystem`, `jar`, `view-source`,
`ms-*`, `shell`, and anything that isn't a valid scheme. Monstrator refuses them before
anything else; portcullis-toml refuses them in manifests.

### 4.3 `atrium-app://`

`atrium-app://<app id>` opens an installed app (its start, no link given). Resolving a
publisher's name to a signed bundle that isn't installed is Nomenclator's work; until it
exists Monstrator answers "not installed", and later it hands that case to Opifex's install
screen ("Try it" there, not in the asking app).

### 4.4 Defaults

A table scheme → app id per person, next to the type table (`<user>/links.toml`, kept by
portcullisd like `opens.toml`). It starts from the system's: the Navigator for `http`/`https`,
nothing else until Atrium ships a mail app. A scheme with one app and no default asks once;
with several, the sheet lists them, and "Always use this app" sets the default. Curia's Default
apps page gets a Links section. portcullisd takes default changes only from Monstrator and
Curia, each with a press.

## 5. The flow

```
asker ── OpenLink { url, token } ─────────────────────────────▶ monstratord
monstratord ── LinkGrant { asker uid, token } ────────────────▶ portcullisd
portcullisd ── ACTIVATION_CONSUME(token, asker uid, LINK) ────▶ frescod
portcullisd ── Granted { grant, person, apps for scheme, default } ─▶ monstratord
           (monstratord shows its sheet if there is a choice; the person presses)
monstratord ── DeliverLink { grant, app, url } ───────────────▶ portcullisd
portcullisd ── starts or finds the app ── LINK { url } on opens.sock ─▶ app
monstratord ── Opened | NotOpened { why } ────────────────────▶ asker
```

- **The asker** sends the link with its press's token straight away (the token lives 1 s).
  `LINK` accepts `BUTTON` and `KEY` (Enter) presses, like `OPEN`.
- **monstratord** checks the link (§3.3, §4.2), takes the scheme, and asks portcullisd for a
  grant. It never spends the token itself, so a compromised Monstrator can't mint presses.
- **portcullisd** knows the asker by peer uid and the launch registry (person, app). It spends
  the token for that uid, and returns a grant: one link, that person, two minutes, once. It
  lists the apps that declare the scheme and that person's policy lets run, minus the asker.
- **The sheet** (Pergola, Monstrator's own frescod window, a system sheet like Arca's and
  Sigillum's): "Open this link in…", scheme and host, the apps, Cancel. Its presses are
  self-spent (`PROMPT`). No sheet when there is a default, or exactly one app the person has
  already confirmed.
- **Delivery** reuses the document channel: portcullisd serves `opens.sock` for every app that
  declares `[[opens]]` or `[[links]]`, and sends `LINK { url }` (atrium-open crate). The app
  gets the text only, no descriptor.
- Rate: one open per asker in flight, ten a minute; more are refused without a sheet.

## 6. Where it runs

- A system service (`etc/services.d`, started by portcullisd's bootstrap), as `_monstrator`
  (65038), one for the machine: each link's person is the asking app's, from the launch
  registry. Its jail mounts `/atrium/sockets/monstrator/` (served) and `portcullis/`, and for
  the sheet (L3) frescod's; nothing else; no network. *(As built: a services.d jail rather than
  a bundle with a restricted `link-opener` capability, since it isn't anyone's app.)*
- **`open-uri = true`** mounts `/atrium/sockets/monstrator/` into the app's jail (replacing the
  planned `broker.sock` in portcullis.md §3.2). `navigator-broker` mounts it too.

## 7. The Navigator (J4)

The page's hand-off prompt (navigator-ui.md §14 step 6) stays: navigatord still hands on only
a link it decided as a hand-off on that connection, once. Its prompt names no app ("Open in
another app?", with the scheme), since it can't know which. The UI's press on *Open* now goes
to navigatord as `Hand { url, how, token }`, and navigatord sends `OpenLink { url, token }` to
Monstrator instead of spending `PROMPT` and running `NAVIGATORD_OPENER`. The prompt row's
window is navigatord's chrome connection, so the press is in the asker's window by uid.
Links from other apps (`https:` from mail) arrive at the Navigator as `LINK` and open in a
new trail.

## 8. Pieces and build order

| piece | where |
|---|---|
| `[[links]]` schemes, refused schemes | portcullis-toml |
| `LINK` purpose, portcullisd as its consumer | frescod activation |
| `LinkGrant`, `DeliverLink`, defaults table, `LINK` on opens.sock | portcullisd, portcullis-ipc, atrium-open |
| `monstratord`, its sheet, `link-opener`, `open-uri` mount | new crate `monstrator/` |
| hand-offs through Monstrator | navigatord, navigator-ui |

1. **L1** manifest `[[links]]`, frescod `LINK`, portcullisd grant/deliver and defaults,
   `LINK` on the document channel; host tests. *Built 2026-11-15:* crate `atrium-links`
   (the scheme rules of §4.1–4.2 and Monstrator's link check, §3.3); portcullis-toml
   `[[links]]` (web schemes only with `navigator-broker`, at most 16, the install line
   "Opens links: …"); frescod purpose `LINK` (13), root its broker, a guarded window's guard
   checked as for `PROMPT`; portcullis-policy `LinkDefaults` (`<user>/links.toml`, the
   Navigator for `http`/`https`); portcullisd `LinkGrant` (only from `_monstrator`; the asker
   and its person from the launch registry; ten a minute per asking app; the press spent last)
   and `DeliverLink` (a grant once, two minutes, one of its apps, a link of its scheme in
   printable ASCII), an app with `[[links]]` gets the document channel, and `atrium-open`'s
   `LINK` message (`Channel::recv_any`; `recv` passes links over). Tests: atrium-links 3,
   portcullis-toml fixtures, frescod activation and credential 16/16, portcullisd opens and
   links 6/6 (in the VM).
2. **L2** `monstratord` in its jail with the link checks and the flow, no sheet (default or
   single app only); a test app bundle that takes `mailto`; VM gate. *Built 2026-11-15
   (host-tested; its VM run is L4's):* crate `monstrator/` (`monstratord`; `open_link` for
   apps; `monctl` for tests): one link at a time per asker, the link checked before
   portcullisd is asked, the default or the only app, logs that name the scheme and never
   the link; `38-atrium-monstratord.toml` and `scripts/monstrator-service.sh` (jailed as
   `_monstrator` 65038, devfs ruleset 4, no network, only its own and portcullisd's socket
   directories); `open-uri` and `navigator-broker` now mount `/atrium/sockets/monstrator/`;
   atrium-open's `open-probe` prints the links it gets.
3. **L3** the sheet and "Always use", Curia's Links section. *Built 2026-11-15:* with more
   than one app and no default, monstratord opens its own Dialog window on frescod ("Open
   this link in…", what the link shows, a row per app, "Always use this app for mailto
   links", Cancel; `monstrator/src/sheet.rs`, `view.rs`, laid out in points), its rows the
   window's guard. Up/Down/Tab move, Space ticks Always use, Enter or a click picks, Escape
   cancels; a pick counts only once frescod counts the press as `PROMPT` for
   `_monstrator` (a press that doesn't count says so under the text and asks again). One
   sheet at a time, open at most 110 s (under the grant's two minutes); navigatord waits
   130 s for the answer. Its jail mounts `fresco/` and `fresco-activation/`. Curia's Apps
   page has the Links section (curia.md §13): `Links` and `SetLinkDefault` on a press.
   `navigator-confined-vm.sh` 60/60 (two mail apps, pick by keys, Always use, the next link
   goes straight to the default); `vm-apps-access-check.sh` 17/17.
4. **L4** (= navigator-ui J4) navigatord's hand-offs through Monstrator, `NAVIGATORD_OPENER`
   removed; links into the Navigator open trails; `navigator-confined-vm.sh`. *Built
   2026-11-15 (before L3, which only adds the sheet):* navigatord sends a hand-off with its
   press to Monstrator (`NAVIGATORD_MONSTRATOR`, default its socket) instead of spending
   `PROMPT`; the prompt names no app (*Not now* · *Open*); *Try it* answers that trying waits
   for the app catalog. jaild's policy allows `monstrator/` and `portcullis/` socket
   directories for services. `navigator-confined-vm.sh` 55/55: monstratord from the
   bootstrap in its jail as 65038, a `mailto:` link pressed in the jailed Navigator reaches
   `org.test.mail` (open-probe) on its document channel, started for navtest by portcullisd.
   *Links into the Navigator, built 2026-11-15:* the Navigator's manifest takes `http` and
   `https` (`[[links]]`, `navigator-broker-bundle.sh`), so portcullisd gives navigatord a
   document channel; navigatord reads it on a thread, keeps only web addresses (printable,
   at most 8192 bytes), and sends each to the newest UI as `Event::Incoming { url }`,
   holding up to 16 until a UI has said Hello. The UI opens it in a recorded trail of its
   own and shows it. `navigator-live-vm.sh` hands navigatord a link through a stand-in
   for portcullisd's end (atrium-open's `link-serve`) before any UI is up, and the UI that
   comes up shows it in a new trail; `navigator-confined-vm.sh` checks the jailed
   navigatord takes up its channel, and (2026-11-17) the whole way: `link-probe`, a jailed
   app with a window and `open-uri`, sends an https link with an Enter in its window, and
   the jailed Navigator shows it in a trail of its own. *Brought forward, 2026-11-17:* when
   portcullisd hands a link to an app that is already running, it asks frescod (activation
   socket, new op `RAISE { uid }`) to bring that app's top window forward. frescod allows
   one raise per `LINK` press the caller spent, within two minutes, so a raise always
   follows a person's press; it picks the uid's top-level window highest in the stack (not
   a slot, pop-up or hidden window) and tells the WM alone (`EV_WINDOW_ACTIVATE`).
   forum-wm focuses it, switching to its space if it is on another. An app just started
   for a link needs none: a new document opens focused.
5. **L5** `atrium-app://` publishers through Nomenclator and Opifex, when those exist.

## 9. Decisions, with recommendations

| # | question | recommendation |
|---|---|---|
| D1 | Separate service or inside portcullisd | Separate, jailed (Shankar, 2026-11-15) |
| D2 | Name | Monstrator (`monstratord`, `_monstrator` 65038) |
| D3 | A press for every open | Yes, no background opens; notifications already give presses (tabula.md §2.4) |
| D4 | Who spends the press | portcullisd, so a compromised Monstrator can't open links on its own |
| D5 | `http`/`https` handlers | The Navigator only (system-signed bundles) |
| D6 | Does the asker learn the app | No: opened or not, nothing else |
| D7 | Move Scrinium's `OpenFile` here too | Not now: it passes descriptors portcullisd must narrow anyway; revisit if Monstrator's sheet should become the one "Open with" for files and links |
| D8 | Delivery channel | The existing `opens.sock` with a `LINK` message, not a second socket |
