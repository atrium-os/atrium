# Atrium unified power posture — one knob across CPU / GPU / display

> **Status:** design, 2026-06-22 (for review). Unifies the CPU power-posture knob
> (`kern.sched.laminar.power_policy`) and the cross-device energy cap
> (`kern.sched.energy_cap_mw` + `water_fill`) into one coherent control across the
> three federated members (`cpu`, `gpu0`, `display0`). Builds on
> [`atrium-scheduler-federation.md`](atrium-scheduler-federation.md) and
> [`energy-policy.md`](energy-policy.md).

## 1. The problem

Two power knobs exist and do not compose:

- **`power_policy` (0..10, powersave/balanced/performance)** maps to a table that
  shapes the **CPU** parking + DVFS controllers. CPU-scoped.
- **`energy_cap_mw`** is a hard power ceiling that `water_fill` splits across the
  three energy-federation members (each declares demand, obeys a budget grant).

So "performance mode" today means only "CPU pinned high, don't park" — it does not
tell the GPU "don't GFXOFF, hold clocks" or the display "disable PSR, max refresh".
The GPU/display follow only the cap + their own per-device policies. We want one
posture that drives all three coherently, *without* a second control path fighting
the cap.

## 2. Model: two orthogonal axes that meet at one loop

The insight that makes them compose rather than conflict — they are different kinds
of thing:

- **Cap = the hard ceiling.** How much power *exists* (thermal / battery / PSU).
  Inviolable; `water_fill` divides it by priority weight.
- **Posture = how eagerly a device converts *available* headroom into performance.**
  A soft preference. *Performance* = spend all you're granted on speed (max clocks,
  no gating, max refresh). *Powersave* = leave granted budget unspent (gate,
  downclock, PSR) to save energy even when budget is available.

Orthogonal: the cap is *how much power is available*; the posture is *how willing
you are to spend it*. Performance under a tight cap still obeys the cap — it just
spends all it is *granted* on speed.

## 3. The unified control loop (one federation tick)

Today: `poll demand[i] → water_fill(cap) → grant[i] → budget_fn[i]`. The change
threads posture through the **same** loop — no second path:

```
posture P = power_policy                       # one system-wide value (§5)
for each member i:
    demand[i] = member.demand(P)               # what it wants AT posture P
grant = water_fill(cap, demand, WEIGHTS)       # cap clamps; WEIGHTS = priority only
for each member i:
    member.actuate(grant[i], P)                # seek the P-target, clamped to grant
```

Posture shapes the **demand** (input to `water_fill`) and the **actuation** (how the
grant is spent); the cap clamps. When `Σdemand ≤ cap` the posture is fully expressed
(everyone gets their target); when the cap binds, `water_fill` throttles by weight
and the posture-shaped relative demand still sets the split proportions.

### Decisions locked (review 2026-06-22)

- **D1 — posture shapes demand + actuation only; `water_fill` weights stay pure
  priority.** Posture and priority are kept orthogonal: performance posture makes
  *everyone* demand more (so the cap binds sooner and throttles fairly), but it does
  **not** re-weight who wins the split. Simpler and predictable; no conflation of
  "energy/perf tradeoff" with "who matters under contention".
- **D2 — system-wide posture only, first.** One `power_policy` drives all three.
  Per-device / per-jail override (e.g. "GPU performance, CPU powersave" for a compute
  job) is a clean later refinement once the unified path is proven (§8).

## 4. Per-device posture → levers

Each device gets a posture table (0..10), like the CPU's today. `demand(P)` reads it
to size its ask; `actuate(grant, P)` applies the levers, clamped to the grant.

| Posture | CPU *(exists)* | GPU *(driver-owned gating, `powergate.rs`)* | Display *(`display.rs` PSR/VRR)* |
|---|---|---|---|
| **0 powersave** | low ctrl/DVFS headroom, eager parking | deep+early gating (gfxoff→d3cold, short idle thresholds), low DVFS headroom | PSR after 2 idle frames, VRR floor low |
| **5 balanced** | today's defaults | gfxoff at moderate idle, mid DVFS headroom | PSR after ~N frames, mid VRR |
| **10 performance** | freq pinned max, parking blocked | shallow/no gating, clocks held high | PSR off (high idle threshold), VRR pinned high |

The GPU's **deadline-aware pre-wake stays on at every posture** — it hides gating
exit latency for free, so it is not a posture lever.

## 5. Plumbing

- Posture is one system value (`power_policy` sysctl, already present; its writer
  `laminar_apply_power_policy` becomes the broadcast trigger).
- Broadcast to members: extend the energy-member interface from
  `budget_fn(arg, grant)` to **`actuate_fn(arg, grant, posture)`** — the federation
  tick already iterates members, so it passes both.
  - **CPU** member: maps posture → the existing `laminar_*` knobs (fold the current
    `laminar_apply_power_policy` into the actuator so there is one path).
  - **GPU** member (kmod): the kernel writes a posture to a new
    `regSCHED_POWER_POSTURE` register alongside the existing
    `regSCHED_POWER_BUDGET_MW`; the driver maps posture → `GatePolicy`
    aggressiveness + DVFS headroom.
  - **Display** member (kmod): the kernel writes `regDISP_POSTURE`; the display maps
    posture → `PSR_IDLE_FRAMES` + VRR floor.

## 6. Coherence invariants

1. **Cap hard, posture soft.** A member never exceeds its grant regardless of
   posture; performance + tight cap → cap wins.
2. **No double-control.** Each device has exactly one controller seeking
   `min(posture_target, grant)` — posture is the set-point, grant is the ceiling.
   (Today `power_policy` sets CPU knobs *and* the cap clamps via a DVFS ceiling;
   these are unified here into the single actuator so they cannot diverge.)
3. **Posture global, weight per-entity.** Posture = the system's energy/perf
   tradeoff (one value); weight = priority under contention (per member/entity).
   Never conflated (D1).
4. **Cap = 0 (federation off) keeps posture live.** With no cap, `grant = ∞`, so
   `actuate(∞, P)` = the pure posture target — the posture still tunes all three.

## 7. How posture and cap read together (worked cases)

- **Slack (idle desktop), powersave:** demand low, cap not binding → grants ample →
  but each device *under-spends* (CPU parks, GPU gfxoff, display PSR). Min energy.
- **Slack, performance:** demand high, cap not binding → grants ample, fully spent →
  CPU pinned, GPU no-gate, display no-PSR. Max responsiveness, higher idle draw.
- **Binding cap (thermal), performance:** everyone demands max → `water_fill`
  throttles by weight → each device spends its (reduced) grant on speed; the cap is
  still honored. Posture didn't change *who* got throttled (D1), only that all asked
  for more.
- **Binding cap, powersave:** demand already modest → cap may not even bind →
  lowest power for the work.

## 8. Implementation phases (when approved)

1. **Engine tier (gpusim, deterministic):** posture tables for GPU (`powergate.rs`
   gating thresholds + DVFS headroom) and display (`display.rs` PSR/VRR); unit tests
   that posture moves the operating point and the cap still clamps.
2. **Federation interface:** `actuate_fn(arg, grant, posture)`; fold the CPU
   `power_policy` application into it (invariant #2).
3. **Driver ABI:** `regSCHED_POWER_POSTURE` + `regDISP_POSTURE`; the kmod energy
   members map posture → their levers.
4. **Verify:** the four §7 cases — posture moves all three devices' operating points
   under slack; the cap clamps under pressure; `power_policy` by name drives the GPU
   gating + display PSR observably (engine tier deterministic; live tier as a
   does-it-move check, per the two-tier design).

## 9. Deferred (not in scope of the first cut)

- **Per-device / per-jail posture override** (D2) — a foreground game jail at
  performance while the system sits at balanced; the per-jail manifest could carry a
  posture, federated under the same cap.
- **Auto-posture** (battery state, AC/DC, thermal trend) driving `power_policy`
  automatically — a policy daemon, not kernel mechanism.
