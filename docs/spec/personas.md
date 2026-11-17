# Personas: work and personal side by side

**Status: accepted 2026-11-17** (Shankar: all §9 decisions as recommended). Nothing here is built yet.
**Scope:** keeping a person's work and personal lives apart on one machine, in one login:
app instances per persona, storage, removable devices and security tokens, what may cross
from one to the other, and how every window shows which side it is on. Network routing
per persona is in [vpn.md](vpn.md) §5.3 and §9.6.
**Background:** Shankar 2026-11-15, starting from the VPN design: "for our split-tunnel,
we need to be able to split our apps too properly... all clearly indicated so there is no
confusion"; then "what about a usb drive or a FIDO stick meant for companies? should we
restrict it to be copied only to storage meant for work? ... different home directory for
work so work and personal files don't get mixed?" Links crossing personas: off by default
(Shankar: "why should we not disable it by default?").

Related: [insula.md](insula.md) §13.6 (personas at the system level),
[navigator-ui.md](navigator-ui.md) §2.4 (personas as cookie jars), [homes.md](homes.md)
(home layout, workspaces, consoles), [scrinium.md](scrinium.md) §9 (home folders),
[device-assignment.md](device-assignment.md) §4.6–4.8 (sticks), [sigillum.md](sigillum.md)
§10 (tokens), [tabula.md](tabula.md) (clipboard), [arca.md](arca.md).

## 1. Short answer

- A **persona** is a named side of a person's life: **Personal** (always there, the
  default) and named ones such as **Work**. It has a colour, optionally a VPN, and its own
  rules for what may cross. Most people have only Personal and never see any of this.
- **An app runs once per persona.** Mail · Work and Mail · Personal are two instances: two
  jails, two uids, two data folders, two Arca personas, two network routes. A workspace
  belongs to one persona; its terminals with it.
- **Storage:** each named persona has its own folders (Documents, Downloads, Pictures,
  Music, Videos) in its own volume, with its own group and, once Tessera encrypts, its own
  key in Arca. An instance's file picker shows only its persona's places. Removing a
  persona erases its data by destroying its key.
- **Devices:** an encrypted stick can belong to a persona (its key lives in that persona's
  Arca). Work files are written only to Work places (Work folders, Work sticks) unless
  the person allows more. Plain sticks are neutral: anything may be read from them. A
  smartcard or token certificate can belong to a persona and signs only for it. FIDO keys
  stay shared by default and can be marked as one persona's.
- **Crossing** (links, files, clipboard, drag and drop, sharing) between personas is **off
  by default in both directions**. A switch per persona and direction allows it, and then
  each crossing is one press in a system sheet that shows what crosses. Turning a switch
  on needs user verification.
- **Indication** is drawn by frescod and Forum from what portcullisd registered, never by
  the app: a persona chip in every window's strip, a coloured frame for named personas,
  badges on icons and notifications.
- **What this is not:** protection against the person themselves (a determined person can
  photograph the screen). It stops mix-ups, mistakes and a compromised app on one side
  reaching the other side's data, network or credentials.

## 2. Personas

| | Personal | a named persona (e.g. Work) |
|---|---|---|
| exists | always, cannot be removed | made by the person in Curia |
| colour | none (neutral) | one of a fixed set; Curia refuses two personas with close colours |
| folders | the home folders (scrinium.md §9) | its own volume (§4) |
| network | direct, or a VPN | direct, or a VPN (vpn.md §9.6) |
| crossing rules | §6 | §6 |

**Who owns them.** portcullisd, which already owns per-person policy
(`/var/db/atrium/<person>/policy.toml`) and allocates per-app uids. The persona list and
each persona's rules live in that file. Curia's Personas page talks to portcullisd over
`CLASS_PORTCULLIS` with a restricted capability `personas-configure` (Curia only), and
follows Curia's rule for services going away (curia.md §10).

**Unrecorded** (navigator-ui.md §2.4) is not a persona in this sense: it is a throwaway
jar inside a Navigator window, and a window's Unrecorded trails belong to that window's
persona.

## 3. Instances

An app runs once per persona that uses it. Each instance is keyed by (person, app,
persona):

- **uid:** portcullisd allocates one per (person, app, persona), extending homes.md
  decision H4 (per (person, app)). The kernel, not only mounts, keeps Mail · Work out of
  Mail · Personal's files.
- **data:** `~/.atrium/apps/<id>/` for Personal (unchanged), `~/.atrium/apps/<id>@<persona>/`
  for a named persona.
- **Arca:** the instance's items live under its persona (arca.md personas): passwords,
  cookies, tokens of one never reach the other.
- **network:** the instance's epair sits in its persona's routing table (vpn.md §5.3).
- **launch registry:** the line for the instance's uid names the persona, so frescod,
  forum-wm, Tabula, Arca, Sigillum, dispensatord and jaild learn it from the peer uid,
  never from the app.

**Launching.** An app with instances in more than one persona asks which, with the last
used one preselected; a person with only Personal never sees the question. Each running
instance has its own icon.

**Navigator.** One persona per window: every trail in a Work window is Work, and its page,
converter, fetcher and courier jails are Work's (their uids are in Work's instance range,
their epairs in Work's routing table). A trail cannot move between windows of different
personas. Named personas were navigator-ui.md's D5 ("when someone asks"); Work makes them
needed.

**Workspaces and consoles** (homes.md §4, §5). A workspace belongs to one persona, chosen
when it is made and changeable only in its settings. Its volumes live under the persona
(`~/.atrium/workspaces/<persona>/<name>/`), its jail runs as a uid of that persona and its
network follows the persona. A console runs in the jail of the instance or workspace it is
attached to, so it has exactly that persona; portcullisd registers the terminal window as
"console of <instance>" so the strip shows the right persona. The console's environment
sets a prompt prefix (`[work]`) as a convenience; the strip is the proof. Workspace links
(homes.md §5.2) across personas follow §6.

**System apps span personas.** Scrinium, Curia, Forum and the picker are not instanced:
they show both sides, labelled, and are the only places where a crossing can happen,
under §6's rules.

## 4. Storage

### 4.1 Folders per persona

```
/home/<person>/
  Documents/ Downloads/ ...                   Personal (scrinium.md §9), group files-<person>
  .atrium/personas/<persona>/                  one volume per named persona
    Documents/ Downloads/ Pictures/ Music/ Videos/   group files-<person>-<persona>, 2770
```

- **Same home, not a second account.** A second login would split the session, the
  dock and the windows, and the point is working side by side. One person, one home, one
  volume per named persona inside it.
- **Groups.** Each persona has its own files group. Scrinium holds the groups of all of the
  person's personas (it spans, §3); no app instance holds any (files reach instances as
  descriptors through the picker, as today).
- **Picker.** The picker opened by a Work instance shows Work's folders and Work's sticks
  (§5.1) and neutral sticks; never Personal's folders. A download from a Work Navigator
  window goes to Work's Downloads.
- **Encryption.** Each persona volume will have its own key in Arca, under that persona,
  class after-first-unlock, once Tessera's at-rest encryption exists (tessera-fs.md flag
  `ENCRYPTED`, the same prerequisite as trusted sticks, device-assignment.md §4.6). Until
  then a persona volume is a separate Tessera volume with its own quota and group, without
  crypto-erase. Design proposed 2026-11-17 in [tessera-encryption.md](tessera-encryption.md):
  a persona's folders are encrypted domains on the internal Tessera volume, one key group
  per persona (and one for Personal), not a partition of their own.
- **Removing a persona** (Curia, with user verification): its instances stop, their
  overlays and workspaces are deleted, its Arca persona and volume key are destroyed, its
  devices are unbound (their data becomes unreadable here; Curia says so and lists them
  first). With encryption, this is the "leaving the job" button: nothing of Work is
  recoverable afterwards.

### 4.2 Backups

A persona volume is a unit: it can be backed up, or left out of a personal backup, on its
own. Backups are not designed yet; this only keeps the option open.

## 5. Devices

### 5.1 USB sticks and disks

Trust is a key, never an ID (device-assignment.md §4.6). So:

- **A persona's stick** is a Tessera-encrypted stick whose key is held in that persona's
  Arca. Scrinium's "Make this a Work stick" formats it so. Only Work instances get it
  through the picker, Scrinium shows it with Work's chip, and on another machine it is
  unreadable without the key. Needs Tessera encryption (§4.1).
- **A neutral stick** is everything else: FAT, exFAT, an unencrypted or foreign Tessera
  stick, an optical disc. Any persona may *read* from it through the picker (someone hands
  you a stick in a meeting; that is a deliberate pick).
- **Writing out of a persona.** A named persona's files may be written only to that
  persona's places: its folders and its sticks. Writing to a neutral stick, or to another
  persona's place, is a crossing (§6), off by default. Personal has no restriction on
  neutral sticks unless the person sets one.
- **Network shares** (network-shares.md) are places too: a share can be bound to a persona
  (its credentials then live in that persona's Arca), with the same rules.

### 5.2 Smartcards, PIV and USB certificate tokens

A company card's certificate should sign only for work. Sigillum already binds a
certificate to an endpoint (`TokenKeyRef`, sigillum.md §10.1); the binding gains the
persona. A card marked as Work's (by serial and certificate hash, the identity Sigillum
already checks) is offered only in Work's choosers, and sigillumd refuses signing requests
whose caller's persona (from the peer uid, through arcad's label) is another. The VPN's
client certificate (vpn.md §9.3) and a Work Navigator window's TLS client certificate both
fall under it.

### 5.3 FIDO security keys

Shared by default. Each FIDO credential is already bound to its website, so a personal
site cannot use a work credential, and many people use one key for both. A key can be
marked as one persona's in Curia: Sigillum then registers a marker credential on it (RP id
`persona.atrium.local`, non-resident) and, when a key is plugged in, proves which key it is
by a silent assertion with that credential (no touch needed, `up=false`). A marked key
answers only that persona's WebAuthn requests and sign-ins. The marker is a credential the
key holds, so it cannot be copied to another key the way an ID could.

### 5.4 Other devices

Cameras, microphones and the like follow the instance that asked, under their existing
grants. Nothing persona-specific in v1. Printers belong to a persona or are shared, and
printing onto another persona's printer is a crossing (printing.md §9, accepted 2026-11-17).

## 6. Crossing between personas

One rule for every way data moves:

| What crosses | Where it is checked |
|---|---|
| a link opened in another persona | the Limen/hand-off path (navigator-ui.md hand-offs) |
| a file (picker, Scrinium copy or move, drag and drop) | Scrinium and the picker |
| a clipboard entry | Tabula: each entry carries the persona of the instance that copied it (from its peer uid) |
| a share-sheet offer | the broker (CLASS_BROKER) |
| a workspace link (homes.md §5.2) | portcullisd |
| a notification action that opens something | Forum, as a link |

- **Off by default, both directions.** The attempt is refused in a sheet drawn by Forum
  that says why. For a link it offers **Copy address**; for a paste, nothing pastes and the
  strip notes "copied in Work"; for a file, the sheet names it and the switch that would
  allow it.
- **Switches per persona and direction** in Curia ("Work may send to Personal", "Personal
  may send to Work"), optionally per kind (links, files, clipboard). Turning one on needs
  user verification (Sigillum `VERIFY_USER`), so a moment's access to an unlocked machine
  cannot quietly open a path.
- **When allowed, every crossing is one press** in a system sheet showing what crosses
  (the full address, the file names, the first line of the clipboard text) with both
  chips. Never remembered.
- **Never, even with the switch:** a Work link that only works inside Work's VPN (vpn.md
  §9.7) opened direct.
- **Into Work is the riskier direction for the employer** (a phishing link or a malicious
  file reaching the internal network). A link that crosses into Work opens anonymous
  (navigator-ui.md §6.2) in a new Work trail.

## 7. How it is shown

The indication must come from something the app cannot fake, so frescod and forum-wm draw
it from the launch registry's persona for the client's uid:

- **The strip.** frescod's 24 px strip starts with a persona chip in the persona's colour:
  `Work`. With a VPN it carries the route (`Work · via Acme VPN`, amber `Work · Acme VPN
  down: no network`, `Work · connecting`). While any named persona or VPN is in use, every
  other window's chip says `Personal` (with `· direct` when a VPN is up), so both sides are
  always labelled.
- **The frame.** forum-wm draws a 3 px border in the persona's colour around every window
  of a named persona, outside the app's surface, where no app can draw.
- **Terminals.** A console's strip and frame show the persona of the jail it is attached
  to; the title reads `Terminal: acme-dev · Work`.
- **Dock, Applications, overview, Alt-Tab.** Each instance has its own icon with a persona
  badge in the corner (a coloured dot and the persona's first letter).
- **Notifications** carry the chip.
- **Scrinium and the picker** label every place and stick with its persona.
- **The Forum VPN item** lists which personas and instances use each VPN right now.

## 8. Threats this answers, and what it does not

| Threat | Answer |
|---|---|
| Opening a work document in a personal app by mistake, or saving it to a personal folder | instances, per-persona pickers, crossing off |
| A compromised personal app (or a malicious page in a Personal window) reaching work data, cookies, VPN or certificates | separate uid, folders, Arca persona, routing table, token binding |
| A phishing link in personal chat reaching the internal network | crossing into Work off; when on, the page opens anonymous |
| A lost Work stick | encrypted with a key that never left this machine's Arca |
| Leaving a job | removing the persona destroys its keys (with Tessera encryption) |
| The employer seeing personal traffic | personas route separately (vpn.md §9.6) |

Not answered: a person deliberately carrying data across (copying by hand, a photo of the
screen, turning a switch on). That is the person's business with their employer; Atrium
has no device management (MDM) to let an employer lock the switches, and none is proposed.

## 9. Decisions

1. **Personas as a system feature, owned by portcullisd** (policy file, Curia page over
   `CLASS_PORTCULLIS` with restricted `personas-configure`). *Recommend yes.*
2. **An app runs once per persona**, each instance its own uid (per person, app,
   persona), data, Arca persona and route. *Recommend yes.* Alternative: one instance
   per app, persona only for network (simpler, but no Work and Personal mail side by side).
3. **Same home, a volume per named persona** under `.atrium/personas/`, own group, own
   key once Tessera encrypts; not a second login. *Recommend yes.*
4. **Pickers show only the caller's persona's places** plus neutral sticks. *Recommend yes.*
5. **Persona sticks = Tessera sticks keyed in that persona's Arca; neutral sticks readable
   by all; a named persona writes only to its own places** unless a crossing switch allows.
   *Recommend yes.*
6. **Tokens and smartcards can belong to a persona and sign only for it.** *Recommend yes.*
7. **FIDO keys shared by default; optional marking by a marker credential.** *Recommend yes.*
8. **Crossing off by default both ways, for links, files, clipboard, sharing and workspace
   links; switches per persona and direction behind user verification; one press per
   crossing.** *Recommend yes.*
9. **Indication by frescod and forum-wm from the launch registry** (strip chip on every
   window while a named persona or VPN is in use, coloured frame, badges). *Recommend yes.*
10. **One persona per Navigator window; a workspace's persona fixed at creation**
    (changeable in settings only). *Recommend yes.*
11. **No employer management of the switches** (no MDM) in v1. *Recommend yes.*

vpn.md decisions 18–21 point here.

## 10. Build order

| Step | What | Done when |
|---|---|---|
| P0 | portcullisd: personas in the policy file, instances keyed by (person, app, persona) with uid and overlay, persona in the launch registry; launch question; Curia Personas page | two Mail instances run side by side in the VM, each sees only its own data; the registry names their personas |
| P1 | Indication: frescod strip chip, forum-wm frame and badges, console-of rule | frames of Work and Personal windows, terminals and the dock |
| P2 | Storage: persona volumes and groups, Scrinium places, picker scoped by persona, Navigator downloads to the persona's folder | a Work instance's picker shows no Personal folder; Scrinium labels both |
| P3 | Crossing: Tabula entries carry the persona, link hand-offs, Scrinium copies, the sheet and switches with user verification | each kind refused by default and allowed with one press when the switch is on |
| P4 | Devices: Sigillum persona binding for tokens, FIDO marker credential; persona sticks after Tessera encryption | a Work card refuses to sign for a Personal window; a marked key answers only Work |
| P5 | Workspaces and Navigator windows per persona (with homes.md W3 and navigator-ui named personas); network per persona is vpn.md V2 | a Work and a Personal workspace terminal side by side, labelled, with different routes |

**P0 as built (2026-11-17, instances).** `[personas.<name>]` tables in the person's `policy.toml` (`colour` from a fixed set of 8, unique; optional `vpn`; at most 8; a bad table means no named persona at all). `portcullis launch --persona work <app>` (IPC `Launch.persona`) starts a separate instance: its own uid from the app range, registry line `<uid> <user> <app> [publisher] persona=<name>` (the strict parser voids a line with a garbled field, so an old reader fails closed), overlay `~/.atrium/apps/<id>@<persona>`, jail root and name per instance, and its own synthetic host identity so an app cannot link its two instances. Refused: an unknown or invalid persona, `personal` as a name, and apps that span personas (home files, removable files, app grants, workspaces). A persona instance gets no document channel until P3. Gate `scripts/vm-personas-check.sh` (amd64 VM, 0 failed). Still to do in P0: the launch question (which persona, when an app is launched with none and the person has named personas) and the Curia Personas page.

**Navigator windows as built (2026-11-17, part of P5).** `portcullis launch --persona work org.atrium.navigator` runs a Work Navigator beside the Personal one (navigator-ui.md §2.4 "Named personas as built"). Its converter and fetcher jails run as its own instance uid, and its page and UI jails as uids of Work's own: `per_instance` worker rules give each (person, worker app, persona) its own uid in the launch registry (navigator-ui.md §2.4). Links stay in their persona: each persona instance has its own document channel (`<id>@<persona>`), and portcullisd opens a link in the asking instance's persona, so a link from Work Mail opens in the Work Navigator (apps that span personas have one instance and take it). Apps learn their persona from portcullisd's new `Persona` request, which reads the registry by peer uid. Gate: `scripts/navigator-confined-vm.sh` "a Work Navigator beside the Personal one".

**P3 for links, as built (2026-11-17).** The switches are `[[crossings]]` tables in the person's `policy.toml` (`from`, `to`, `kinds`; Personal is `personal`), off unless one names that direction and kind (`Policy::may_cross`). Monstrator's sheet has an "In: <persona>" row whenever the person has a named persona; an app's "Open with…" (`monstrator::open_link_with`, the Navigator's toolbar button) always brings the sheet up. portcullisd's `LinkGranted` lists each persona with whether the link may go there, and the asker's own app as a choice only in another persona (the Work Navigator's page in the Personal one); `DeliverLink.persona` names the target, and portcullisd checks the switch again before delivering. With the switch off the sheet says "Links from Work to Personal are off. Settings, Personas has the switch." and nothing crosses. Gate: `scripts/navigator-confined-vm.sh` (94/94).

**Copy address and the clipboard, as built (2026-11-18).** Monstrator's sheet has a "Copy address (in Work)" row whenever the person has a named persona. It copies the address through Tabula for the app that asked (`Set.for_uid`, which Tabula takes only from `_monstrator`), so the entry is in the asker's persona. Tabula entries and drags carry the persona of the app that copied them, and a paste or drop in another persona is refused unless the person's switch has `clipboard` in its `kinds` (tabula.md §3.3). The sheet now comes up over the window that asked: forum-wm keeps the last document in front while a dialog has the focus (before, any dialog put the first-opened document in front). Gate: `scripts/navigator-confined-vm.sh`. Not yet: the strip's "copied in Work" note on a refused paste, the Curia switches behind `VERIFY_USER` (today the file), a link crossing into Work opening anonymous (the Navigator opens it in a new trail, but a site Work already knows still knows it), and the VPN-only rule.
