# Doing it all from the command line, and with no screen

Status: DEFERRED 2026-11-26. Shankar: "split the cli based items to a new
improvement and defer it for later". It was split out of
[background-apps.md](background-apps.md), where it was §5a and §5b. It was
reviewed twice by the reviewer thread, and those findings are folded in
below. Decisions G8–G11 are still open.

It covers two things:
- **CLI parity.** Every GUI action in background-apps.md gets a command,
  and a change that grants something is confirmed on a system sheet with
  portcullisd's own words, bound to a nonce.
- **No screen.** A person building over Stoa answers those asks, and
  pkg, Run, side-load and launch, on the Stoa client's own status row,
  with a security-key touch.

Section numbers below keep their old names (§5a, §5b) so the reviews
still match.

## 5a. Everything from the command line (Shankar, 2026-11-26)

Shankar: "there should be CLI ways to do everything the gui does." Every
action above has a command, with the same rules as the GUI.

| GUI | Command |
|---|---|
| "Running in the background" list | `portcullis background list [--json]`: each app, its persona, its state (running, paused after crashes, quit, not allowed), its restarts, and where it listens |
| Settings › Apps switch on | `portcullis background allow <app> [--persona P]` |
| Switch off ("Don't open at sign-in") | `portcullis background disallow <app> [--persona P]` |
| Quit in the list | `portcullis background quit <app> [--persona P]` |
| Open it now | `portcullis launch <app>` (exists) |
| Why did it stop? | `portcullis background log <app>`: the supervisor's starts, exits, restarts and pauses for that app |
| Listen switch, per network | `portcullis listen allow <app> <port> [--network current\|<saved name>]`, `portcullis listen disallow <app> <port> [--network ...]` |
| Listen status, connections | `portcullis listen list [--json]` |
| New project from a template | `cargo atrium new --template status-item\|server` |
| Package view | `opifex token agent`, `cargo atrium bundle --sign … --json`, `opifex side-load <file>`, all existing |

**How a command proves it is the person.** Nothing runs as the person
(homes.md §1), so a command typed in the Terminal runs in a workspace
or an app console, as that jail's uid. Anything that writes to that pty
can say anything (a script, `cat` of a file, a remote ssh session), so
the caller proves nothing. The defense is a press that is bound to one
exact request and shown by the system, not by the Terminal:

- **Reads** (`list`, `log`) are answered only to workspace and console
  uids, Curia and forum-wm, for the person who owns that jail (from the
  launch registry). An ordinary app's uid gets nothing, so an app can't
  fingerprint the person's background apps or open ports.
- **Changes that grant** (`background allow`, `listen allow`) send only a
  structured request: the verb, app id, persona, port and network. In a
  workspace, the command passes it to the Terminal with OSC 7770 (the
  path `pkg` and `make run` use today, dev-tools-plan.md M4). The Terminal
  forwards the fields untouched, and never shows text from the OSC.
  The pane's owning jail ("proj") comes from stoad, never from the
  sequence.
- **portcullisd turns the fields into a confirm sheet.** It checks them,
  builds the words itself (the app's name from the installed manifest,
  the persona's name, the port and the network), and asks a **system
  confirm sheet**. That sheet is a small jailed service with its own uid,
  like opifex-sheet, prelum-sheet and share-sheet, and it is the only
  uid portcullisd takes approvals from. The sheet says "proj asks: let
  Photo Server run in the background in Work" with Allow and Cancel.
  Neither button is the default, both stay inert for 500 ms after the
  sheet shows, and Enter does nothing until one has focus. So a person
  typing their next command can't approve by accident.
- **The press is spent for that one request.** portcullisd hands the sheet
  a fresh nonce with the request. The sheet's press (frescod's `PROMPT`
  purpose, in its own window) comes back with that nonce. portcullisd
  writes the grant only if the nonce matches the request it built, and
  uses the nonce up. A compromised Terminal can forward a different
  request, but the sheet then shows that request's true words. It can't
  spend a press it didn't get, because the press lands on the sheet, not
  on the Terminal. So the Terminal needs no grant capability at all.
- **Persona.** The request's persona defaults to the persona of the jail
  the command runs in. Asking for another persona works only through the
  sheet, which names it ("…in Personal, asked from Work"), so a Work
  workspace can't quietly grant in Personal.
- **Asks are rate-limited** per pane and per person: one open sheet at a
  time, and at most 5 a minute. Extras are refused with the reason.
- **Changes that only reduce** (`disallow`, `quit`) need no press when
  they come from the owner's own workspace or console uids, as the GUI's
  Off and Quit need none. They can only take away.
- **The host shell** (homes.md §6, root behind Sigillum) can do it all
  for any person: `portcullis --user alice background allow …`. Each such
  change is logged, and the person is told at their next sign-in that
  the administrator changed their grant (a Praeco notice).

Every command takes `--json` for scripts. A script can run every read,
and every change that only reduces. A change that grants always waits
for the person at the sheet. Its words and refusals are the GUI's, from
one table in `portcullis-ipc`, so the two never drift.

## 5b. With no screen: building over Stoa (Shankar, 2026-11-26)

Shankar: "I'm thinking of users building using stoa without GUI." A
person attaches to a workspace console from another machine with
`stoactl` ([stoa.md §2, §4.5](stoa.md)), then builds, bundles, installs,
runs, and allows a background server, with no screen at the Atrium end.
Every press in §3–§5a assumes a screen: the confirm sheet, the install
sheet, and the Terminal's asks for `pkg` and `make run`. So each needs a
second place to be answered.

**The answer goes where the asker is.** When an ask comes from a pane
that a Stoa client is attached to, portcullisd sends the confirm to
that client instead of the screen. The screen may be locked, or another
person may be sitting at it.

- **The confirm travels on Stoa's control channel, not in the pane.**
  portcullisd sends stoad (its own `_stoad` uid) the request's words and
  its nonce, as it would send them to the sheet. stoad delivers them to
  the attached client as a control datagram. `stoactl` already
  emulates the pane into its own grid and redraws the local screen
  itself; it never passes pane bytes through. It keeps one row below
  the grid that only it draws, and redraws that row after every frame.
  So no cursor movement or escape sequence from the pane reaches the
  row. No colour or style is unforgeable on a plain terminal, so the
  safety comes from owning the screen, not from the look. Nothing the
  pane reads sees the ask.
- **The person answers by typing `allow`.** The client's own key
  handler reads the word, not the pane's input, and sends it back as a
  control datagram. When the ask appears, the client flushes queued
  input, ignores keys for 500 ms, and then takes only `allow` followed
  by Enter on an empty answer line. A bracketed paste never counts, nor
  does a single `y` or Enter. So type-ahead, a paste, or a command that
  happens to contain "allow" can't answer it. stoad passes the nonce and
  the answer to portcullisd.
- **What proves it is the person.** The Stoa session itself does: sshd's
  userauth of the person's key, and the session's MAC key anchored in
  the SSH handshake (stoa.md §2). portcullisd takes the answer only:
  - from stoad's uid;
  - for that nonce;
  - for a session that stoad authenticated as the owner of the asking
    jail;
  - from a client that is attached and that itself authenticated as the
    owner. The check is per client, not per session: view-only clients
    and other people's never get the ask. With two such clients, both
    see it, the first answer counts, and the ask is withdrawn from the
    other.

  This is the same trust a GUI press puts in frescod.
- **Never from this machine.** A workspace can run `stoactl`, or ssh to
  the host, and may hold the person's key, since Arca's agent is
  reachable in a workspace (dev-tools M9). If stoad accepted such a
  client, the pane could answer its own ask. So stoad and sshd refuse
  clients from this machine's own addresses, loopback and
  100.64.0.0/16. pf already keeps jails off the host's addresses
  (network.md §0), and these refusals add a second check. That still
  leaves a workspace with outbound network: it could reach the host from
  outside, through a relay, with a key Arca's agent offers it. Only a
  touch closes that, which is why G10 now requires one.
- **The same channel answers every ask that needs a press, not only this
  proposal's:** `background allow` and `listen allow`, `pkg install` in a
  workspace (`WorkspaceAdmin`), `make run` (`WorkspaceRun`),
  `opifex side-load` (the install sheet takes a stoad answer bound to its
  own nonce), and `portcullis launch`'s first-launch prompt. So a
  headless person can build, install and run their app end to end.
- **A security-key touch for remote approvals.** A remote approval also
  carries a touch of the person's FIDO2 key: stoad sends a challenge,
  and the client's local agent signs it with an `sk-ecdsa` key that has
  `verify-required` set and is enrolled for remote approvals. A
  workspace can't touch a key. The recommendation is on by default,
  with a person who has no security key able to turn it off knowingly
  (G10).

**Two things block this today. Both are decisions, not just code:**

1. **sshd.** homes.md §6 says images for people ship without the host's
   sshd, while Stoa and aqueduct-remote authenticate through it. For
   headless use, the recommendation is to ship sshd restricted to the
   handoff. The config:
   - `ForceCommand` to `stoa-shell` or `aqueduct-shell`, with no
     subsystems;
   - keys only, read from a root-owned `AuthorizedKeysFile` outside the
     person's key group;
   - `PermitUserRC`, `PermitUserEnvironment`, `PermitTunnel`,
     `X11Forwarding`, `AllowTcpForwarding`, `AllowStreamLocalForwarding`
     and `PermitTTY` all off;
   - no agent forwarding into the host; the FIDO2 challenge goes to the
     client over Stoa's own channel;
   - a login class whose shell is `nologin` for anything but the forced
     command.

   The person's uid then exists only for the handoff's few milliseconds,
   and the shell runs in a workspace or console jail as always (G9).
2. **Unlocking the person's data.** A person's workspaces and app data
   are in their persona's key group ([tessera-encryption.md](tessera-encryption.md)),
   which opens when they sign in at the screen. A person who has only
   come in over Stoa has it locked: Stoa already runs such sessions in
   memory only (stoa.md §5.4a), and workspaces can't start. Headless
   building needs a remote unlock, such as the person's passphrase or a
   FIDO2 touch over the same channel, handed to Vestibulum and Arca.
   That belongs in Vestibulum's spec, with its own review, so this
   proposal only names it (G11). Until then, headless building works
   only while that person has a screen session up, possibly locked.
   Signing out at the screen puts the keys away.

Once the person is unlocked, background apps start at their first Stoa
sign-in after boot, as at a screen sign-in (§3.3). They stop when the
keys are put away, just before. Under G11, that happens when the
person's last session ends: a screen session counts until sign-out, and
a Stoa session counts while it is attached, or while it is detached
and still within a keep time this proposal adds (8 hours after its
last client detached, as a setting). Stoa sessions persist when
detached, so without that limit a forgotten session would keep the keys
open forever. A session that is only detached therefore keeps
background apps running until its keep time runs out.

## What changes where

| Where | Change |
|---|---|
| `portcullis` CLI | `background list/allow/disallow/quit/log` and `listen list/allow/disallow`, `--json`, `--user` for root (§5a) |
| Terminal (stoa-term) | forwards the OSC 7770 ask's fields untouched, with the pane's jail from stoad |
| stoad and `stoactl` | the confirm on the control channel: the status-row ask, the typed `allow`, nonce passed back; asks routed to the attached client when the pane has one |
| opifex-sheet, `WorkspaceAdmin`, `WorkspaceRun`, launch prompt | take a stoad answer bound to their nonce as well as a press |
| image | sshd restricted to the Stoa/aqueduct handoff (if G9) |
| a system confirm sheet (new, opifex-sheet's pattern) | shows portcullisd's words for one request and returns its press with the nonce; portcullisd takes approvals only from its uid |

## Checks

When this is built, each of background-apps.md's VM checks also drives
the CLI as well as the GUI: a change from the Terminal confirmed on
the sheet; a forged OSC asking for another app shows that app's true
name and is cancelled; Enter typed just after the sheet appears
approves nothing; an app's uid gets no read; a Work workspace asking
for Personal sees "Personal" on the sheet.

## Decisions (open)

| # | Question | Recommendation |
|---|---|---|
| G8 | Should a CLI change that grants be confirmed on a system sheet with portcullisd's own words, while one that only takes away needs no press, and root can act for anyone with a notice? | Yes (§5a) |
| G9 | Should images for people ship sshd, restricted to the Stoa handoff (ForceCommand, keys only), so that people can use it with no screen? | Yes (§5b); this changes homes.md §6 |
| G10 | Should remote approvals require a security-key touch? | Yes, on by default (a workspace holding the person's key could otherwise approve itself from outside); a person with no key can turn it off knowingly |
| G11 | Should remote unlock of a person's data go to its own Vestibulum proposal? | Yes; until then, headless works only while a screen session is up |

## Review

§5a (the CLI, 4ac8db0b8) had a second review on 2026-11-26. It found a
self-approval path, now closed:

1. High: the words and the press weren't bound to the request, so a
   forged OSC could say one thing and grant another. Now only fields are
   forwarded, and portcullisd builds the words and binds the press to a
   one-time nonce.
2. High: a compromised Terminal could have spent any press in its
   window. Now the press lands on a system confirm sheet, and the
   Terminal holds no grant capability.
3. High: Enter could approve. Now there is no default button, the
   buttons are inert for 500 ms, and asks are rate-limited.
4. Medium: reads let any app fingerprint the person. Now reads go only
   to workspace and console uids, Curia and forum-wm.
5. Medium: a Work workspace could grant in Personal. Now the persona
   defaults to the caller's, and the sheet names any other.
6. Medium: "scripts are refused" was unenforceable. The spec now says
   the defense is the bound press.
7. Low: the host shell's changes were silent. Now they are logged, and
   the person gets a notice at their next sign-in.
8. Low (optional, taken): `disallow` and `quit` from the owner's own
   jails need no press.

§5b (no screen, over Stoa, 03ecaa4ac) had a third review on
2026-11-26:

1. High: a workspace could attach as the person's own client and answer
   its own ask. Now stoad and sshd refuse clients from this machine, and
   remote approvals need a FIDO2 touch by default (G10).
2. High: with two clients, the wrong one could answer. Now the ask goes
   only to clients that authenticated as the owner, checked per client;
   the first answer counts and the ask is withdrawn from the others.
3. Medium: the status row was claimed to be safe because of its style.
   Now it is safe because `stoactl` owns the local screen and redraws a
   reserved row; the style claim is dropped.
4. Medium: type-ahead or a paste could answer. Now queued input is
   flushed, keys are ignored for 500 ms, `allow` must be followed by
   Enter, and paste never counts.
5. Medium: sshd beyond ForceCommand. Now the full restricted config is
   listed.
6. Low: "once since boot" was wrong. Now it reads "while a screen
   session is up".
7. Low: "the last session ends" was undefined for detached Stoa
   sessions. Now it is defined.
