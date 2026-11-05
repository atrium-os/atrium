# Multi-monitor support: design

2026-11-05. Thread "Build the Fresco display pipeline". Builds on display
ABI 2.1–2.3 (modes, EDID, hotplug, VRR, HDR) and spec §8 (output topology).

## Summary

Today the display is one connector driven by one CRTC, and frescod, Forum and
the ABI all assume a single screen. This plan makes every layer multi-output
and attacks, one by one, the causes that make multi-monitor flaky on other
systems. The core choices:

- **One display node and atomic commits across all outputs.** A layout change
  across three monitors is applied as a whole or not at all, after a dry run.
  No half-applied states and no flicker from a sequence of per-monitor mode
  sets.
- **Monitors are identified by what they are, not where they are plugged.**
  The identity comes from the EDID (maker, product, serial), with the
  connector path only as a tie-break for identical monitors. Layouts are
  remembered per *set* of monitors and restored exactly when that set comes
  back.
- **Each monitor runs on its own clock.** Frame pacing, damage, VRR and HDR
  are per output; a 60 Hz monitor never holds back a 144 Hz one.
- **Hotplug is debounced, and windows are never lost.** A monitor that drops
  out for a moment (a DP monitor going to sleep, a cable wiggle) does not
  reshuffle the desktop. A window on a monitor that really leaves moves to a
  remaining one and goes back when the monitor returns.
- **A chaos test enforces it.** Random plug, unplug, sleep and mode-change
  storms in gpusim, with invariants checked after every step.

Decision needed (card in the thread): the kernel ABI shape. Recommended: one
node, atomic multi-output commit (KMS-like objects, without the legacy
per-object calls). The alternative, one node per output, is simpler, but
it can't make a cross-monitor change atomic.

## Why multi-monitor is flaky elsewhere, and our answer

| # | Failure seen elsewhere | Cause | Our answer |
|---|---|---|---|
| 1 | Monitors swap places, layout comes back wrong after sleep or reboot | Monitors are keyed by enumeration order or port, which changes (MST ports reorder, docks re-enumerate) | Identity = EDID maker/product/serial (+ connector path for identical twins). Layouts keyed by the set of identities present |
| 2 | Flicker, black screens, windows jumping twice on plug | Each monitor is mode-set separately; the compositor relays out after every step | One atomic commit for the whole new configuration, checked first with TEST_ONLY; Forum relays out once |
| 3 | A plug storm (dock, KVM, DP sleep) leaves a broken state | Every HPD edge triggers a full reconfiguration | Debounce: hotplug changes settle for 500 ms before acting; a monitor gone for under 5 s (its sleep) keeps its place |
| 4 | Windows stranded off-screen or piled on one monitor; not restored on replug | No per-window memory of which monitor it lived on | Each window remembers its monitor identity and relative position per display set: rescued on unplug, put back on replug |
| 5 | Stutter on the faster monitor with mixed refresh rates | One global frame clock (X11, early Wayland compositors) | Per-output vblank, damage and commit; VRR and HDR per output |
| 6 | Blurry or wrongly-sized windows across monitors with different DPI | One global scale | Per-monitor scale; a window renders at the scale of the monitor it is mostly on; layout in logical points |
| 7 | The third monitor silently stays black | Hardware limits (CRTCs, PHYs, link bandwidth) surface as a failed mode set | TEST_ONLY returns a reason (no free CRTC, exceeds link, MST oversubscribed); Forum picks the next-best mode deterministically and says why |
| 8 | Cursor stuck at an edge, unreachable regions | Layout gaps and mismatched edge sizes | The layout validator requires monitors to touch; edges map proportionally |
| 9 | New windows open on the wrong monitor | No policy | New windows open on the monitor with the pointer (Forum policy, configurable) |
| 10 | One misbehaving monitor (bad EDID, link drop) takes down all outputs | Shared failure handling | Faults are per output: a failed link is reported on that output only; the others keep running |

## Architecture

### Kernel ABI (display 3.0 on the same node)

- **Objects.** CONNECTORS lists every connector (physical and MST child) with
  type, state, EDID, modes, VRR range, HDR caps and a stable identity (a
  64-bit hash of EDID maker, product and serial, plus the connector path).
  CRTCS lists the scanout pipes and which connectors each can drive.
- **Atomic commit.** One COMMIT carries an array of per-output states (CRTC →
  connector, mode, buffer, VRR, HDR, or off). TEST_ONLY validates the whole
  set and returns the first infeasible output and why. Mode sets and flips on
  several outputs apply together; a flip-only commit can touch one output.
- **Events.** kqueue vblank events carry the CRTC; hotplug carries a
  topology generation. Fences per output.
- **Driver side.** `atrium_display_hw_ops` gains a CRTC index on the timing,
  VRR, infoframe and surface hooks, plus `route(crtc, connector)` for the
  crossbar. The shared code does the assignment and the bandwidth check.
- 2.x callers keep working: the 2.x ioctls address output 0.

### gpusim

- DCN 4 has four pipes: model HUBP0–3, OTG0–3, per-pipe vblank and flip
  interrupts (their real IH source ids).
- Three physical connectors (HDMI, DP, USB-C) with their own HPD, plus the
  existing MST hub model for daisy chains; a link bandwidth budget per DP
  link.
- `sim_plug` takes a connector; a new `sim_sleep` drops HPD briefly, as a DP
  monitor does on standby.

### frescod

- One logical desktop in points; each output is a viewport with its own
  scale, scanout buffers, pacing, VRR and HDR.
- Composition per output, only for its damage.
- Hotplug handling: debounce, build the whole new configuration, TEST_ONLY,
  commit atomically, then send one `EV_DISPLAY_CHANGED` with every output.

### Forum (window manager policy)

- Display-set memory: arrangement, primary, scale and mode per set of
  monitor identities, saved per user.
- Window rescue and restore by monitor identity.
- New-window placement on the pointer's monitor; fullscreen per monitor.
- `forum-ctl outputs`, `forum-ctl arrange A right-of B`, `forum-ctl mirror`
  (mirroring is a later phase, below).

## Phases and tests

1. **gpusim: four pipes, three connectors.** Engine tests per pipe; per-pipe
   interrupts. (Needed whatever ABI is chosen.)
2. **Kernel: display 3.0.** CONNECTORS / CRTCS, atomic multi-output commit
   with TEST_ONLY reasons, per-CRTC events, stable identity. Test
   `display_multi`: light 3 monitors in one commit; flip each at its own
   rate; refuse a 4th output beyond the bandwidth with a reason; hotplug one
   while the others keep flipping.
3. **frescod: multi-output desktop.** Per-output composition and pacing,
   debounced atomic reconfiguration.
4. **Forum: policy.** Display sets, rescue and restore, per-monitor scale,
   placement, forum-ctl.
5. **Chaos test.** Random storms of plug, unplug, sleep, mode change and MST
   chain changes. After each step: no window off-screen, every output lit or
   reported with a reason, a known display set restored exactly, no flip lost,
   no half-applied state.
6. **Later.** Mirroring (one CRTC per output with the same content, or a
   shared buffer when timings match), per-monitor colour profiles.
