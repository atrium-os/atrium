# Navigator UI: trails, pages and the window

**Status:** proposed 2026-11-13, at Shankar's request ("now that pergola and navigator backends
are all almost done, we can do the main navigator UI design … don't have to follow existing
conventions … how to sandbox each window/tab, how the navigator ui layout is, is it intuitive to
browse"). Nothing here is built. §13 lists the decisions, each with a recommendation.
**Mockups:** rendered by Pergola on a live headless frescod in the FreeBSD VM
(`pergola-document/examples/navigator_mockup.rs`; run with
`EXAMPLE=navigator_mockup sh scripts/pergola-showcase-vm.sh main|dark|start`). The PNGs are in
project files under `navigator/ui/`; §5.7 says what each shows and what in them is a stand-in.

This document designs what a person sees and does in the Navigator: how pages are organised,
how a window is laid out, how they move around, and which process holds what. The pieces under
it already exist or are specified:

- the broker, workers and sessions ([atrium-navigator-backend.md](atrium-navigator-backend.md));
- the threat model and its trusted chrome rules ([atrium-navigator-threat-model.md](atrium-navigator-threat-model.md) §2.5, §2.10, §2.11);
- the two lanes and islands ([atrium-navigator-legacy-web.md](atrium-navigator-legacy-web.md) §7, §8);
- Pergola, ScrollView and DocumentView ([pergola.md](pergola.md) §10);
- Limen slots ([limen.md](limen.md)), Fresco pop-ups ([fresco-popups.md](fresco-popups.md)) and the
  origin strip (Fresco F4d);
- Arca cookies and autofill ([arca.md](arca.md) §8, [arca-autofill.md](arca-autofill.md)), Sigillum
  for WebAuthn ([sigillum.md](sigillum.md) §9), Nuntius ([nuntius.md](nuntius.md));
- Forum's window model ([forum.md](forum.md) §2) and PiP ([atrium-player-pip-keys.md](atrium-player-pip-keys.md));
- the look ([atrium-visual-language.md](../design/atrium-visual-language.md)).

---

## 0. The short version

- **The unit is the page, not the tab.** A page is one document at one address, as you saw it.
  It is also the security unit: one jailed worker, one renderer, one GPU VM, one origin. Tabs
  and windows are only ways of arranging pages, so they need no sandbox of their own.
- **Pages are arranged in trails.** A trail is the path you took from where you started:
  following a link adds a page to it, Back walks back along it, and opening a link "on the
  side" makes a branch. The left rail lists trails the way a tab bar lists tabs, and shows
  the current trail's path. Closing a trail puts it away; nothing you looked at is lost unless
  you ask.
- **A page that is not on screen costs nothing.** Documents are inert: no script, no timers.
  So a page can be saved (its recording's address plus the steps you took, backend §8.8) and
  its jail ended, then brought back exactly as it was. The Navigator keeps a few pages live and
  parks the rest. A person can have a thousand trails; the machine runs a dozen jails.
- **You are anonymous until you say otherwise.** Every page starts without cookies
  (legacy-web §5.4.1e). The toolbar says whether this site knows you, and letting it
  remember you is one press on a control the page can't draw.
- **Identity is the system's job, not the toolbar's.** frescod's origin strip names the page's
  origin above every window, where no app pixel can reach. The toolbar's address is a
  convenience, not the proof.
- **Every page says what runs for it** (asked by Shankar, 2026-11-13). A tier indicator at the
  start of the Where field and in the origin strip reads **Static** (tier 1), **Replayed**
  (tiers 2 and 3: the site's scripts ran once, sealed, at conversion; nothing of theirs runs
  now) or **Live code** (tier 4: the site's JavaScript is running, in a jail). A page with an
  island reads "Replayed · 1 live part", and the island itself gets a frame frescod draws.
- **Apps are not pages.** An `atrium-app://` link opens a native app in its own window, after
  a consent moment, and the trail records that you went there.

---

## 1. What the UI is, and what it may hold

The backend's invariant stands unchanged (backend §1): **the UI process holds no authority and
contains no parser.** Pergola's decision stands too (pergola.md §10.4): the chrome (rail,
toolbar, find, zoom, prompts) is a Pergola app in the trusted UI process, and each page is a
jailed document surface embedded in it.

What the UI receives from pages is therefore small and bounded:

| from a page | bound | how the UI uses it |
|---|---|---|
| title | 256 bytes (limen.md §2.1 `Loaded`), control characters stripped, bidi isolated | rail rows, the toolbar, window title. Shaped in-process by the memory-safe shaper (pergola-architecture-review step 4), like any other string |
| anchors | 256 of them | "jump to section" in the find bar |
| link activations | one URL, after a press in the page | handed to the broker, which decides (backend §4.6) |
| embeds list | 64 rects | placing islands and players (limen.md §2.1) |

It never receives document text, pixels, images or favicons. **No favicons** is a decision
(§13, D6): a favicon is an image from the site, so showing one would put a decoder's output in
the chrome, and a site's icon is the easiest thing for a lookalike to copy. The rail shows a
monogram of the registrable domain instead, drawn by the UI from the URL the broker reported.

What the UI holds:
- its Fresco windows, and the right to report slot rects for them (Pergola `SlotBinder`);
- the control-plane connection to `navigatord` (backend §3);
- the trail store (§4.5): page records, which are addresses and steps, never content.

What it does not hold: network, the content store, cookies, credentials, any page's scene. A
compromised UI can lie in its own chrome (a wrong title in the toolbar). It can't read a page,
fake the origin strip, or fill a password, because none of those pass through it.

---

## 2. Processes, and what each window and page gets

### 2.1 One page

A live page is these processes, each in its own jail:

| part | jail | holds | per |
|---|---|---|---|
| document worker | zero capabilities (backend §4.7) | the page's document and its in-page history | page |
| page renderer | frescod split-mode renderer, no GPU fd, its own GPU VM (threat model §2.5.2) | the page's scene buffer | page window |
| converter | one-shot, rlimits (backend §4.7d) | the page's bytes while converting | conversion |
| fetcher | Nuntius; anonymous fetches shared per conversion, cookied ones through Arca's courier for one (persona, origin) | network, nothing else | request or (persona, origin) |
| island engine (tier 4) | origin-scoped, forked from a zygote (legacy-web §7.5, §8.1) | that island's script state | island |
| media player | Limen `media-player`, a throwaway jail per embed (limen.md §13 item 4) | decoders | embed |
| credential field | Arca's `arca-ui`, a system role | the value, never the page | field |

The page's window is a Limen slot child pinned into the Navigator window (limen.md §5). So the
page never sees the chrome's pixels, the chrome never sees the page's, and the page can't draw
over the toolbar, the rail or a prompt: frescod clips it to its slot.

### 2.2 Tabs, trails and windows are not boundaries, and why that is right

Browsers made the tab a process because the tab was the unit they could afford to isolate.
Here the page is already isolated more finely than any tab model needs:

- **Two pages in one trail** are two jails. The page you came from cannot see the one you are on.
- **Two trails on one site** are two sets of jails that share only that site's cookie
  partition, and only once you let the site remember you (§6.2). That is the same sharing a
  person expects from "I'm signed in to this site".
- **Two windows** are two arrangements of the same kind of thing.

So the question "how do we sandbox each tab" becomes "how do we separate what pages share",
and the answer is the **persona** (§2.4).

### 2.3 How many UI processes

**One `navigator-ui` per user session**, for all windows (§13, D7). It holds no authority and
parses only bounded titles, so splitting it per window would buy a crash domain and cost a
Pergola instance, a font set and a control-plane connection per window. A crash of the UI
takes the chrome down; the pages are saved records, so restarting it brings every trail back
where it was (§4.5).

The alternative, one UI process per window, is cheap to switch to later because windows share
nothing but the trail store.

### 2.4 Personas: what pages may share

A **persona** is a cookie and credential namespace. Arca's partition key becomes
(persona, site) instead of (site) (arca.md §8.2). Every trail belongs to one persona, and every
page in it uses that persona's partition.

| persona | cookies and logins | trails and history | shared store |
|---|---|---|---|
| **Personal** (the default) | kept in Arca, while unlocked | kept | anonymous conversions shared per user (threat model §2.6) |
| **Unrecorded** | in memory, gone when its last trail closes | never written; parked pages stay in memory | none: conversions are private and never stored |
| **named** ("Work", "Shopping", added later) | their own Arca partition | kept, filtered by persona | as Personal |

Unrecorded is what browsers call private browsing, with one difference that matters: since a
page is anonymous until you let a site remember you (§6.2), Personal trails are already private
from sites. Unrecorded is privacy from *this machine*: nothing written, nothing remembered.

Named personas are the browser "container" idea (two accounts on one site, work kept apart from
home). They cost nothing new in the jail design, only a key in Arca. **Recommend shipping
Personal and Unrecorded first, and named personas when someone asks** (§13, D5).

---

## 3. First principles: what is different here, and what it buys the UI

Each of these is a property the backend already has. The UI's job is to turn it into something
a person can feel.

1. **A page is inert.** No script runs at view time in the document lane (profile §1.1), so a
   page left alone does nothing. Nothing is lost by stopping it and nothing changes while it is
   away. → Pages can be parked and brought back freely (§4.4). There is no "this tab is using
   a lot of memory" and no tab discarding that reloads the page under you.
2. **A page is a saved thing.** The content store keeps what you saw, by address. → History is
   not a list of URLs to fetch again; it is the pages themselves. Going back shows what was
   there, even offline (§5.5).
3. **Following a link is the broker's decision.** The href must be one the document carries,
   and the broker resolves it (threat model §2.10). → The UI never needs to warn about a link
   a page invented; it shows where a link will go from the broker's answer.
4. **Anonymous is the starting state.** → The UI shows "known / not known" per site, not a
   wall of permissions (§6.2).
5. **Identity is drawn by frescod.** → The toolbar can afford to be friendly (title first,
   address second), because it is not the security indicator (§6.1).
6. **The lane is per region.** → The marker goes on the region, not on the page (§6.3).

---

## 4. Organising pages: trails

### 4.1 The options

| | **A. Tabs** (the convention) | **B. Trails** (recommended) | **C. No tabs: every page a window** |
|---|---|---|---|
| what a person sees | a strip of tabs above the toolbar; Back and Forward per tab | a rail of trails beside the page; the current trail shows its path; Back walks the path | each page a Forum window; Forum's switcher and groups do the arranging |
| opening a link "for later" | a new tab at the end of the strip, cut off from where it came from | a branch under the page it came from, in the same trail | a new window |
| "where was I?" | the tab's title; the path is hidden behind Back's long-press | the path is on screen | the window list |
| a thousand open things | a tab strip that no longer shows titles; tab-discard heuristics | trails put away into the rail's Earlier list; parking makes them free | a thousand windows |
| fits Atrium | works, but throws away §3.1 and §3.2 | uses them: pages are saved things, trails are their arrangement | over-uses Forum: pages are not apps, and the WM would need a "this window is a page of that trail" model |
| familiarity | total | high: vertical tabs are common, and a trail behaves like a tab that remembers its path | low |

**Recommend B** (§13, D1). It is a tab model where the tab remembers the path, the branches
and every page, which is what people already try to get from tabs by never closing any. A
person who ignores branches and the path uses it exactly like vertical tabs.

### 4.2 What a trail is

- A trail is a **tree of pages**, with one **current page**. It starts from a typed address,
  a search, a link opened from another app, or a branch split off another trail.
- **Following a link** (a plain click) adds the destination as a child of the current page and
  makes it current. If the current page already had children, the new one is the newest
  branch, and older branches stay in the tree.
- **Back** makes the current page's parent current. **Forward** makes its newest child current.
  Long-press or right-click on Back or Forward lists the whole path.
- **Opening on the side** (middle-click, Ctrl-click, or "Open beside" in a link's menu) adds the
  destination as a child **without** moving you. It loads in the background, and appears in
  the rail under the current page with a dot until you look at it. This replaces "open in new
  tab" and keeps the link's origin in view.
- **Moving a branch out** (drag it in the rail, or "Make its own trail") turns a branch into its
  own trail.
- **Reload** fetches the page again. The result is a **new version of the same page**, not a
  new page; the previous version is still in the store and the page's menu can show it
  ("As seen at 10:42").

A trail's title is its current page's title. The rail row shows that and a monogram for the
current page's registrable domain.

### 4.3 Back means the previous page, not the previous click

Inside a page, the document lane replays recorded transitions: opening a section, switching a
tab pane, expanding a menu. The broker keeps a per-page history of these (backend §4.4). The
question is what Back does after you opened three sections of a page.

| | **Back undoes in-page steps first** | **Back goes to the previous page** (recommended) |
|---|---|---|
| browser parity | matches single-page apps that push history for every pane | matches ordinary sites |
| after expanding sections | Back collapses them one by one before leaving | Back leaves; the page keeps its expanded state for when you return |
| what a person expects | rarely: "Back closed my accordion" is a known complaint | almost always |

**Recommend: Back goes to the previous page** (§13, D3), with one exception: a transition that
changed the page's URL (a route change in an app-like page, recorded as such) counts as a page
in the trail, because the site meant it as one. In-page state is kept with the page, so
returning shows the sections you had open. Ctrl-Z, or Esc on what you just opened, undoes the
last in-page step.

### 4.4 Live and parked pages

The broker bounds live sessions: 16 and 64 MiB by default, refusing past either (backend
§4.4a), with an idle timeout of 30 minutes and an 8-hour cap (§4.4b). The UI turns those bounds
into something no one has to manage:

- **A page is live** when it is on screen, or is the parent of a page on screen, or was looked
  at recently within the budget.
- **Every other page is parked**: the UI sends `Save`, keeps the address the broker answers,
  and sends `Close`. The jail ends.
- **Showing a parked page** sends `Restore`. The broker starts a fresh worker on the stored
  recording and replays the steps, and the result hashes to the saved document or is refused
  (backend §8.8). Under TCG this took 2.9 s; jail start is about 51 ms under HVF, so on hardware
  the target is under 100 ms and the restore is shown as the page fading in, never as a spinner.
- **An idle expiry is a park, never a loss.** The UI saves a page before its idle timeout
  falls due; the "that timed out" event the broker sends for a page it could not save becomes
  "this page will be fetched again" on the page, with a button.
- **Pages that can't be parked as they are** stay live while they have something to lose, and
  the rail shows why:
  - a page whose steps are not all in its recording's transition table is unsaveable (backend
    §8.8). It stays live; if the budget forces it out, it comes back as the published page;
  - a page with an island that is playing (§6.3). Parking stops the island; the player keeps
    its position (atrium-player.md), so playback resumes there;
  - a page with typed but unsent form text keeps it in the worker. It stays live until sent
    or abandoned.

Choosing which page to park is the UI's job, as the backend intends ("mechanism here, policy
with the caller", §4.4a). The rule is least recently looked at, never the page on screen or the
current page's parent.

### 4.5 The trail store

Trails are stored by the UI as small records: per page, the URL, the title, the broker's saved
address, the version history, and the tree links. They live in the user's Tessera home,
encrypted with the session like other user data, and roam with the session (forum.md §2.4).
Nothing in a record is page content; the content is in the content store under its address.

- **Closing a trail puts it away.** It moves to the rail's **Earlier** section, grouped by day,
  and is kept 30 days unless kept for good (§5.6).
- **Forgetting a trail** removes its records now. Pages only it referred to become unreachable
  and are collected with the store's ordinary rules; private (cookied) conversions are deleted
  with them (threat model §6.7).
- **Unrecorded** trails have no records on disk at all.

---

## 5. The window

### 5.1 Layout options

| | **A. Classic** | **B. Rail + toolbar** (recommended) | **C. Bare page + command palette** |
|---|---|---|---|
| shape | tabs strip, toolbar, page | trail rail on the left, one toolbar row, page | page only; Ctrl-L opens a floating command field, everything else by keyboard |
| vertical space for the page | loses two rows | loses one row | loses none |
| discoverable | yes | yes | no: a new user sees a page and no way to go anywhere |
| fits wide screens | wastes width | uses width | — |

**Recommend B** (§13, D2), with a **reading view** that hides the rail and toolbar until the
pointer reaches the top edge or a key is pressed. That gives C's page when someone wants it
without making it the only way in.

### 5.2 The recommended layout

```
┌──────────────────────── frescod origin strip (24 px, not the app's) ─────────────────────┐
│ https://atlas.example                                                                     │
├─ rail (248 px; 56 collapsed) ─┬──────────── toolbar (40 px) ────────────────────────────────┤
│ [Personal ▾]        [+ Trail] │ ← → ⟳  [ Atrium (architecture) · atlas.example/wiki/… ] ◌ Aa ☆ ⋯ │
│                               ├─ prompt row (only when something is asked) ─────────────────┤
│ TODAY                         │                                                             │
│ ● Atrium (architecture)       │                                                             │
│   ├ Search: atrium            │                     page (Limen slot)                       │
│   ├ Atrium (architecture)  ◀  │                                                             │
│   │  └ Impluvium           •  │                                                             │
│ ○ Rust 1.92 release notes     │                                                             │
│ ○ FreeBSD Handbook: Jails     │                                                             │
│ EARLIER                       │                                                             │
│ ○ Bread hydration tables      │                                                             │
└───────────────────────────────┴─────────────────────────────────────────────────────────────┘
```

Dimensions follow the visual language: toolbar 40 px (§8 reference dimensions), rail rows 32 px
(list row default), 28 px for nested path rows (dense tier), icons 16 and 20 px (Phosphor, §9).
The rail and toolbar sit on `bg-surface`, the page on its own canvas, with one 1 px
`border-default` seam between them. One accent, used for the current trail's marker, focus and
the primary button of a prompt. No shadows except on pop-ups (§5 shadow policy).

**The rail.**
- Top: the persona control (a chip naming the persona, its menu switches persona for new
  trails) and **New trail** (Ctrl-T).
- **Today** lists open trails, newest activity first; **Pinned** above it once something is
  pinned; **Earlier** below, put-away trails by day.
- A trail row: monogram tile, title on one line (truncated), and state glyphs on the right:
  a speaker while audio plays, a person while a site there knows you, a dot for an unseen
  branch.
- The **current trail is expanded** into its path: every page from the root to the current
  one, then the current page's branches, indented. Older side branches fold into "2 more".
  This is the "where am I" answer, always on screen.
- Collapsed (Ctrl-B, or a narrow window), the rail is 56 px of monograms, the width of Forum's
  dock rail.

**The toolbar**, left to right:
- **Back, Forward, Reload** (ghost buttons, 20 px icons). Reload becomes **Stop** while loading.
- **The Where field** (§5.3).
- **Presence** (§6.2): an outline person when this site doesn't know you, a filled person and
  the site's name when it does.
- **Aa**: reading controls (§7).
- **Keep** (☆): keeps the page for good (§5.6).
- **More** (⋯): find, print, versions of this page, open in reading view, Navigator settings.

**The prompt row** appears between the toolbar and the page only while something needs an
answer (§6.4). It pushes the page down rather than covering it, so a prompt never hides what it
is about.

### 5.3 The Where field

**At rest** it shows two things: the page's **title**, and its **address** with the registrable
domain in `text-primary` and the rest in `text-tertiary` (threat model §2.11's emphasis rule;
IDN as punycode when the script is mixed or confusable). `http:` pages show "Not private" in
`warning-fg` before the address, from the broker's `insecure` flag.

**Focused** (click, Ctrl-L) it becomes a text field holding the full URL, selected. Typing shows
suggestions in a Fresco pop-up below it (fresco-popups.md), from local sources only:

1. open trails and their pages (switching to one rather than opening it again);
2. kept pages;
3. pages from history, by title and address;
4. "Search for …" with the chosen search provider, as the last row.

**Nothing is sent while typing** (§13, D4). Browsers send every keystroke to a search provider
for suggestions; here the first byte leaves the machine when you press Enter. Enter on
something that parses as an address goes there (with `https:` assumed); anything else is a
search, whose result page is an ordinary page in the trail.

The search provider is a setting with no default chosen by us: the first search asks once,
from a short list, and remembers the answer.

### 5.4 Starting a trail

A new trail opens on **Start**, a page the UI draws itself (no hostile content, nothing
fetched): the Where field focused, below it pinned and recent trails and kept pages. No feeds,
no suggestions from a server, nothing that phones home on open.

### 5.5 History is the pages themselves

Because pages are in the store (§3.2), history is browsable as what you saw: **Ctrl-H** opens a
history view (a page the UI draws) listing trails and pages by day, and opening one shows the
stored version, offline, then offers "Fetch the current version". Clearing history is
per day, per site, or all, and removes trail records and private conversions together.

### 5.6 Keeping pages

**Keep** (☆, Ctrl-D) marks a page to be kept for good: its record leaves the 30-day expiry and
its stored version is pinned in the store, so it can be read offline indefinitely. Kept pages
are listed in Start and in the Where field's suggestions. This is bookmarks and read-later in
one, and it costs no network to read later.

### 5.7 The mockups

Four frames, each 1280×720, drawn by Pergola and composited by frescod:

1. **`navigator-ui-main-light.png`**: the recommended layout. The rail with the current trail
   expanded into its path (a search, two pages, then the current page with two branches, one
   unseen), four other trails (one playing audio) and three put away; the toolbar with the
   Where field at rest (title, then the address with the registrable domain emphasised),
   presence showing *Anonymous*, Aa, Keep and More; the page at a comfortable reading width.
   The tier chip reads **Static**, and so does the strip.
2. **`navigator-ui-dark-prompt-island.png`**: the same page in dark, after the reader let
   atlas.example remember them (presence filled, the person glyph on the trail's row), with a
   download asked in the prompt row and a video island framed and tagged "live code ·
   media.example". The tier chip reads **Replayed · 1 live part**.
3. **`navigator-ui-start-where.png`**: a new trail on Start with the rail collapsed to
   monograms, the Where field focused on "impl" and its suggestions open: an open trail's page
   first ("Switch to"), then kept and history pages, then the search, marked as sent only on
   Enter.
4. **`navigator-ui-live-code.png`**: a web app running whole in the engine (a stand-in mail
   inbox at mail.example), rail collapsed: the tier chip and the strip read **Live code** in
   amber, and presence shows the site knows the reader.

What is a stand-in: the 24 px strip is drawn by the mockup to show where frescod's own strip
goes (headless direct mode has none); the page is a DocumentView in the mockup's own process,
where the real one is a jailed Limen slot; the island's frame and tag are drawn by the mockup
where frescod would draw them; the suggestions are drawn in the window where the real ones are
a Fresco pop-up; and the trails and sites are made up.

---

## 6. Trust on screen

Four things on screen carry trust. Each is drawn by the process that knows the fact, never by
the page, and each is placed where the page can't draw.

### 6.1 Who the page is: the origin strip (frescod)

frescod's 24 px strip (F4d, a9bf150) names the focused window's origin in its own font, above
every app. For the Navigator two changes are needed:

- **The strip follows the page, not the chrome.** When the Navigator window is focused but
  the focus is in the toolbar or rail, the strip should still name the page shown in that
  window. frescod knows which slot child is the page (navigatord opened its connection through
  the chrome channel and set its origin), so the rule is: the focused window's origin, or if it
  has none, the origin of its single `page` slot. Proposed as a frescod change (§12).
- **The strip carries the tier** (§6.3): `static`, `replayed`, `replayed + 1 live part` or
  `live code` after the origin, set by navigatord over the chrome channel with the origin,
  never by the renderer.

Fullscreen keeps the strip (threat model §2.11). The toolbar's address is a convenience; the
strip is the proof, and the two always agree because both come from the broker.

### 6.2 Whether the site knows you: presence (the UI)

Every page starts anonymous: no cookies, no logins (legacy-web §5.4.1e). The **presence**
control says which state this page is in, in words in its menu:

- **"atlas.example doesn't know you"** (outline person). Menu: *Let this site remember me*.
  Pressing it sends the broker `Cookies { allow: true }`; from then on that site's pages in this
  persona carry its cookies, and the conversions are private.
- **"atlas.example knows you"** (filled person, the site's name shown beside it). Menu: which
  account Arca has for this site if any, *Sign out here* (ends the session cookies), and
  *Forget this site* (destroys its data key, arca.md §8.6).

The site can ask too: a consent button pressed in the page, or a redirect that needs a cookie,
turns the view cookied by the backend's own rules (legacy-web §5.4.1e). The control then shows
it, and the rail row gains the person glyph, so the change is never silent.

This is the first-principles inversion of a browser's padlock and permission prompts: the
question people actually have is "does this site know it's me?", and here it has a direct,
always-visible answer.

### 6.3 What runs for this page: the tier indicator

*Asked by Shankar, 2026-11-13: "some kind of indication of the tier of the page: static, dynamic
content recorded and replayed (both secure), vs javascript running in a sandbox (eg. gmail), to
indicate to the user the level of security". This replaces the first draft, which marked only
islands and engine pages and left documents unmarked.*

The four tiers of legacy-web §5.3 are three different answers to the one question a person can
act on, **"is this site's code running on my machine right now?"**:

| indicator | tiers | what it means, in the words the popover uses | colour |
|---|---|---|---|
| **Static** (document glyph) | 1 | "This page is text and pictures. No code from atlas.example ran to make it, and none runs now." | neutral |
| **Replayed** (film-strip glyph) | 2, 3 | "atlas.example's scripts ran once, sealed off, to record what this page can do. What you click now replays those recordings. None of the site's code runs now." | neutral |
| **Live code** (lightning glyph) | 4, whole page | "atlas.example's JavaScript is running now, in a jail of its own. It can't see your other pages, your files or other sites' logins. It can see what you type into this page and send it to its own site." | `warning-fg` |
| **Replayed · 1 live part** | 1–3 with islands | the page as Replayed (or Static), plus "1 part of this page runs live code: the video player from media.example" | the live part in `warning-fg` |

Tier 3 (a known script swapped for a native behaviour by its hash) is shown as Replayed: to the
reader it is the same promise, no site code at view time, and the difference is how the
converter got there.

**Why three, and why these words.** The security difference that matters to a reader is between
the first two and the third: in Static and Replayed no site code runs at view time, so the
document lane's guarantees hold (no scripted timers, no scripted fetch, nothing typed leaves
except by a form the broker builds; legacy-web §7.4). In Live code they do not. So the two safe
tiers share the calm neutral look and differ only in their word, and Live code alone gets
colour. People who don't care learn "amber means the site's own code is running", which is the
one fact worth learning; people who do care get the finer word.

**Where it is drawn.**
- **In the Where field**, at its left end where browsers put the padlock: a 22 px chip with the
  glyph and the word, drawn by the UI from the broker's report. Clicking it opens the popover
  with the sentence above, the origins involved and, for Live code, the document lane version
  if the converter produced one ("Read as a document instead").
- **In the origin strip** (§6.1), after the origin, in frescod's own font: `static`,
  `replayed`, `replayed + 1 live part`, `live code`. This is the copy no app can fake. navigatord
  sets it with the origin over the chrome channel; the renderer never names its own tier.
- **On each island**: a 1 px frame and a tag, "live code · media.example", drawn by frescod as
  part of a new slot kind (`island`, as `credential` draws its ring; limen.md §5.1). The page
  can't draw over it or remove it. While the page's content overlaps the frame the island takes
  no input (threat model §2.9), and the frame is drawn dashed.
- **In the rail**, a trail whose current page is Live code shows the lightning glyph after its
  title, so a running web app is findable without opening it.

**Who decides the tier.** The broker, from what it holds: a session opened from a recording
with no transitions is Static; one with a transitions table attached (or tier-3 substitutions
in its recording) is Replayed; a page served by an engine jail is Live code; each live island
the broker bound adds a live part. The worker and the page never report their own tier, so a
compromised worker can't claim Static.

**It changes in front of the reader, and says so.** Progressive display (backend §4.6) shows a
tier-1 document first and swaps in the replayed one at an idle moment, so a page can go from
Static to Replayed while it is read; that is a step to a richer page with the same promise, and
the chip changes without comment. A step towards Live code (an island starting, or a whole page
handed to the engine) never happens without a press, and the chip's change is announced to
accessibility tools.

**Not a rating of the site.** The indicator says what runs, not whether the site is honest. A
Static phishing page is still phishing; that is what the origin strip, the registrable-domain
emphasis and Arca's exact-recipient rule are for (threat model §2.11). The popover says so in its
last line, so "Static" is never read as "safe to give your password to".

### 6.4 What the system is asking: the prompt row (the UI)

Questions that need the person, drawn by the UI in the prompt row between toolbar and page:

| prompt | when | answers |
|---|---|---|
| open an app | an `atrium-app://` link (navigator.md §2.2) | *Try it* (ephemeral jail, insula trial-launch) · *Open* · *Not now*. Shows the publisher and the capabilities it will ask for |
| hand off a link | an allowlisted scheme (`mailto:`), threat model §2.10 | *Open in Mail* · *Copy address* |
| save a download | a link to a file | *Save* · *Open in viewer* (a jailed viewer, threat model §2.10) · *Cancel*; the row shows the file's origin |
| not private | a form on an `http:` page | *Go back* · *Continue* |
| fetched again | a page that could not be restored (§4.4) | *Fetch now* |
| play | a media embed beyond the page's autoplay policy | *Play* |

Rules:
- **The row never asks for a secret.** Passwords, passkeys and codes go through Arca's
  credential slots and Sigillum's sheet, which are system surfaces of their own
  (arca-autofill.md §5, sigillum.md §7.2). A page that draws a fake prompt row inside its
  own slot can at most draw buttons that do nothing.
- **A press counts only after the row has been still and unobscured for 500 ms**, the same rule
  as credential slots, so a page can't move something under the pointer just before it
  appears.
- **One prompt at a time per page**, newest last; a page can't queue prompts faster than one a
  second, and a dismissed prompt doesn't come back for that page until it is reloaded.
- **Arca's save-password offer** and **Sigillum's sheet** are not in this row: they are Arca's
  and Sigillum's own surfaces (notification layer and system sheet).

---

## 7. Reading controls

**Aa** opens a pop-up with the controls the backend already supports, so none needs new
machinery:

- **Text size** (backend §4.9 item 2) and **zoom** (item 1), each as a stepper with a reset;
  Ctrl-plus/minus/0 is zoom, and ScrollView owns pinch and Ctrl-wheel (pergola.md §10.4).
- **Theme**: *Follow system* (the desktop appearance, visual-language §15), *Light*, *Dark*.
  The document worker re-lays-out with the media context; no re-conversion.
- **Reading width**: *Full* or *Comfortable* (a measure of about 70 characters, centred). The
  page's CSS is laid out at the narrower width, the way a narrow window would.

Choices are remembered per site and per persona, in the trail store, never sent anywhere.

**Find** (Ctrl-F) is a bar under the toolbar (Pergola's find UI, pergola.md §10.4): matches
come from the page's worker over its role channel as counts and the current match's rect,
never as text. The anchors list from `Loaded` gives "Jump to section".

---

## 8. Windows, split, fullscreen, picture-in-picture

These belong to Forum (forum.md §2.3: the WM owns geometry, the human owns intent), and the
Navigator does not reimplement them.

- **New window** (Ctrl-N) opens a Navigator window on Start. Trails belong to a window; dragging
  a trail out of the rail makes a new window with it.
- **Side by side**: "Open beside in a new window" on a link or a trail asks Forum to group the
  new window with the current one (forum.md §2.3's group/split intent, not yet in `forum-ctl`). Two pages side by side are two Navigator
  windows, each with its own strip origin when focused.
- **Fullscreen** (F11): the rail and toolbar hide, the strip stays (§6.1). Moving the pointer
  to the top shows the toolbar over the page, as a Navigator pop-up, so the page is never
  covered by its own content.
- **Reading view** (§5.1) is fullscreen inside the window: rail and toolbar hidden, shown on
  the top edge or Esc.
- **Picture-in-picture** is the player's (atrium-player-pip-keys.md §3): the video leaves its
  slot for Forum's `pip` layer, and the page shows a placeholder. The rail row keeps the speaker
  glyph, and closing the trail ends PiP.

---

## 9. Keyboard

System shortcuts go to Forum first (limen.md §6). Within the Navigator:

| keys | does |
|---|---|
| Ctrl-L | focus the Where field |
| Ctrl-T | new trail |
| Ctrl-W | put away the current trail |
| Ctrl-Shift-T | bring back the last trail put away |
| Alt-Left / Alt-Right | back / forward along the trail |
| Ctrl-Tab / Ctrl-Shift-Tab | next / previous trail (most recently used order) |
| Ctrl-1 … 9 | the nth trail in the rail |
| Ctrl-B | collapse or expand the rail |
| Ctrl-H | history view |
| Ctrl-D | keep this page |
| Ctrl-F, Esc | find, close find |
| Ctrl-plus, Ctrl-minus, Ctrl-0 | zoom (ScrollView) |
| Ctrl-Shift-N | new Unrecorded trail |
| F6 | move focus between rail, toolbar and page |
| Ctrl-Enter on a link | open beside (§4.2) |

Focus inside the page is the page's; Tab at its end hands focus back to the chrome
(limen.md §6 `FocusOut`).

---

## 10. Accessibility

- The rail is a tree (atrium-ax role `tree`), each trail an item, its path nested. The toolbar
  is a toolbar; the prompt row is a live region announced when it appears.
- The page slot is an `embedded-content` node; atrium-ax grafts the page worker's tree there
  when the bridge exists (limen.md §9). Until then a screen reader reads the page's window when
  it is focused.
- Everything in §9 works without a pointer. Reading controls cover text size without zoom, for
  people who need large text but not wide pages.

---

## 11. Is it intuitive?

The test for each piece is "what would a person who has used Chrome or Safari for ten years do,
and does it work?"

| habit | what happens here |
|---|---|
| clicks links, presses Back | works as before; Back goes to the previous page (§4.3) |
| opens links in new tabs to read later | Ctrl-click and middle-click still do that; the result appears under the page it came from rather than at the end of a strip |
| keeps 200 tabs open | works and costs nothing (§4.4); old ones drift into Earlier |
| closes a tab by mistake | Ctrl-Shift-T, or it is in Earlier, exactly as it was |
| types a search in the address bar | works; suggestions come from their own history only, and the search is sent on Enter |
| expects to stay signed in | the first visit to a site is anonymous; signing in or pressing the site's own consent button makes the site remember them, and the toolbar shows it. This is the one habit that changes, and it is the point |
| watches video | the video plays in a frame marked "live code", and the page says "Replayed · 1 live part"; PiP and media keys work |
| looks for the padlock | in its place is the tier chip (Static, Replayed, Live code); the strip names the site, "Not private" marks `http:`, and presence says whether the site knows them |

The new ideas are four: the path in the rail, presence, the tier chip and the island frame. Each is visible
rather than hidden in a menu, and each answers a question people already ask ("how did I get
here?", "am I signed in here?", "is this part different?").

What needs checking with real use, before it is settled: whether a branch under its parent
page is easier to find than a tab at the end of a strip, and whether people read the presence
control. The rail and presence should be measured with a few people once the UI runs, not
argued further on paper.

---

## 12. What this needs from other components

| owner | change | why |
|---|---|---|
| navigatord | page-to-page history stays the UI's (trails); the broker keeps in-page history only. Add a `url_changed` flag on transitions that changed the URL | §4.3 |
| navigatord | a persona in `OpenSession` and in Arca tickets | §2.4 |
| navigatord | `Save` from the jailed host into the store on a Tessera volume (M4's open item) | §4.4 |
| Limen (L6) | `doc-viewer` with the Navigator's worker as the child, opened by navigatord and bound into the UI's window; `LinkHover { url }` from child to parent (advisory, for showing a link's destination) | §2.1 |
| Limen + frescod | slot kind `island`: frescod draws a 1 px frame and the "live code" tag, dashed while obscured | §6.3 |
| frescod | origin strip follows a window's `page` slot when the window has no origin of its own; a tier label after the origin, set with `SET_ORIGIN` | §6.1, §6.3 |
| navigatord | a tier in each session's report (static, replayed, live code, plus live islands), derived by the broker | §6.3 |
| Arca | partition key (persona, site); an in-memory partition for Unrecorded | §2.4 |
| Forum | the group/split intent of forum.md §2.3 in `forum-ctl` (it has `Focus` and display intents today), used for "open beside" | §8 |
| Pergola | a tree list widget (VirtualList rows with indentation and disclosure); a toolbar strip; the find bar (already planned) | §5.2 |

---

## 13. Decisions, with recommendations

| # | question | options | recommend |
|---|---|---|---|
| **D1** | How are pages organised? | A tabs · **B trails** · C every page a window (§4.1) | **B.** A tab that remembers its path, its branches and every page; used plainly it is vertical tabs |
| **D2** | Window layout | A classic tabs + toolbar · **B rail + one toolbar** · C bare page + palette (§5.1) | **B**, with a reading view that hides the chrome |
| **D3** | What Back means | in-page steps first · **previous page** (§4.3) | **Previous page**, with URL-changing transitions counted as pages, and Ctrl-Z for in-page steps |
| **D4** | Where-field suggestions | network suggestions as browsers do · **local only, search sent on Enter** (§5.3) | **Local only.** Nothing leaves the machine until Enter |
| **D5** | Personas | one jar per site · **Personal + Unrecorded now, named personas later** · named personas now (§2.4) | **Personal + Unrecorded now.** Named ones cost only an Arca key, so they can follow when wanted |
| **D6** | Favicons | show site favicons (decoded in the page's jail, validated) · **monograms only** (§1) | **Monograms.** No site pixels in the chrome; lookalikes can't copy a monogram any better than the name |
| **D7** | UI processes | **one per user session** · one per window (§2.3) | **One per session.** It holds nothing worth isolating per window, and the switch is cheap later |
| **D8** | Tier indicator (revised 2026-11-13 at Shankar's ask) | **three words on every page: Static, Replayed, Live code**, plus live parts and island frames · four tiers shown as numbers · two states only, safe and live (§6.3) | **Three words.** Static and Replayed share the calm look because neither runs site code now; Live code alone gets colour |

Settled by earlier decisions and only applied here: the chrome is a Pergola app in the trusted
UI process (pergola.md §10.4); pages are jailed document surfaces in Limen slots; the origin
strip is frescod's; credentials never pass through a page or the UI (threat model §6); cookies
are off until the view is cookied (legacy-web §5.4.1e).

---

## 14. Build order

Each step runs in the VM on headless frescod before the next.

1. **Static shell.** `navigator-ui` crate: window, rail, toolbar, prompt row, Start, drawn from
   a fake trail store, with the page as a placeholder slot. The mockup example is its seed.
2. **One live page.** Connect to `navigatord`: open a session, bind the page through Limen L6,
   Back/Forward along a one-page trail, link activation, the origin strip following the slot.
3. **Trails.** The tree, open beside, put away and Earlier, the trail store on disk.
4. **Parking.** Save and restore under the broker's budget; measure restore time on the Mac
   (HVF) and the R9700 box, and set the live budget from it.
5. **Presence and personas.** The cookie action, Arca partitions, Unrecorded.
6. **Islands and prompts.** The `island` slot kind, the prompt row with its press rule, downloads,
   app hand-off.
7. **Reading controls, find, history view, keyboard map, accessibility tree.**
