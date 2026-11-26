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

### 4.4 Opening your own server in Navigator (B6, accepted 2026-11-26)

Today a typed `http://localhost:8080` opens nothing. Navigator's fetcher,
`navigator-fetchd`, runs in a jail with its own network stack, under
Capsicum, reaching out only through casper's `cap_net`. So 127.0.0.1 there
is its own, and pf blocks apps from every host address. That is right for
pages: no web page can reach the person's server (local CSRF, DNS
rebinding). But the person can't open it either.

**Proposal: navigatord asks for a connected socket; nothing new can dial.**

- **Only an address the person typed, per navigation.** "Typed" comes only
  from navigator-ui's chrome socket, on a real Enter or press in the
  address bar, or a bookmark or history entry the person opens; never from
  the engine. It belongs to that one navigation, not to the tab. After it,
  a localhost request is allowed only from a document of that exact origin
  (same scheme, host and port). A link, redirect, script, form or
  `window.location` from any other page never starts one, even in the tab
  the person typed into, and neither does Monstrator's "open URL" from
  another app. Those stay refused by fetchd's address rules, as now.
- **One parser.** The address bar and fetchd share one parser that accepts
  exactly `localhost`, `127.0.0.1` and `[::1]` with a port. It refuses
  the rest of 127/8, `0.0.0.0`, `::ffff:127.0.0.1`, `localhost.` and
  `*.localhost`. `localhost` is never resolved through DNS.
- **Only a port of the person's own server, in that persona.** portcullisd
  takes the person and persona from navigatord's peer uid (the launch
  registry), never from the request. The port must be a `this-computer`
  listen port of one of that persona's apps, running now and allowed
  (§4.2). Any other port says "Nothing of yours is serving port 8080". So
  a page can't probe other local services, and Navigator can't reach
  another person's or another persona's servers.
- **No confused deputy.** For every `OpenLocalPort`, navigatord checks its
  own record of the page's top-level origin and the requested origin
  (fetch, XHR, WebSocket, frames alike), never what the engine claims.
- **How the bytes get there.** navigatord asks portcullisd
  `OpenLocalPort { port }`. portcullisd checks the rules above and asks
  jaild to connect to that app's own jail address and port (from the
  registry, never the host's loopback, where anything could listen), with
  a 5 s connect timeout. jaild connects as root, which the base rules
  allow, and passes the connected socket back through portcullisd to
  navigatord. navigatord checks the socket's peer is that address, then
  hands it to that page's fetchd over its existing socket. fetchd gets a
  new `Net` that holds only those sockets: it can speak HTTP on them and
  dial nothing. One socket per request; at most 8 open per page and 32
  per persona; refused once the server stops or the grant goes.
- **The origin belongs to the app, not the port.** Cookies, storage and
  service workers for `http://localhost:8080` are kept per (app id, port),
  so when another app later takes 8080 it starts empty and never sees the
  first app's state. The jar is keyed by host and port for localhost
  (cookies otherwise ignore ports, RFC 6265), so :8080 and :9090 share
  nothing. Subresources load only from that same origin, each through a
  new socket. Links out of it go to the normal web path. It is never cached
  or shared (threat model §7.3). The address bar shows a "Your computer ·
  Photo Server" chip, naming the app, not a padlock.
- **Not a secure context.** Other browsers treat `http://localhost` as
  secure. Navigator doesn't in v1: no service workers, no WebAuthn with
  rpId `localhost`, no other secure-only APIs. HTTPS to localhost isn't
  offered in v1.
- **What it doesn't do.** No other host name that resolves to loopback,
  no LAN addresses (those go through the normal private-network rules),
  no UDP. Pasting a GET with side effects into the bar is social
  engineering, as in any browser; that is accepted.

Checks for B6: a typed localhost opens the person's server; the same URL
as a link, a redirect or a `window.location` from another page, even in
the typed tab, is refused; a port no app of theirs serves is refused;
another person's and another persona's ports are refused; after another
app takes the port it starts with no cookies or storage; :8080 and :9090
share no cookies; each address the parser refuses (127.0.0.2, 0.0.0.0,
::ffff:127.0.0.1, localhost., a.localhost) is refused; after Quit the
next request is refused; fetchd's own dial to 127.0.0.1 still fails.

Review (2026-11-26, standing reviewer): three high findings (state
outliving the server, persona not person, "typed" per navigation and only
from the chrome), four medium and three low; all folded in above. Verdict:
sound once the high ones were in.

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
6. **B6 your own server in Navigator** (§4.4, accepted). portcullisd
   `OpenLocalPort`, jaild's connect and fd pass, navigatord's typed-address
   path, fetchd's socket-only `Net`, the address bar chip; the checks in
   §4.4.

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

Accepted 2026-11-26 (Shankar: "yes to all"):

| # | Question | Recommendation |
|---|---|---|
| G12 | Should Navigator open only typed (or bookmarked) localhost addresses, never ones a page links to? | Yes (§4.4) |
| G13 | Only ports of the person's own running, allowed servers, or any port on this computer? | Only their own servers' `this-computer` ports |
| G14 | Plain HTTP only to localhost in v1? | Yes; HTTPS to localhost later if a template needs it |
| G15 | Treat `http://localhost` as a secure context, as other browsers do? | Not in v1: no service workers or WebAuthn there |

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

**B4 (2026-11-26).**
- jaild: `NetGrants.listen` (`NetListen { port, udp, from }`, `from` =
  `Lan { iface, subnet }` or `ThisComputer { owner_uid, owner }`), every
  value a validated token. A `Lan` port is an `rdr` from the network's own
  subnet on that interface to the app, then a pass for that flow with
  `max-src-conn 16, max-src-conn-rate 30/10`. `SetListen { jail_name,
  listen }` swaps a running jail's anchor in one pfctl transaction and cuts
  flows to the app when a rule goes. One owner per host port: a second jail
  asking for a port another holds is refused ("port 8080 is in use by …";
  portcullisd decides what to show).
- **This computer is a forwarder, not an rdr.** The base rules `set skip
  on lo0`, and a loopback rdr could not tell one local user from another.
  jaild listens on 127.0.0.1:port for the app (`jaild/src/listen.rs`), asks
  the kernel which uid opened each connection (`net.inet.tcp.getcred`), and
  lets through only the person's login uid and their own apps' uids from
  the launch registry. The app's anchor lets only root (the forwarder)
  connect to that port from the host. TCP only; 16 connections; 5 minutes
  idle.
- Base pf (`etc/atrium.pf`): `rdr-anchor "atrium/*"` (which covers
  atrium/vpn), and `block in quick on atrium_egress inet from any to
  100.64.0.0/16` after the anchors. Before this, a LAN neighbour routing
  100.64.0.0/16 through the machine reached any app's ports. And the host
  reaches an app's address over TCP or UDP only as root (`pass out … user
  root`, then a block): before, any local process could connect to any
  app's ports.
- Network identity: atrium-net's `status` line for a Wi-Fi interface now
  carries `ssid=` and `secured=` (the saved entry has a credential).
  portcullisd (`listen_run.rs`) names the egress network `wifi-<ssid hex>`
  (secured only) or `wired-<router MAC>` (the ARP table), follows
  atrium-net's `subscribe` stream and re-renders every running listener on
  each event (and every 5 quiet seconds). With no network, or one the grant
  doesn't name, only this-computer ports stay.
- The grant: `SetListen { app_id, persona, network, on, activation }` on
  the Apps page's path; `network` adds or removes the network the computer
  is on now; an open network is refused with the reason. On needs the
  APPROVE press. Curia's Apps page has the two switches; the network one
  says it's a guard against mistakes, not against someone on that network.
- Navigator: its engines are app jails with their own network stack, so
  127.0.0.1 there is their own and the host's addresses are blocked to
  them; no page can reach the person's port. A typed
  `http://localhost:8080` opens it through B6 (below).
- The bar's line under each server: "3 connections" (jaild's
  `ListenStats`: the forwarder's live connections plus pf's established
  states from a network) and "Not reachable on this network" (the person
  allowed it on some network, not this one).
- A router once named keeps its name while atrium-net reports the same
  interface and router; any answer with no network forgets it. portcullisd
  logs each change of the network it sees.

**B6 (2026-11-26).** Your own server in Navigator, as §4.4, with these
differences:
- **One parser**, `navigator_control::local`: `http://` with `localhost`,
  `127.0.0.1` or `[::1]` and a port (no leading zero), an optional path
  and query; the fragment is dropped. The address bar (`where_field`),
  navigatord and fetchd all use it. A typed `localhost:8080` becomes
  `http://localhost:8080/`; without a port it is a web address as before.
- **Typed.** The address bar sends `Request::Typed { url }` before the
  page opens (Enter, a bookmark or history suggestion). navigatord holds
  at most 16 such grants per UI connection for 30 s, each spent by the one
  `Open` of that address. A link is granted only from a page of the same
  local origin (navigatord's own record of the page, on `Activate`).
  Anything else gets "Your own server opens only from an address you
  type".
- **The socket.** portcullisd's `OpenLocalPort { port }` takes the person
  and persona from the caller's uid; the caller's installed manifest must
  hold `navigator-broker`. The port must be applied now, as a
  this-computer port, to a running instance of that person in that
  persona (`listen_run::local_target`). jaild's `ListenTarget` names the
  jail's own address only while that rule is loaded. **portcullisd
  connects, not jaild**: jaild serves one request at a time, so a 5 s
  connect there would stall every launch; portcullisd is root on the
  host, which the base rules let through, and is threaded. The answer is
  `LocalPort { app_id, app_name, addr }` and the socket; navigatord checks
  the socket's peer is `addr`.
- **The fetcher.** navigator-fetchd started on a Unix socket (not a pipe)
  is the local fetcher: capability mode with no casper, so it can't dial
  or resolve. Each request is one message, `G <url>\n`, carrying one
  socket; it speaks one HTTP/1.1 exchange and closes it. A redirect to the
  same origin comes back as `redirect` for navigatord to follow with a new
  socket (at most 5); a redirect elsewhere is refused.
- **Limits.** One page's requests go one at a time, so a page holds one
  socket at once (well under 8); at most 256 requests a page. Per persona,
  portcullisd hands out at most 32 sockets in any 10 s, a rate rather
  than a count of open sockets, since portcullisd can't see when
  navigatord closes one without holding a copy that would keep it open.
- **Cookies, per app and port.** The fetcher passes on each well-formed
  `Set-Cookie` (`cookie <path> <max-age or -> <name>=<value>`, at most 32,
  `Expires` read as a max-age, `Secure` ones dropped, `Domain` ignored),
  and takes a `C <header>` line with the request, checked as name=value
  pairs before anything is sent. navigatord keeps them in memory per
  (trail jar, app id portcullisd named for that socket, origin with its
  port): another app on the port starts with none, :8080 and :9090 share
  none, an Unrecorded trail's go when it closes, and "Forget this site"
  takes an origin's. At most 50 a jar, 64 jars, 400 days. Not at rest:
  navigatord has no persona key group directory for them, so they end with
  navigatord. The converter's `document.cookie` sees none.
- **Not stored.** Local pages are never written to the page store and
  never served from the cache. Live pages run only for HTTPS pages (D12),
  so a local page shows in the document lane: its scripts run in the
  converter, not live, and there is no web storage for it to keep.
- **The chip.** `Event::LocalServer { id, app }` before `Opened`; the
  address bar shows "Your computer · Photo Server" where an http page
  says "Not private", and the popover says the page doesn't cross a
  network. frescod's strip shows the origin, `http://localhost:8080`.
- Host tests: the parser (`navigator-control`), the local fetcher's
  exchange, redirects and refusals (`navigator-fetch`), navigatord's
  grants and origin check (`local_server.rs`), the address bar and chip
  (`navigator-ui`), portcullisd's rate, jaild's `ListenTarget`. VM check
  `scripts/vm-local-server-check.sh` (37 checks): the server side end to
  end with a stand-in for navigatord (`examples/local_port_ask`), then the
  real navigatord as Navigator's broker with the real fetcher and
  prerender, driven by `navigatord/examples/nav_ask` in the UI's place: an
  untyped Open is refused, a typed one opens and is named as the app's, the
  next open carries the cookie navigatord's jar kept, and nothing of the
  page reaches navigatord's store. The chip itself is covered by the UI's
  host tests.

