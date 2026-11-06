# Fresco: spanning a fullscreen window across monitors

Status: **accepted** (Shankar, 2026-11-06: all six §9 recommendations). Owner: Fresco thread (frescod, display
kmod, gpusim), with Forum for the policy and the user-facing controls.

Builds on display 3.0 (several monitors, atomic `COMMIT_MULTI`, monitor identity), Forum's
per-monitor layout (`forum-ctl outputs/arrange`), and the zoom intent (forum.md §2.3).

## 1. The ask

A game should be able to fill two, three or more monitors as one picture, like a racing sim
on three screens. A game that drives each monitor itself already works on Atrium: it opens a
window per monitor and Forum places them. This spec covers the other case, which is how most
games on other platforms get triple-screen support: the system presents several monitors as
one wide surface, and the game renders a single frame for all of them.

## 2. What other platforms do

| Platform | How | Weak points |
|---|---|---|
| Windows: AMD Eyefinity, NVIDIA Surround | The driver groups monitors into one display (say 5760x1080). The game sees one screen and renders one frame; the driver splits it and flips the outputs together, with their timing synced in hardware. Bezel compensation is a driver setting that adds hidden pixels behind each monitor's frame. | Same mode and refresh on every monitor. Set up in a driver panel, outside the OS. The desktop also becomes one huge screen (taskbar stretched, dialogs centred across a bezel), so many people turn it on only to play. The game gets one flat projection, so angled side screens look stretched unless it knows about the layout. |
| Games that drive each monitor (iRacing, some flight sims) | One view per screen, each with its own angle and field of view. | Only the games that implement it. |
| X11 (Xinerama, TwinView) | All screens were one root window, so a fullscreen game spanned by accident. | Spanned when you didn't want it; no bezel handling; tearing between outputs. |
| Wayland compositors, macOS | No spanning: a fullscreen game gets one monitor. | Triple screens need the game to do it all, or don't work. |
| Consoles | One output. | — |

The two lessons: spanning must not turn the whole desktop into one screen (Eyefinity's main
annoyance), and the game should be told where each monitor is inside the surface, so a game
that cares can still render a correct view per screen into one surface.

## 3. The model

**A span group is a set of monitors that a fullscreen window may cover as one surface.** The
user makes it (Forum: `forum-ctl span A B C`, and a settings panel later); Forum remembers it
per set of monitors, as it remembers arrangements. Nothing else changes: the desktop stays
per-monitor, windows, the strip, notifications and dialogs keep living on one monitor each.

**Spanning happens only through zoom.** A game asks for fullscreen the same way any app does
(the zoom intent). If the window is on a monitor that belongs to a span group, Forum zooms it
over the whole group instead of the one monitor, unless the user turned spanning off for that
app. Apps can't pick monitors or ask for a span by geometry (forum.md §0: no ambient screen
authority); a game that wants per-monitor windows still opens several windows.

**The game is told the layout.** When a window is zoomed over a group, frescod sends
`EV_WINDOW_SPAN { window_id, tiles: Vec<Tile { rect: [x, y, w, h], present: bool }>, hidden: Vec<[x, y, w, h]> }` along with
the resize: `tiles` is each monitor's part of the surface and `hidden` the bezel strips. A game
that knows nothing about it renders one wide frame (Eyefinity behaviour). A game that cares
renders one view per tile, with that monitor's angle, into the same surface: correct
perspective without managing several windows. `EV_WINDOW_SPAN` with no tiles means the span
ended. The event is sent to the window's owner only.

## 4. Geometry

- **Same mode on every monitor of a group** (resolution and refresh). Forum picks the best
  mode all members share when the group is made; a group with no common mode is refused, with
  the reason. Mixed sizes (a 1440p centre with 1080p sides) are out of v1: they need either
  letterboxing or an L-shaped surface, and Eyefinity's experience says few people want it.
- **One row or a grid** in the group's arrangement order, landscape only in v1 (portrait
  needs rotated scanout, which display 3.0 doesn't have).
- **Bezel compensation**: off by default; on, Forum adds a hidden strip between neighbours,
  sized from the monitors' physical size (EDID) and a bezel width the user sets once per
  monitor (millimetres, default 0). The surface includes the hidden pixels, so lines carry
  straight across the frames; frescod never scans them out.
- Surface size limit: `MAX_WINDOW_DIM` (16384) per side, which allows 4x 4K wide or 3x 5K.
  Larger groups are refused.

## 5. Presenting a span

This is where spanning differs from "a window that happens to be big", and where most of the
work is.

- **One atomic flip for the whole group.** Today frescod flips each monitor on its own
  (per-output pacing, so a 60 Hz monitor never holds back a 144 Hz one). Members of a span
  group are flipped together with one `COMMIT_MULTI`, so every monitor shows the same frame:
  no tear or step between screens.
- **Same refresh, timing synced.** The group runs one mode, and frescod asks the display
  driver to lock the members' timing (`COMMIT_SYNC_GROUP`, display ABI 3.1): on AMD that is the
  DCN's OTG timing synchronisation, the mechanism Eyefinity relies on, so the vblanks line up
  and a flip lands on every monitor in the same refresh. gpusim models it (all OTGs of a group
  share one frame clock). Without it the frames still match, but a flip can land up to a
  refresh apart on different monitors.
- **VRR across a group**: on when every member supports VRR, over the intersection of their
  ranges; one frame period for all. Otherwise off for the group while the span is up.
- **HDR**: the group is HDR only if every member can take it; else SDR.
- **Direct scanout when nothing covers it.** A zoomed game owns its monitors, so frescod
  skips composing the desktop under it and presents the game's surface straight to each
  member, each cropped to its tile. Anything frescod must draw over it (the trusted strip
  when shown, a notification, a credential slot) falls back to compose for that frame.
- **Leaving the span**: un-zoom, a member unplugged (§6), or the user's escape (§7) ends it;
  members go back to per-output pacing and their own modes and VRR.

## 6. Monitors coming and going

A span is more fragile than a desktop: a resize in the middle of a race is worse than a
window moving, and every member shares one timing lock and one flip. So a span holds its
shape through anything short, and changes shape only for a monitor that is really gone.
The desktop rules (display 3.0) stay underneath: hotplug signals settle for 500 ms
(`outputs::SETTLE`) before anything is reconfigured, and a monitor missing for under 5 s
(`GRACE`) keeps its place.

| Case | What the game sees | What frescod does |
|---|---|---|
| **HPD glitch** (a pulse, a link retrain, a burst of hotplug interrupts that settles within 500 ms) | Nothing. | Nothing: the debounce absorbs it. If the link dropped, the member's link is retrained in place; its pipe never stopped. |
| **Short dropout** (monitor power-cycles, input switch, cable wiggle, under 30 s) | No resize. `EV_WINDOW_SPAN` marks that tile `present: false` and then `true` again, so a game may pause drawing it; one that ignores this loses nothing. | The member's tile isn't scanned out. Its **pipe keeps running** (OTG timing on, stream off at the link), so the timing lock and the other members' flips are untouched, and the member's VRR and HDR capabilities still count for the group. Group flips leave it out of the commit while it is gone. On return: same monitor identity, so the link is trained and the group mode set on that member alone; the others don't blank. |
| **Accidental disconnect** (a cable pulled and pushed back) | Same as a short dropout, if it is back within 30 s. | Same. A *different* monitor plugged into that connector is not a return (identity differs): it gets the desktop rules, not the span. |
| **Gone** (more than 30 s, longer than the desktop's 5 s because a mid-game resize costs more than a moved window) | One resize to the members left, with `EV_WINDOW_SPAN`. One monitor left: an ordinary zoom. | Pipe released, timing group shrunk, group VRR/HDR recomputed. Coming back later grows the span again, also as one resize. |
| **Flapping** (a member drops 3 times within 60 s: a failing cable, a monitor with a bad power-save, a dying link) | The tile stays `present: false` at the same size: no resizes, however often it flaps. A notification names the monitor. | The member is held out: its returns are ignored until it has stayed up for 10 s, then it rejoins once. Each flap costs nothing but that tile. |
| **Returns in another mode** (some monitors forget their mode on power-up) | Nothing more than for a short dropout. | frescod sets the group's mode again; if the monitor no longer offers it, the member stays out and the user is told. |
| **The timing master goes** | Nothing. | The lock's master is a pipe, not a monitor, and pipes keep running through dropouts. If a member is released for good, a remaining pipe takes over as master at the next vblank (gpusim models the hand-over; silicon check with D5). |

Throughout, a gone member never holds up a flip: group flips commit only the present members,
and a member's flip-done fence is not waited on while it is out. The S4/S5 tests cover each row
(sim_plug unplug/replug inside and past 30 s, a burst of HPD pulses, a flapping script, and a
swapped monitor).

## 7. Security and control

- Only Forum makes span groups and zooms windows, so an app can't cover monitors on its own.
- Covering several monitors doesn't change what zoom already allows: the game covers chrome
  on those monitors, Super+F (and the system escape) still un-zooms, and frescod's strip
  still shows on focus changes. A notification still appears in its band, on the primary
  member.
- `EV_WINDOW_SPAN` tells the owner only its own window's layout: monitor positions inside
  the window and bezel sizes, which reveal little beyond what `DISPLAY_INFO` already gives.
- Locked timing and one flip per group reuse the display lease (only frescod holds the
  display), so no new privilege.

## 8. Work, in order

| # | Work | Where | Test |
|---|---|---|---|
| S1 | Display ABI 3.1: `COMMIT_SYNC_GROUP` (lock OTG timing of a set of outputs that share a mode), and a group flip as one `COMMIT_MULTI` | kmod, gpusim (shared frame clock for a sync group) | `display_span`: flips of a group land in the same vblank on every member, 100 of 100 |
| S2 | frescod span groups: group pacing, one commit per frame, VRR/HDR rules, direct scanout cropped per member | frescod | VM: frame sequence equal on all members at every flip; screen dumps per head |
| S3 | Protocol: `OP_WM_SPAN_GROUP` (Forum defines groups and bezels), `EV_WINDOW_SPAN` to the zoomed window's owner; fresco-client event | fresco-protocol, frescod, fresco-client | unit + probe |
| S4 | Forum: span groups remembered per monitor set, zoom over a group, per-app opt-out, `forum-ctl span`, bezel widths | forum-wm, forum-ctl | VM: zoom over 3 heads, unplug the middle one, plug it back |
| S5 | A span probe: a test game renders one view per tile with bezel lines, checks continuity across members on scanout; chaos run with span toggles and hotplugs | atrium-test-client, scripts | probe in both modes; chaos 60 steps, 0 failures |

S1 and S2 are the hard parts; S3 to S5 follow the patterns multi-monitor already set.

**S1 as built (2026-11-06).** Display ABI 3.1 `COMMIT_SYNC_GROUP`
(atrium-display-architecture.md §7.6): one call sets a group's members and
which of them are flip-locked, so the §6 dropout is the same call with that
member's lock off. gpusim models the timing lock and the flip lock with SIM
registers; the DCN programming waits for D5. `display_span` passes in the
amd64 VM: 100 of 100 group flips in one vblank, with no split frame in 7375
snapshots of the three pipes (5361 of them while one member's in-fence was
held back); with a member's lock off the others flip without it, and the
test's detector sees the split.

**S2–S4 as built (2026-11-06).** Forum keeps span groups per set of
monitors (`forum-ctl span A B C [gap PX]`, `spans`, `span-app APP on|off`)
and, when the zoomed window's monitor is in a group, asks frescod for the
span (`OP_WM_SPAN_GROUP`); the reply is the rect the window covers. Bezel
compensation is done by frescod moving the members (and any monitor past
them) apart by the gaps while the span is up, so the window covers the
hidden strips and no head scans them out; they go back when it ends. The
gap is in pixels for now; millimetres from the EDID size come later. frescod
(`frescod/src/span.rs`) locks the members' timing, presents them with one
`COMMIT_MULTI` per frame, runs VRR and HDR on all members or none (VRR off on
all if one lacks it; the kernel runs the highest floor), gives members a
30 s grace, darkens a dropped member's tile with its flip lock off, and holds
out a member that drops 3 times in 60 s until it has stayed up 10 s (logged;
a notification naming it is not posted yet). A member gone past the grace
ends the span and Forum asks again over the members left: one resize.
**Not yet:** direct scanout of the game's surface (members show the composed
desktop, cropped per head, as multi-monitor does), the per-tile probe game
and the span chaos run. `scripts/vm-span-e2e.sh` passes in the amd64 VM with
0 failures, split mode on: three 1080p monitors (one with a 60 Hz VRR floor),
a window covering 5840x1080 with two 40 px strips, a 10 s dropout, flapping
held out and rejoining, a member gone 45 s (one resize to 3880x1080 and back),
and the un-zoom putting the monitors back.

## 9. Decisions for Shankar

1. **Spanning only through zoom over a user-made span group, never a whole-desktop mode.**
   *Recommend yes.* It avoids Eyefinity's "the taskbar is 5760 wide" problem and keeps apps
   from claiming monitors.
2. **Tell the game the tiles (`EV_WINDOW_SPAN`).** *Recommend yes.* It costs one event and
   lets a game get per-screen perspective without several windows, which no other platform
   offers through the system.
3. **v1 requires the same mode on every member, landscape only.** *Recommend yes.* Mixed
   sizes and portrait come later if asked for.
4. **Lock member timing in hardware (ABI 3.1) rather than only flipping together.**
   *Recommend yes.* Without it a flip can land a refresh apart across monitors, which shows as
   a tear between screens in fast motion. It is gpusim-modelled now and silicon-verified with
   D5.
5. **A span holds its shape through dropouts up to 30 s and through flapping**, with the
   tile marked not present; it resizes only for a monitor gone longer (§6). *Recommend yes.*
6. **Bezel compensation off by default**, with widths set once per monitor. *Recommend yes*:
   most people find hidden pixels surprising until they try it.
