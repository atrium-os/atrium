# Opening files: types, "Open with", and what never runs

Status: accepted 2026-11-14 (Shankar: "accept as recommended", all of §11 D1–D10 as recommended). Building T1–T5.
Asked by Shankar (2026-11-14): open documents, audio, video and so on by
clicking them in Scrinium, safely; can apps be run that way.

Related: [scrinium.md](scrinium.md) (the file manager and the picker),
[limen.md](limen.md) (roles, `doc-viewer`, `media-player`),
[tabula.md](tabula.md) §2 (frescod's activation tokens),
[portcullis.md](portcullis.md) (launch), [officina.md](officina.md) §4,
[atrium-player.md](atrium-player.md), [atrium-pkg-install-ux.md](atrium-pkg-install-ux.md),
[forum.md](forum.md) §6 (apps are launched from Forum, not from files).

## 1. Short answer

- A double-click on a file in Scrinium opens it in the app chosen for its
  type. The app gets **that one file as an open descriptor**, read-only
  unless it edits that type, and nothing else: no path, no folder, no
  other file. This is the picker's rule (scrinium.md §4.2) run the other
  way round.
- The **type** is decided by Scrinium from the extension *and* the first
  bytes. When they disagree, Scrinium says so and doesn't open it in
  either app without the user choosing.
- Apps **declare** the types they open, view or edit, in their signed
  manifest; the install screen shows the list. Declaring a type gives an
  app no file: it only puts it on the "Open with" list.
- The **default app** per type is the user's choice. Atrium's own apps are
  the first defaults. Installing an app never changes a default.
- **portcullisd** does the hand-over: it checks the click was a real one
  (a frescod activation token), that the app declares the type, narrows
  the descriptor's rights, and starts the app or gives the file to the one
  running.
- **Nothing is executed from a file.** Sticks and shares are `noexec`
  already; a program, script or shortcut file is shown as what it is and
  never run. A signed Atrium app bundle opens the installer, which checks
  it and asks; that is installing, not running.
- Apps are started from **Forum's app list**, not from files (forum.md §6;
  the Applications grid is Forum's thread).

## 2. What exists

- Apps are folders under `/var/lib/atrium/apps/<id>/` with a signed
  `atrium.toml`: id, name, icon, capabilities. forum-dock gets the catalog
  from portcullisd and asks it to launch one; portcullisd checks policy and
  starts the app in its jail. `Launch` hands the app stdio and nothing
  else. Manifests say nothing about file types.
- Scrinium's picker returns a file as an open descriptor through limend
  (`EMIT_FD`): regular files only, `O_RDONLY` or `O_RDWR`, Capsicum-narrowed.
  The asking app never sees the path.
- Limen user roles with defaults: `doc-viewer` (the Navigator's document
  worker) and `media-player` (the default media app). Curia's default-app
  UI is not built.
- frescod mints a one-shot activation token for each press; Tabula, the
  share broker and Praeco spend them as proof of a user gesture.
- Officina opens through the picker and plans "open with" through the
  same descriptor path (officina.md §4.1).

## 3. Threats

| threat | answer |
|---|---|
| A file named `.pdf` that is something else (HTML with script, an executable) | type from extension and content; disagreement stops the double-click (§4.2) |
| A file that exploits the app's parser | the app's own jail and capability mode; decoders are already jailed (Tessera decoders, Navigator's converter, the player's decode jail); the app holds one descriptor, not the folder |
| An app that opens files by itself, or makes Scrinium do it | only a press in Scrinium's window opens; portcullisd spends frescod's token for it (§6.2) |
| An app that installs itself as the handler for everything | declarations are eligibility only; defaults change only in system UI the app can't draw over (§5) |
| Running a program from a stick, share or download | never; `noexec` mounts, no "run" action anywhere (§8) |
| A shortcut or link file that points elsewhere (`.desktop`, `.url`, `.lnk`, `.webloc`) | shown as a file; never followed, never run (§8) |
| A write through a viewer | read-only descriptor unless the app declares *edit* for that type |
| The app keeps the file after it closes it | it has the descriptor only; a stick's eject or a share's disconnect ends it (reads fail: `ENXIO` from a share, checked in the gate) |

## 4. File types

### 4.1 Names

Types are IANA media types (`audio/flac`, `application/pdf`), so they
match what the web, mail and the converters already use. Atrium ships
one table, the `atrium-types` crate, compiled into Scrinium,
portcullis-toml and portcullisd, so it is part of the signed system and
no jail needs a file for it:
each type with its extensions, the bytes that mark it (offset and
magic), whether it is text, and a family (`document`, `audio`, `video`,
`image`, `archive`, `program`, `link`, `other`) that picks the icon and
the words Scrinium shows. Apps can't add types in v1 (D2).

### 4.2 Deciding a file's type

Scrinium reads the first 4 KiB through its own descriptor (it already
holds the stick or share) and compares:

- the extension names a type and the bytes match it, or the type has no
  magic and the bytes fit (text: valid UTF-8, no NUL): **that type**;
- the bytes match a different type than the extension says (a `.pdf` that
  starts with `<html`, a `.jpg` that is an ELF file): **mismatch**. A
  double-click shows "This file says it is a PDF but is really an HTML
  page" with Open as HTML, Open as text, and Cancel; nothing opens until
  the user picks. A mismatch toward `program` gets no Open at all (§8);
- no extension, or one the table doesn't know: the bytes decide if they
  match; otherwise **unknown** (§4.3).

The check is Scrinium's (it runs in its own jail, reads bytes, parses
nothing). The app then parses the file in its own jail as it would any
file the user picked.

### 4.3 Unknown files

"No app opens this kind of file", with Open as text when the bytes are
text, and "Open with…" listing every app that opens `text/plain` or
`application/octet-stream` (a hex viewer, later). Never a guess.

## 5. Apps and defaults

### 5.1 The manifest

```toml
[[opens]]
types = ["audio/flac", "audio/mpeg", "audio/ogg", "audio/wav"]
mode = "view"            # or "edit": the app may change the file in place

[[opens]]
types = ["application/vnd.oasis.opendocument.text"]
mode = "edit"
```

Validation (portcullis-toml): types from the table only, at most 64, no
wildcards (`audio/*`) so a list is always what the install screen shows;
`program` and `link` families can't be declared (§8). The install screen
says "Opens: music (FLAC, MP3, Ogg, WAV); edits: documents (ODT)". An
update that adds types shows them again.

### 5.2 Defaults

The user's defaults are a table type → app id per user, kept by
portcullisd next to the user's policy (`<user>/opens.toml`). It starts
from the system's: Officina for office documents, Atrium Player for audio
and video, the Navigator's document worker for PDF, HTML, SVG and plain
pictures until there is an image viewer (D5).

- Installing an app changes no default. A type with no default (its
  only app was removed) asks once, in the Open with sheet.
- Changing a default happens in two places, both system UI: Scrinium's
  Open with sheet ("Always open FLAC files with…") and Curia's Default
  apps page. portcullisd takes the change only from Scrinium or Curia
  and only with a token for a press (§6.2), so an app can't make itself
  the default.

## 6. Opening

### 6.1 In Scrinium

- **Double-click or Enter**: open with the default app for the type.
- **Open with ▸** (menu and context menu): the apps that declare the type,
  the default first, each with its mode ("view" apps are listed under
  "Open a copy to view"). "Always use this app" sets the default (§5.2).
- **Space**: a quick look in Scrinium's own window (§7).
- Several files selected: one open per file, to each file's own default;
  more than 16 asks first.

### 6.2 The hand-over

```
Scrinium ── OpenFile { app: Option<id>, type, write, activation, name } + fd ─▶ portcullisd
portcullisd ── ACTIVATION_CONSUME(token, scrinium uid, OPEN) ─▶ frescod
portcullisd ── narrows fd, starts or finds the app ─▶ app: Open { name, type, write } + fd
```

- Scrinium opens the file itself (`O_NOFOLLOW`, regular file, `O_RDWR`
  only when the app's mode is `edit`, the file is writable and the place
  isn't read-only), and sends it with the press's token. The **name** is
  the file's own name only, for the app's title bar.
- portcullisd takes `OpenFile` only from Scrinium (the session's file
  manager, as dispensatord's `CONNECT`), spends the token (new purpose
  `OPEN`, a `BUTTON` or `KEY` press in Scrinium's window within 1 s),
  checks that the app declares the type in that mode and that the user's
  policy lets it launch, and checks the descriptor itself (`fstat`:
  regular file; access mode as claimed).
- It narrows the descriptor with `cap_rights_limit`: read, seek, fstat,
  and for `edit` write, ftruncate, fsync. No `fchmod`, `fchown`,
  `fexecve`, `mmap` with exec.
- The app gets it on its **document channel**: a socket portcullisd
  serves for each app that declares `[[opens]]`, mounted into that app's
  jail alone at `/atrium/sockets/opens/opens.sock` (the `atrium-open`
  crate). The app connects once at start; portcullisd takes the
  connection only from the app's own uid and sends each file there as an
  `OPEN` message (type, `r` or `w`, name) with the descriptor. A file
  opened before the app has connected waits (60 s at most); portcullisd
  starts the app if it isn't running. An app that declares no types has
  no channel. *(As built: not fd 3 and `ATRIUM_OPEN=1` as first written,
  since jail(8) closes every descriptor above 2 and cleans the
  environment; one channel serves new and running instances alike.)*
- The app never learns the path, the folder, or the place. Save As goes
  through the picker (officina.md §4.2).

### 6.3 What the app can do with it

Read it, and change it if it edits that type, for as long as it holds the
descriptor. Ejecting the stick or disconnecting the share ends that: its
reads fail (`ENXIO` once a share is disconnected). An editor's autosave writes through the same descriptor;
renaming or moving the file needs the picker.

## 7. Quick look

Space in Scrinium shows the file in a pane of Scrinium's window: an
embedded Limen `doc-viewer` for documents and pictures, `media-player` for
audio and video, plain text drawn by Scrinium itself. The embed is its
own jail and gets the file as a read-only descriptor through limend (new
`ToChild::OpenFd`, the reverse of `EMIT_FD`), so a crafted file attacks the
viewer's jail, not Scrinium. Types with no viewer show their icon, size
and type. D6 asks whether quick look comes in the first build.

## 8. What never runs

- **Programs and scripts** (ELF, Mach-O, PE, shell, Python, `#!` files):
  family `program`. No Open, no Open with. Scrinium shows "This is a
  program. Atrium doesn't run programs from files." Copying and deleting
  work. Places are mounted `noexec` (network-shares §4.4, sticks), so even
  a shell can't run them from there.
- **Shortcuts and links** (`.desktop`, `.lnk`, `.url`, `.webloc`): family
  `link`. Shown as files, never followed. Open as text is allowed.
  An `smb://`, `sftp://` or `https://` link inside a file is only text;
  Scrinium's Connect form is opened by the user (network-shares §6.3).
- **Office macros and document scripts**: Officina never runs them, the
  Navigator's converter drops scripts (navigator threat model).
- **Signed Atrium app bundles**: family `bundle`. Double-click opens the
  installer (atrium-pkg-install-ux), which checks the signature and the
  publisher and shows the capabilities and the types it opens before
  anything is installed. The file is passed to it as a read-only
  descriptor like any other. Installed apps then start from Forum's list.
- **Linux programs** (Hospes): not run from files. Hospes runs only
  what an installed app ships.

## 9. Pieces and who builds them

| piece | where | thread |
|---|---|---|
| types table, detection, icons, mismatch sheet, Open with, quick look pane | Scrinium | this one |
| `[[opens]]` in the manifest, validation, install screen line | portcullis-toml, atrium-pkg install UX | this one |
| `OpenFile`, the document channel, defaults table | portcullisd, portcullis-ipc | this one |
| purpose `OPEN` for activation tokens | frescod (`fresco_protocol::activation`) | Fresco, small |
| `ToChild::OpenFd` for embeds | limend, limen | this one |
| receiving `Open` | Atrium Player, Officina, Navigator document worker | each app's thread (a small reader on the channel) |
| Default apps page | Curia | later |
| Applications grid | Forum | Forum thread |

## 10. Build order

1. **T1** types table and detection in Scrinium: icons and kind column
   from it, the mismatch rule, program and link files shown as such.
   *Built 2026-11-14:* `atrium-types` (52 types, IANA names, `marks` that
   name a file by its bytes and `confirms` that only agree with a name,
   e.g. any zip with `.docx`; programs by their bytes whatever the name;
   scripts, `.jar`, packages and shortcuts by name). The Kind column and
   icon come from the name only (reading every file's head would be a
   network read per file on a share); a double-click reads the first
   4 KiB off the window's thread (`O_NOFOLLOW`, regular files) and says
   what stops it: a program, a shortcut, a name that lies ("says it is a
   PDF document but is really a Web page"), or an unknown type.
2. **T2** `[[opens]]` in manifests and the install screen; the defaults
   table with the system's defaults.
   *Built 2026-11-14:* `[[opens]]` in `portcullis-toml`, checked against
   `atrium-types` (no wildcards, only declarable types, no repeats, at
   most 64); the approval screen in portcullisd and the CLI adds lines
   such as "Open audio files: FLAC audio, MP3 audio" and "Open and change
   document files: OpenDocument text". `portcullis-policy::opens` holds
   the system defaults and the per-user table in
   `/var/db/atrium/<user>/opens.toml` (the user's choice, then the system
   default, else ask; a choice counts only while the app still declares
   the type; programs, shortcuts and bundles can't be given a default).
3. **T3** `OpenFile` in portcullisd, the `OPEN` token purpose, the
   document channel; Atrium Player and Officina receive files;
   Scrinium's double-click and Open with. Gate on the cloud VM: open a
   FLAC and an ODT from a stick and from a share; the app gets one
   read-only (or edit) descriptor and can't reach the folder; a renamed
   HTML `.pdf` stops at the mismatch sheet; a script gets no Open; an app
   can't open a file or change a default by itself.
   *Built 2026-11-14:* frescod purpose `OPEN` (a click, or Enter, in the
   file manager's window; Forum can't grant it; root, i.e. portcullisd,
   is a consumer); portcullis-ipc `OpenApps` and `OpenFile`, both only
   from an app holding `removable-files` (whose jail now reaches
   portcullisd's socket); portcullisd checks the type is declarable,
   the app declares it (and edits it, for write), the user's policy
   already lets the app run (no prompt on this path), spends the press
   last, takes one descriptor (`fstat` regular, open for writing if
   asked), narrows it (read, seek, fstat; write, ftruncate, fsync for an
   edit), saves "Always", and hands it over on the document channel,
   starting the app if needed. Scrinium: double-click, Enter, Open and
   Open with… in the toolbar; the Open with sheet (apps, default first,
   "Always open … with the app I pick"); the question sheet when the
   name and bytes disagree, offering what the bytes show and "Open as
   text"; programs, shortcuts and (until T5) bundles never open. Pergola
   now runs a click's handler with its press's token (the release
   carries none). Atrium Player and Write take files from the channel
   (Player replaces what plays, in a window titled with the name; Write
   opens it as a picked file, Save writing back only when given write).
   Gate `scripts/vm-open-files.sh` on the cloud amd64 VM with a test
   app (`atrium-open`'s `open-probe`) from a stick: the app, started by
   portcullisd in its own jail and uid, reads a FLAC but can't write,
   chmod, map it or learn its path (`F_KINFO` refused), sees no
   `/media`; an ODT with Always is opened for changes and written, the
   default saved, the next double-click goes straight to the running
   app; an HTML `.pdf` stops at the question and opens as text,
   read-only; a script gets no Open; the app, alice, and the file
   manager without a real press are refused.
   *Then, 2026-11-15:* the same gate opens files from an SFTP share (an
   ODT to its default for changes, written through the share; a FLAC
   read-only), and disconnecting the share ends both descriptors (the
   app's next read fails, `ENXIO`). Write ships a manifest
   (`atrium-write/atrium.toml`: graphics, and `[[opens]]` ODT, DOCX and
   plain text to edit) and Player one (`atrium-player/atrium.toml`:
   audio and video to view); in the gate Write, installed and approved
   once, gets a `.docx` as the system's default, in its own jail and
   uid, and its Save writes the typing back as `.docx` through the
   descriptor. Pergola's default frescod socket is now the jailed one
   when it is there (it was always `/tmp/frescod.sock`), so a Pergola
   app portcullisd starts with `graphics` finds frescod. Player end to
   end in the VM waits for its FFmpeg cross-build there.
4. **T4** quick look (D6).
   *Built 2026-11-15:* Space in Scrinium opens a pane at the right of
   the list, and the arrow keys move it with the cursor; Space again or
   leaving the folder closes it. What it shows comes from the name and
   the first 4 KiB, read off the window's thread: text (and a file
   whose name lies but whose bytes are text, said as such) is drawn by
   Scrinium itself, monospaced, at most 64 KiB and 400 lines, control
   characters dropped; Web pages go to Limen's `doc-viewer` and audio
   and video to `media-player`, each embedded in the pane through the
   existing `SEND_FD` (the "`ToChild::OpenFd`" of §7: limend already
   checks a passed descriptor is a read-only regular file and narrows
   it to read, seek and fstat), with the role's own `Load`; programs,
   and types with no viewer yet (pictures, PDFs, office files), show
   only what they are and their size. The viewer goes when the pane
   moves on or closes. frescod no longer gives the keys to a slot's
   child just because it sits on top with no window focused (a press,
   or the parent, still gives them). Gate: `vm-open-files.sh`'s quick
   look part, with a stand-in viewer (`limen-view-probe`, both roles)
   that limend starts through portcullisd as its own user in its own
   jail: it gets the FLAC and the page read-only, can't write, chmod,
   map it or learn its path, sees no `/media`. Pictures (2026-11-15):
   `doc-viewer` gained `DocKind::Picture`; the Navigator's page worker
   (`navigator-page`) decodes PNG, JPEG, GIF, WebP and SVG in its jail
   with navigator-image's decoders and ceilings, reduced to at most 2048
   px per axis, and draws it whole and centred. Other pictures (TIFF,
   HEIC, AVIF, BMP) say "No viewer shows these yet". The gate now runs
   the real page worker as the doc-viewer (a Web page and a 1600x400
   PNG). Not yet: PDFs (renderer choice open), Player as the real
   media-player in the gate.
5. **T5** bundles open the installer.

## 11. Decisions, with recommendations

| # | question | recommendation |
|---|---|---|
| D1 | Type names | IANA media types, one signed system table |
| D2 | May apps add their own types? | Not in v1; a new type is a system table update |
| D3 | When extension and content disagree | Stop and ask, offering the type the bytes show and text; never open silently |
| D4 | Wildcards in `[[opens]]` (`audio/*`) | No: the install screen lists exactly what an app opens |
| D5 | Pictures until there is an image viewer | The Navigator's document worker; an Atrium image viewer is later work |
| D6 | Quick look in the first build | After T3: it needs `OpenFd` for embeds and is not needed to open files |
| D7 | Who may change defaults | Scrinium's Open with sheet and Curia, each with a press token; never an app |
| D8 | A file opened in an editor | Read-write descriptor only for `edit` types on a writable place; else read-only, and the editor offers Save As |
| D9 | Running programs from files | Never, including scripts and Linux binaries; bundles go to the installer |
| D10 | Unknown types | No guess: "No app opens this", Open as text when it is text |
