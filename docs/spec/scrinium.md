# Scrinium: the file manager and the system file picker

**Status:** built 2026-11-14 (first cut: removable sticks; home folders only by
configuration). Gate: `scripts/vm-scrinium-check.sh` on the cloud amd64 VM.
**Scope:** the Scrinium app (`scrinium/`), which is two things in one binary:

1. **The file manager.** A Pergola app in its own jail that holds the
   restricted capability `removable-files` and so has every stick, whole and
   read-write, for as long as it runs ([device-assignment.md](device-assignment.md) §4.8, §6.11).
2. **The system file picker.** The provider of Limen's `picker` role
   ([limen.md](limen.md) §2.3). An app asks for a file or a folder;
   Scrinium's window comes up as a sheet over the app's window (§4.4), in
   Scrinium's own process and jail, and only what the user picks reaches
   the app.

## 1. Short answer

- Adding a persona in Settings restarts an open Files window so it can use the
  new persona’s folders. The replacement starts in the usual view.
  See [personas.md](personas.md) §4.1.
- No app sees `/media`. The file manager sees all of it; every other app asks
  the picker and gets one file (an open descriptor) or one folder (a mount of
  that folder alone in its own jail).
- The picker is a **system** role now: limend's catalogue names
  `org.atrium.scrinium` as its only provider, so no app can register to be
  the picker and see what others pick. (limen.md had it as a user role with
  Scrinium as the default; changed, see §7.)
- A file goes back over Limen with a new op, `EMIT_FD`: the picker opens the
  file itself, from its own standing grant, and limend checks and narrows
  the descriptor before passing it on. No root process is in that path.
- A folder goes through dispensatord's new `PICKED` op, which only the
  session's picker (an app holding the restricted capability `file-picker`)
  may call. dispensatord makes the same one-shot grant `GRANT` makes.

## 2. Pieces

| Piece | Where | What |
|---|---|---|
| model | `scrinium/src/browser.rs` | places, folder listing, selection, keyboard, new folder, rename, delete, copy/cut/paste, eject; picker mode with a filter and an `Outcome` |
| file system | `scrinium/src/fs.rs` | listing (folders first, natural order, symlinks shown but never followed), names, copy/move without overwrite (`a copy.txt`, `a copy 2.txt`), size and time text |
| places | `scrinium/src/places.rs` | sticks under `/media` from dispensatord's `media` events (or a 2 s scan), free space by `statvfs`, extra places from `SCRINIUM_PLACES` |
| views | `scrinium/src/view.rs` | the manager window and the picker window, Pergola (`VirtualList` rows, `TextField` for names) |
| binary | `scrinium/src/bin/scrinium.rs` | the manager window, the picker service, the hand-over |
| demo | `scrinium/src/bin/pick_demo.rs` | `scrinium-pick-demo`, an app that asks the picker, for the gate |
| picker protocol | `limen/src/picker.rs` | `Filter`, `ToChild`, `ToParent` with their limits |

### 2.1 Places

The sidebar lists the sticks Scrinium has been given (each `/media/<part>`
in its jail), labelled by size ("16 GB stick") with an Eject button, then any
places from `SCRINIUM_PLACES="Label=/path;..."`. There is no home folder by
default: Scrinium's jail does not hold the user's files, and giving it them
is a separate decision (§8).

### 2.2 The file system rules

- Names: no `/`, no NUL, not `.` or `..`, at most 255 bytes. Non-UTF-8 names
  are skipped in listings (shown later, escaped).
- Symlinks are listed (marked) but never followed: entering one is refused,
  delete removes the link, copy skips it.
- Nothing overwrites: rename to an existing name is refused (a case-only
  rename is allowed), paste picks a free name. A folder is never copied into
  itself. Move is `rename`, else (another stick) copy then remove.

### 2.3 The manager window

Sidebar of places; toolbar with Back, Up and the path as crumbs; actions New
folder, Rename, Delete (asks first), Copy, Cut, Paste and Show hidden files;
the list (name, size, modified); a status line with the item count and free
space. Double-click or Enter opens a folder.

A double-clicked app bundle (`.insula`, by its bytes) goes to the install sheet
(opifex.md §7.4, 2026-11-18): Scrinium opens it read-only and sends it as a
descriptor with the ask `"SideLoad"` (`scrinium::install`), never a path; the
sheet shows the person what it is and who signed it, and the status line says
what they decided. `removable-files` mounts the sheet's socket for this.

### 2.4 Keys

Up/Down move, Shift extends, Home/End, Enter opens, Backspace or Alt-Up goes
up, Alt-Left back, Delete asks to delete, Ctrl-A all, Ctrl-C/X/V, Ctrl-Shift-N
new folder, Ctrl-E rename, Ctrl-H hidden files, Ctrl-R reload, Escape cancels
an edit (or the pick).

## 3. Trust

Scrinium is system-trusted UI: it decides what an app gets. That trust is
held in three places, each checked by someone else:

- limend gives the `picker` role only to the uid registered for
  `org.atrium.scrinium`, and tells Scrinium the parent's uid (from the
  parent's socket credentials), so an app cannot claim to be someone else.
- dispensatord gives the standing grant only to an app whose installed
  manifest holds `removable-files`, and `PICKED` only to one holding
  `file-picker`; both are restricted capabilities that portcullisd installs
  only with policy approval.
- An app sees none of the picker. Scrinium's window is its own surface,
  placed over the app by frescod as a sheet (§4.4, limen.md §5.1); the app
  cannot read its pixels, send it input, move it, shrink it or draw over
  its frame.

## 4. The picker

### 4.1 How an app asks

```rust
use limen::picker::{Filter, ToChild, ToParent};
let e = embeds.request("picker", token, SLOT)?;          // pergola::embed
embeds.send(e, ToChild::Open(Filter::file(&["txt", "md"])).encode())?;
// then, from embeds.poll():
//   ParentEvent::MessageFd { bytes } + embeds.take_fd()  -> ToParent::File { name, size, write }
//   ParentEvent::Message { bytes }                       -> Folder { at, name, read_only } | Done { files } | Cancelled | Failed { why }
```

`Filter { kind: File | Folder | Save | Workbook, extensions, multiple, write, title }`. Limits:
16 extensions of 16 bytes, a title of 80, 64 files, names of 255; anything
over is refused by the decoder. The app never names a path or a stick.

### 4.2 Files

For each picked file Scrinium opens the path itself, `O_NOFOLLOW`, read-only
or read-write as the filter asked, and sends `ToParent::File` with
`EMIT_FD`. limend (limen.md §2.0, §2.3):

- takes a descriptor from a child only for a role whose catalogue row says
  `returns_fd` (today `picker` alone), only once the embed is attached;
- `fstat`s it: a regular file only (no directory, socket, pipe or device),
  opened `O_RDONLY` or `O_RDWR`;
- narrows it with Capsicum to read, seek and fstat, plus write, ftruncate and
  fsync for `O_RDWR`;
- passes it in the same frame as `ParentEvent::MessageFd`.

Then `Done { files }`, and the app detaches. Nothing is mounted in the app's
jail for a file.

When Sheets asks for `Workbook`, Limen identifies the parent. Scrinium
spends the picker's confirmation press through Portcullis and sends the file
directly to a new Sheets jail in the parent's persona. The parent receives
`Done { files: 0 }` after the broker accepts the launch. Sheets review inputs
use ordinary `File` mode. `Save` creates a unique named file in the chosen
folder and sends its writable descriptor to the parent. For Sheets, the request
includes a sealed snapshot of up to 2 GiB. Scrinium syncs it before publishing
the new name with a no-replace rename and returns a session save grant.
Scrinium keeps the last 128 grants. Each pins a directory and workbook inode.
Commit checks the selected entry and file revision immediately before rename.
Hard links, special permissions, filesystem flags and user extended attributes
require Save As. Replacement preserves ownership, mode and supported access ACLs.
A directory sync error after rename means publication could have succeeded.

Known limit: the descriptor is on Scrinium's grant mount, so it stops working
if Scrinium's jail goes, or the session locks, the stick is ejected or
unplugged. That matches §4.8.4's one-shot rule.

### 4.3 Folders: `PICKED`

`PICKED` (dispensatord media socket, op 6): `{part, path, app_uid,
read_only}`. dispensatord:

1. refuses a malformed request (`..`, an empty part, uid 0 as caller or
   target), a stick that is not served (`NOT_FOUND`), a locked or absent
   session;
2. checks the caller is an app of the session's user whose manifest holds
   `file-picker`, else "`<app>` is not the session's picker";
3. checks `app_uid` is an app (not the session's user) of the same user, and
   finds the one jail its processes run in;
4. makes a grant exactly as `GRANT` does (§6.11): that folder alone, owned by
   the app's uid, 0700, at `/media/<part>` in the app's jail, read-only if
   asked. It ends with the jail, on eject, unplug, lock and logout.

Scrinium then sends `Folder { at: "/media/<part>", name, read_only }` and
`Done`. One folder per stick per app at a time: while a grant from a stick is
in the app's jail, a second pick from it is refused `BUSY` ("already has
/media/<part>"); the picker says so.

CLI for tests: `atrium-dispensatord --picked PART PATH APP_UID`
(`PICKED_RO=1` for read-only).

### 4.4 The picker window: a sheet

Shankar 2026-11-14: modal to the app that asked, not to the system. The
picker is a Limen slot of kind `sheet` (limen.md §5.1):

- **The system places it.** 640 x 440, centred across the app's window and
  48 px below its top; the app's slot rect is ignored, so the app can't
  shrink or hide it. It moves with the app's window and has no title bar of
  its own; it can't be dragged. A window smaller than the sheet has it
  hanging past its edges, still centred on it.
- **Modal to that app only.** While it is up, frescod dims the app's window
  (a translucent layer under the sheet), gives the app and its other slots
  no pointer input, and sends keys meant for the app to the sheet. Every
  other app and the desktop work as usual.
- **A frame the app can't draw.** frescod's chrome draws a 2 px frame round
  the sheet in the focus ring's blue and a tag above it, `system:
  org.atrium.scrinium` (the chrome's font has lowercase only). A fake
  picker drawn by an app gets nothing, since the app still can't reach the
  stick, but the frame is what says this one is real.
- Escape or Cancel closes it.

Inside it: title (the app's, or "Choose a file" / "Choose a folder"), a line saying for
which app and whether it may change what it gets, the sidebar of sticks, Up
and crumbs, the list filtered to the extensions (folders always shown), and
Cancel plus Open / Choose folder. Escape cancels.

## 5. Running it

```
scrinium [--no-window] [--no-picker]
  FRESCO_SOCKET, LIMEN_SOCKET
  SCRINIUM_SIZE=960x600  SCRINIUM_SECS=n  SCRINIUM_OPEN=/path
  SCRINIUM_MEDIA=/media  SCRINIUM_MEDIA_SOCKET=...  SCRINIUM_PLACES="Label=/path;..."
  SCRINIUM_TRACE=1   log a line per change (place, path, entries, selection)
```

At start Scrinium asks dispensatord for its standing grant (`ATTACH_ALL`) if
the media socket is there.

## 6. The gate

`scripts/vm-scrinium-check.sh` (after `vm-automount-check.sh` has set the VM
up), on the cloud amd64 VM with frescod headless and limend: two jails
(Scrinium with `removable-files` + `file-picker`, a notes app without), a
FAT32 stick plugged through QEMU. It checks the `PICKED` refusals, the
manager listing the stick, keys (into a folder, a new folder from the
keyboard that alice then sees on her own mount), Scrinium providing the
picker, the picker in the app's slot, a read-only file pick read through the
descriptor and not writable, a writable pick whose line lands on the stick, a
folder pick mounted in the app's jail as only that folder (50601, 0700) that
the app writes into, and the grant ending with the app's jail. Screenshots
land in `scratch/scrinium/`. **0 failed** 2026-11-14 (TCG, about 20 minutes).
The sheet (§4.4) adds: two presses on the app beside and below the sheet
reach nothing, the picks are made at the sheet's frescod-chosen place, and
Escape typed with the app focused reaches the sheet and cancels.

Setting it up needs: `/usr/bin/env` in the test jails (the script copies
it), and frescod started with `FRESCOD_SLOT_UIDS=0` so that limend, run as
root here, may bind slots (else frescod logs "activation socket: refused uid
0" and the picker never appears). The host needs `dosfstools`, `exfatprogs`
and `libmd-dev` for `vm-automount-check.sh`.

## 7. Changes elsewhere

- limen.md: `picker` is a system role (provider Scrinium); `EMIT_FD` (0x15),
  `ParentEvent::MessageFd`, catalogue `returns_fd`; the picker's messages are
  per item (`File`, `Folder`, then `Done`) rather than one `Picked`.
- device-assignment.md §4.8.6: the picker does not go through portcullisd;
  files come from Scrinium's own grant over `EMIT_FD`, folders over `PICKED`
  gated on `file-picker`. `OPEN` stays root-only and unused by the picker.
- portcullis: capability `file-picker` (restricted, consent "Be the file
  picker: give other apps the files and folders you pick"); it also gives
  the `removable-media/` socket.

## 8. Not yet

- Home folders: proposed in §9, awaiting Shankar's decision.
- "Open with" ([opening-files.md](opening-files.md), proposed), thumbnails, search, drag and drop (Tabula), trash.
- "Always allow for this app" (device-assignment.md §4.8.4), listed in Curia.
- Activation tokens: a pick is a user gesture inside Scrinium's own window;
  dispensatord could also ask frescod for proof (tabula.md §2).
- More than one file per pick is in the protocol and the model but the gate
  only covers one.

## 9. Home folders (proposed 2026-11-15)

The layout around these folders (no user context, per-app data under the
home, workspaces and consoles) is in [homes.md](homes.md).

Asked for by Officina's O5h (recent files), which needs Scrinium to hold
the user's files and to issue bookmarks (officina.md §4.3). Today no app
can reach the user's files: portcullisd runs every app as its own per-app
uid, never the person's (portcullis.md §9.0), and `filesystem = ["~/…"]`
names the app uid's own home, not the person's.

**The folders.** Documents, Downloads, Pictures, Music and Videos, under
the person's own home on the host (`/home/<user>/`), made at first login.
They are the only part of the home Scrinium ever sees; dotfiles and app
state are elsewhere.

**Who may touch them (H1, the choice).**

| option | how | for | against |
|---|---|---|---|
| **A. A files group (recommended)** | each person gets a group `files-<user>`; the five folders are `<user>:files-<user>`, mode 2770 (setgid, so new files keep the group), and files are made 0660. A new restricted capability `home-files` (system apps only, like `removable-files`) makes portcullisd add that one group to the app uid's supplementary groups at launch, and mounts the five folders into the jail. Only Scrinium asks for it. | keeps the per-app uid rule; the kernel enforces it; a small change in portcullisd | files written by the person's own shell need the group and mode (atrium-session sets umask 007 and the setgid folders do the rest) |
| B. Scrinium runs as the person | an exception to the per-app uid rule for one system app | least code | breaks portcullis.md §9.0; Scrinium decodes thumbnails and previews of untrusted files later, so it should be the least privileged, not the most |
| C. Everything through dispensatord | Scrinium holds no permission; every list, rename, copy and open is a request to root | the narrowest app | every file operation becomes IPC to a root daemon: much more root code and slower listings |

**Picking from home.** Files: as today, Scrinium opens the file and hands
the app a Capsicum-narrowed descriptor (`EMIT_FD`), so the app's uid
doesn't matter. Folders: refused for home in v1 ("pick a folder on a stick,
or pick files"), because a folder mount would need the app to hold the
group for as long as it runs. Folder picks from home get their own
decision later.

**Bookmarks (for O5h and "open recent").** A bookmark is an Arca-sealed
token naming (person, app id, file handle). The file handle is FreeBSD's
`fhandle_t` (the file system id and the file's inode and generation), which
is the file identity officina.md §4.3 asked for: a moved file is the same
file, a copy is a different one, and a deleted and reused inode doesn't
match. Only root can open by handle and only root can see the generation,
so:

1. Scrinium, when it hands over a file, asks dispensatord for a bookmark;
   dispensatord takes the handle (`getfh(2)` on the path in
   Scrinium's grant), seals {uid, app id, handle, time} with Arca and
   returns it. The app keeps it (Officina keeps them in its recent list).
2. To open it, the app gives the token to Scrinium's picker channel with
   no sheet shown; Scrinium passes it to dispensatord, which unseals it,
   checks the app id and person match the caller, `fhopen`s it, checks the
   file is still a regular file under one of the person's home folders
   (owner and group as above), narrows the descriptor and sends it back
   through `EMIT_FD`. Missing, moved out of the home folders, or not the
   person's: "The file is no longer there."
3. Bookmarks for files on sticks are not issued in v1 (msdosfs handles
   are not stable across remounts).

**Build steps.** H1 the group, folders and `home-files` (portcullisd,
atrium-session, Scrinium's places list); H2 the picker over home (files
only); H3 bookmarks (dispensatord `BOOKMARK` and `REDEEM`, Arca sealing,
`limen::picker` messages); gate: `vm-scrinium-check.sh` grows home checks,
and a bookmark survives a rename and is refused after a copy-and-delete.

| | decision | recommendation |
|---|---|---|
| H1 | how Scrinium may touch the person's files | A, a per-person files group given by the restricted capability `home-files` |
| H2 | folder picks from home | not in v1; files only |
| H3 | bookmark identity | `fhandle_t` through dispensatord, sealed by Arca; home folders only |

