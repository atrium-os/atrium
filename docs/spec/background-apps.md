# Background apps: bar apps at sign-in, servers, and their templates

Status: ACCEPTED 2026-11-26, G1–G7 as recommended (Shankar: "split the cli
based items to a new improvement and defer it for later and execute the
rest"). Building, B1 first. Shankar asked on 2026-11-26: "does it support
menu only apps with no windows? daemon apps like servers?" The reviewer
thread's findings are folded in (§9).

The command-line and no-screen parts (doing all of this from the CLI, and
answering over Stoa) moved to [cli-and-headless.md](cli-and-headless.md),
DEFERRED.

## 1. Short answer

Three things are missing today:

1. **Nothing opens an app when the person signs in.** A bar app
   (`status-item`, [forum-status-items.md](forum-status-items.md)) needs no
   window, but it shows up only after someone opens it from the dock.
2. **Nothing keeps a windowless app running.** An app's entry can be a
   server, and its jail's rc.d helpers run beside it
   ([portcullis.md §3.5](portcullis.md)). Nothing restarts it if it
   crashes, nothing starts it again after sign-in, and it isn't listed
   anywhere the person can see.
3. **Nothing reaches an app from the network.** Apps reach each other only
   through peers that both sides agree to ([network.md §0](network.md)).
   The manifest refuses `expose` and `lan_alias`
   (`portcullis-toml/src/network.rs`), so a phone can't reach a dev
   server, and a home server can't answer the network.

This proposal adds:
- an app that the person allows to **run while they are signed in**,
  supervised by the supervisor portcullisd already has for system services;
- one **listen** grant that lets the local network reach one port of a
  running app, behind the person's press;
- **status-item** and **server** templates, and the same words in
  Artifex's Package view and on the install sheet.

Apps that run with nobody signed in, triggered background work
([insula.md §11.4](insula.md)) and reach from the internet are out of
scope (§8).

## 2. What exists, and what this builds on

- **Status items** (forum-status-items.md): a bar icon with a menu or a
  panel that the bar draws, or a panel the app draws. Built and VM-checked.
- **portcullisd's supervisor** (`portcullisd/src/supervisor.rs`): it
  watches each service's procdesc with kqueue, restarts it per
  `[supervision].restart`, and pauses restarts after a burst. Today it
  runs only `services.d` system services.
- **Insula's lifecycle classes** ([insula.md §11.1–11.3](insula.md)): the
  design sketch names "resident background" and its manifest table,
  `[background.resident]`, mapped onto Laminar's low-weight WFQ class.
  None of it is built. This proposal builds the part of it that runs
  while the person is signed in, and keeps its names.
- **The person's grants**: the launch prompt and Settings › Apps (Curia
  §13) write a person's grant only after a press that frescod confirms
  (`APPROVE`). Background running and listening are two more grants of
  that kind, never a manifest's own say-so.
- **Per-app network stacks** (network.md §0): each networked jail has its
  own vnet and a /30 in 100.64.0.0/16. pf blocks everything from apps by
  default, and jaild renders each app's anchor from structured grants.
  Peer consent is a pf table that is filled while the app runs.

## 3. Running while signed in

### 3.1 The manifest asks

```toml
[background.resident]
# Opens when the person signs in, if they allow it. "manual": only when
# opened, but still kept running and restarted until quit.
start = "sign-in"          # "sign-in" | "manual"
restart = "on-failure"     # "on-failure" | "never"
# Required: a background app must name its ceilings (rctl).
[resources]
memory = "128M"
cpu = 25                   # percent of one core
```

- `entry` is the app's own `[app].entry`. There is no second binary.
  An app that wants a window and a daemon uses rc.d helpers in its jail
  (portcullis.md §3.5 Pattern B), as today.
- `[resources]` is required when `[background.resident]` is present. The
  manifest fails to load without it, with the reason, so no background
  app can run without limits.
- It runs at Laminar's low weight (insula.md §11.1), so a busy
  background app can't slow the foreground.

### 3.2 The person allows it

- The manifest only asks. On the first launch, the launch prompt adds
  one line: "Open Clipboard History when you sign in". It is unchecked by
  default. Settings › Apps (Curia §13) shows the same switch on the app's
  row.
- Turning it on writes `background = true` into that person's grant for
  that persona, under the same `APPROVE` press rule as the rest of Curia
  §13. Turning it off needs no press. An update that changes
  `[background.resident]` makes the grant stale, as any manifest change
  does, and the app asks again.
- **The person sees every background app.** The bar's system menu gets a
  "Running in the background" list: each app's name (from the launch
  registry, never the app), and Quit and "Don't open at sign-in" for
  each. A bar app is also listed there, so quitting it is always one
  place away even if its own menu offers no Quit.

### 3.3 Who starts it, and when it stops

- After sign-in, the session supervisor (the same step that starts the
  person's session components, ostiarius-privsep.md) asks portcullisd to
  start that person's allowed background apps. A new request,
  `StartBackground { owner }`, is gated as `LaunchSessionComponent` is:
  only the session launcher may send it, and portcullisd reads the list
  from the person's own grants, not from the request. portcullisd also
  checks that `owner` has a live session, and starts apps only in
  personas whose keys are unlocked. A second call in the same session
  starts nothing that is already running or that the person quit (it is
  idempotent and never undoes a Quit).
- Each app starts through the ordinary `Launch` path in each allowed
  persona. It gets the same jail, uid, overlay and persona key group as
  when the person opens it, so nothing new touches the data-at-rest rule
  in AGENTS.md.
- portcullisd's supervisor watches the instance's procdesc. With
  `restart = "on-failure"`, a non-zero exit or a signal restarts it,
  with the same burst limit and cooldown as system services. A clean
  exit (0) or Quit from the list stops it until the next sign-in.
- **Before every restart** the supervisor checks again that the grant
  still holds for the installed manifest, that the person's session is
  live, and that the persona's keys are unlocked. A revoked or stale
  grant, a locked persona or a session that ended means no restart.
- **Sign-out** marks each instance as stopping before it is killed, so
  the kill's non-zero exit is not a failure and nothing restarts it. The
  apps are stopped before the persona's keys are put away, because their
  data lives in that key group. The B2 check kills an app during
  sign-out and asserts that it does not come back.
- `start = "manual"` gives the same supervision and listing, but the app
  starts only when it is opened. Examples: a VPN client's bar item, a
  sync helper the person starts by hand.

### 3.4 Windowless apps in the dock

An app with no window is in the "Running in the background" list, not
the dock's running row. Opening it from the dock while it runs gets the
launch's existing "already running" answer (single instance). For a bar
app, Forum then opens that app's bar menu, so the press does something
visible. A windowless app without a bar item shows its row in the list.

## 4. Reaching a server from the network

### 4.1 The manifest asks

```toml
[capabilities.network]
outbound = "any"
listen = [{ port = 8080, proto = "tcp", from = "local-network" }]
```

- `listen` replaces the refused `expose`. Ports 1024 to 65535, at most 4
  entries. `from` is `"this-computer"` or `"local-network"`. There is no
  "internet" value: nothing here opens the router, and Atrium adds no
  UPnP.
- `listen` is not the same as `inbound`. `inbound` stays the consent for
  other apps on this computer to dial this one (network.md §0).

### 4.2 The person allows it, per network

- The launch prompt and Settings › Apps word it as "Lets devices on your
  network connect to Photo Server on port 8080". It is off by default,
  takes an `APPROVE` press, and is stored in the person's grant.
- **Per network.** `local-network` holds only on the networks the person
  allowed it on: a saved Wi-Fi network (its saved entry in Network
  settings, Curia §3), or a wired network named by its gateway's MAC,
  which atrium-netd records when the grant is made. On any other network
  the forwarding is off, and the bar list says "Not reachable on this
  network". So a café's Wi-Fi never sees the person's dev server. A
  gateway MAC can be spoofed on a hostile wire. It is a guard against
  mistakes, not against an attacker on that LAN, and the switch's words
  say so.
- **Open Wi-Fi networks are refused.** On a network with no password,
  the saved entry names only an SSID, which anyone can copy (an evil
  twin), so `local-network` can't be turned on there. The switch says
  why.
- `this-computer` needs only the first press, because it doesn't leave
  the machine. It reaches only the person's own processes (§4.3).

### 4.3 How it is enforced

- **jaild renders it**, like the other grants. It writes an `rdr` (not
  `rdr pass`) from the host's address on the allowed interface and port,
  `from` the allowed network's own subnet, to the app's 100.64 address
  and port. Then it writes a separate `pass` for that one flow, which
  carries the rate limit (`max-src-conn-rate`), so the filter still
  applies to the redirected traffic. Every value is a validated token
  (interface name, subnet, two ports, the jail's address), and the
  renderer refuses anything else rather than escaping it (AGENTS.md
  review checklist).
- **Failing closed when the network changes.** The rule exists only
  while the app runs and the grant holds on the current network. jaild
  drops it when the jail goes, when the grant is revoked, and when the
  link goes down or Wi-Fi disassociates. It waits for neither
  atrium-netd's identity check nor its change event, and re-adds the
  rule only after atrium-netd confirms the new network is an allowed
  one. The `from <subnet>` clause keeps a stale rule from matching on
  the next network during that window.
- **One owner per host port.** If two apps ask for the same port, the
  first allowed one keeps it. The second one's switch shows "Port 8080
  is in use" and can't be turned on. It names the holder only when that
  app belongs to the same person and persona, so one person's apps
  aren't revealed to another.
- **`this-computer` is the person's, not everyone's.** It forwards from
  the host's loopback. Its pass rule matches only the person's own
  sockets (pf `user` on their login uid and their apps' uids from the
  launch registry), not another signed-in person's or a system
  service's. Navigator refuses `localhost` and loopback addresses for
  any page it did not load from there itself, so no web page can fetch
  the person's local server (local CSRF and DNS rebinding). A typed
  `http://localhost:8080` still opens. Workspaces reach it through
  their own network grant, as today. B4's check proves both rules.
- Inbound flows are counted. The bar list shows "3 connections" next to
  the app, and the rate limit keeps a noisy LAN from flooding the jail.

## 5. The SDK and Artifex

- **Templates.** `cargo atrium new --template status-item` makes a bar app
  with a menu and a Quit, and `[background.resident] start = "sign-in"`.
  `--template server` makes a windowless Rust HTTP server on 8080 that
  answers one page, with `listen` set to `local-network`. A C variant of
  each comes after.
- **Artifex.** New project lists both. Run in a workspace works as it
  does today: workspace networking is the workspace's own and is not
  this grant. The Package view's app card shows "Runs in the background
  (if allowed)" and "Listens on 8080 (local network, if allowed)" in the
  install sheet's words, so the developer sees what the person will be
  asked.
- **The install sheet** shows the same two lines. The words live in
  `opifex-sheet` with the other capabilities.

## 6. What changes where

| Where | Change |
|---|---|
| `portcullis-toml` | parse `[background.resident]` (requires `[resources]`) and `network.listen`; `expose` stays refused, and its message points to `listen` |
| `portcullisd` | `StartBackground` (session launcher only; live session, unlocked personas, idempotent); the supervisor runs app instances as well as services, re-checking grant, session and keys before each restart; `background`/`listen` in the person's grant; Quit and list verbs that only forum-wm may call, for that session's own instances |
| `jaild` | render `listen` as rdr (from the allowed subnet) and a separate rate-limited pass; drop it on jail exit, revocation, link down or disassociation |
| `atrium-netd` | the current network's identity (saved Wi-Fi entry, or wired gateway MAC) in its change event |
| `ostiarius` / session | send `StartBackground` after the session components are up |
| `forum-wm` | the "Running in the background" list in the system menu |
| Navigator | refuse loopback for pages not loaded from loopback (§4.3) |
| Curia (Apps page) | two switches per app; the listen switch also lists the networks it is on for |
| `opifex-sheet`, launch prompt | words for both asks |
| SDK | `status-item` and `server` templates; `cargo atrium check` warns on a `listen` with no `[resources]` |
| Artifex | the templates in New project; the Package card lines |

No app-specific code goes into portcullisd. Both verbs are generic and
read only the manifest and the person's grant.

## 7. Build steps and checks

1. **B1 manifest and grant.** portcullis-toml parsing and refusals;
   grant fields; launch prompt and Curia switches. Host tests.
2. **B2 sign-in start and supervision.** `StartBackground`, the
   supervisor for app instances, stop at sign-out. VM check
   `vm-background-check.sh`: a sample bar app allowed, sign in, its item
   is in the bar with no press; kill -9 restarts it; a third crash in a
   minute pauses it; a revoked grant isn't restarted; a second
   StartBackground doesn't undo a Quit; a kill during sign-out doesn't
   come back, and the app is gone before unmount; a person who didn't
   allow it gets nothing.
3. **B3 the bar list.** forum-bar's list with Quit and "Don't open at
   sign-in"; added to the same VM check.
4. **B4 listen.** jaild rdr rendering with refusals; atrium-netd's
   network identity; Curia's per-network switch; Navigator's loopback
   rule. VM check: a second guest on the same QEMU network reaches the
   sample server only after the press, only on the allowed network, not
   after Quit, not after the link drops, and not when a second app asks
   for the same port. On this computer, another user's process and a page
   in Navigator are refused, and the person's own process gets through.
5. **B5 templates and Artifex.** Both templates, Artifex's New project
   and Package card, the sheet's words; vm-templates-check and
   vm-artifex-check gain one section each.

## 8. Out of scope, on purpose

- **Running with nobody signed in** (a home server that serves while the
  machine sits at the login screen). A person's app data lives in their
  persona's key group, which is locked until they sign in. A machine-
  owned app would need its own key group and an admin's consent. That is
  a separate proposal if wanted.
- **Triggered background** (push, alarms, network-resume, insula.md
  §11.4) needs Tabellarius first.
- **Reach from the internet** (router port mapping, relays). A tunnel or
  VPN the person sets up themselves is the way for now.
- **Below 1024.** A server that must answer on 80 or 443 can have the
  person's own reverse proxy in front of it, or come later as a
  privileged-port grant.

## 9. Decisions (accepted 2026-11-26)

| # | Question | Recommendation |
|---|---|---|
| G1 | Should a background app need the person's press, or is installing enough? | A press, per persona, off by default (§3.2) |
| G2 | Start at sign-in and stop at sign-out only, not at boot? | Yes; boot-time apps go to a separate proposal (§8) |
| G3 | Must every background app declare `[resources]`? | Yes, so its load fails without them |
| G4 | Should every background app be listed in the bar? | Yes, all in one "Running in the background" list |
| G5 | Should `listen` be allowed per network, with a café Wi-Fi never reached and open Wi-Fi refused? | Yes: per saved network, open networks refused (§4.2) |
| G6 | Should ports be 1024+ only, with no internet reach? | Yes for v1 (§8) |
| G7 | Templates: Rust first, C after? | Yes |

### Review

The reviewer thread read 55db95af4 on 2026-11-26 and made nine
findings. All nine are folded in above:

1. High: sign-out raced the supervisor. Now instances are marked
   stopping before the kill, and keys and session are checked before
   each restart (§3.3, B2).
2. High: `this-computer` reached every local caller, including web
   pages through Navigator. Now it matches only the person's own uids,
   and Navigator refuses loopback for pages it didn't load from there
   (§4.3, B4).
3. Medium: a restart could revive a revoked or stale grant. Now each
   restart re-checks the grant (§3.3).
4. Medium: StartBackground lacked checks. Now it needs a live session,
   starts only in unlocked personas, and is idempotent without undoing
   a Quit (§3.3).
5. Medium: an open Wi-Fi network is identified only by its SSID, so an
   evil twin could pass as it. Now `local-network` is refused on open
   networks (§4.2).
6. Medium: a network change had a window. Now the rule drops on link
   down or disassociation and matches `from <subnet>` only (§4.3).
7. Low: the redirect bypassed the filter. Now it is `rdr`, not
   `rdr pass`, with a separate rate-limited pass (§4.3).
8. Low: the port clash named another person's app. Now it says "in use"
   unless the holder is the same person and persona (§4.3).
9. Low: anyone could call Quit and list. Now only forum-wm may, for
   that session's own instances (§6).

The reviewer found nothing in §6 that needs app-specific code in
portcullisd.

## 10. As built

**B1 (2026-11-26).**
- `portcullis-toml` parses `[background.resident]` (`start`, `restart`,
  default `on-failure`) and refuses any other `[background]` class as an
  unknown key. Validation fails a resident app without `[resources]`
  `memory` and `cpu`. `network.listen` takes at most 4 entries, ports
  1024–65535, `tcp`/`udp`, `from` this-computer or local-network, and no
  port twice; `expose` stays refused and its message points to `listen`.
  `Manifest::background_lines()` holds the words every surface shows.
- `portcullis-policy`: the person's policy has `background` (per app: the
  manifest hash and the personas it may run in) and `listen` (per app: one
  persona, and the networks for `local-network`). A grant for another
  manifest holds nowhere, and allowing again doesn't bring back its old
  personas. Removing a persona or the app's access drops both.
- `portcullisd`: `SetBackground { app_id, persona, on, activation }` on the
  Apps page's peer-uid path (`app-grants`). On needs an app that asks, that
  runs as it is, a persona of the person's, the manifest trust check and
  then the APPROVE press; off needs none. The Apps page row carries
  `background { at_sign_in, allowed_in, stale }`.
- Curia's Apps page: a switch per persona in the app's details.
- The launch prompt: the dock has no prompt of its own today (it sends the
  person to Settings › Apps), so the switch lives there. The background
  line is in the asks the dock shows.


**B2 (2026-11-26).**
- Supervision is not portcullisd's kqueue supervisor: each background
  instance has a thread of its own around the ordinary launch path
  (`portcullisd/src/background_run.rs`), and the state machine is
  `portcullisd::background::Registry` (unit-tested). Every run has an id,
  so a thread left from an earlier session never restarts a later one.
- A crash is restarted; a third crash within 60 s pauses restarts for 60 s
  ("crashed too often"). Before each restart portcullisd checks again that
  the installed manifest is still resident, its signature verifies, the
  person's switch holds for that manifest, and its capabilities need no
  new press; otherwise it stops ("no longer allowed"). `restart = "never"`
  stops after the first exit.
- `StartBackground { owner }` comes from ostiarius after the session
  components, once per new session, and only from the `session_launch`
  service uid; portcullisd also wants `owner` to hold the active seat. It
  reads the list from the person's grants, never from the request, and
  starts nothing already known this session (so a Quit sticks).
- A resident app's `[resources]` become jaild's rctl (`memoryuse:sigkill`,
  `pcpu:deny`). Laminar's low weight is not applied yet.
- An app the person opens from the dock that is allowed in the background
  in that persona is supervised the same way.
- Sign-out marks the person stopping first, so the kills are not crashes.
- `scripts/vm-background-check.sh` (amd64 cloud VM): sign-in start with no
  press, rctl limits, refusals, crash restarts and the pause, revocation
  during the pause, a manual app opened and supervised, sign-out and the
  next session, and the bar's verbs below.

**B3 (2026-11-26).** The list lives in `forum-bar` (the bar draws the
system menus, not forum-wm): `forum-bar/src/background.rs`, polled every
3 s, hidden while nothing runs, amber while an app is paused. Each app has
Quit and, when it opens at sign-in, "Don't open … at sign-in". portcullisd
answers `BackgroundApps`, `BackgroundQuit` and `BackgroundOff` only to the
person's own `org.atrium.forum-bar` session component, by peer uid; names
come from the installed manifest. Off takes no press, since it only takes
leave away.

**B5 (2026-11-26).** `cargo atrium new` has `status-item` (a bar menu,
sign-in, 64M, 10% CPU) and `server` (a std-only HTTP server on 8080 with
an 8 KiB head limit, 5 s timeouts and 16 connections; `listen`
local-network; 64M, 25%). Artifex's New project lists them from `cargo
atrium new --list`. `cargo atrium check`'s consent lines and the install
sheet carry the background and listen words; opifex counts
`background:<start>` and `listen:<port>:<from>` as capabilities, so an
update that starts asking for either is asked about.
