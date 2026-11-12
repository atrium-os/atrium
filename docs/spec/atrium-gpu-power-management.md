# Atrium GPU power management on real AMD silicon — the safety contract

> **Status:** contract, 2026-11-12. Nothing in it is implemented beyond what
> stage 3d already does (`smu.c`, 3333d96c). It binds every change to the
> AMD driver's SMU path from here on. Companion to
> [`atrium-power-posture.md`](atrium-power-posture.md) (posture + cap),
> [`energy-policy.md`](energy-policy.md) and
> [`r9700-bare-metal.md`](r9700-bare-metal.md) §4.3 (SMU bring-up log).

## 1. Who controls what

On RDNA4 dGPUs the power-management firmware (PMFW) on the SMU (MP1) is the
controller. It owns the voltage/frequency curves, the current and power limits,
thermal throttling, fan control and the over-temperature shutdown. The
hardware thermal trips (THM/OTP) act without any driver. The driver never
touches a regulator or a clock directly; it sends **requests** through the
MP1 mailbox (`C2PMSG_66` msg, `C2PMSG_82` arg, `C2PMSG_90` response).

The R9700 runs with **SCPM** (secure power management): the power-play
table, the voltage and power limits and the feature set are signed and
enforced by the PSP. We saw it on silicon on 2026-11-12: the SMU refused
`SetAllowedFeaturesMask` with `0xfd` (prerequisite not met), because the PSP
owns that mask. Under SCPM a driver bug cannot raise a limit beyond the
board's signed table or upload a PPTable.

What the driver decides is **policy inside that envelope**:

| Lever | Driver decides | Bounded by |
|---|---|---|
| Feature enable | `EnableAllSmuFeatures` once at init | the PSP's signed allowed mask |
| GfxOff | allow / disallow | PMFW |
| Power cap (PPT) | value | the min/max the SMU reports from the signed table |
| Clock floor / ceiling | soft min/max per domain | the signed DPM table |
| Workload profile | which profile | PMFW's profile set |
| Fan | **nothing: always PMFW auto** | — |
| Overdrive (OD) | **nothing: never used** | — |
| Driver table | its contents in our VRAM | read by PMFW |

## 2. Rules

**R1. Thermal protection is never switched off.** The driver must not
disable the thermal, throttler, fan-control or OTP features, never selects a
manual fan mode, and never sends an overdrive table. These are the realistic
ways a driver damages a card; all of them are removed, not configured.

**R2. An explicit allow-list of SMU messages.** `smu.c` may send only the
messages in one table (today: TestMessage, GetSmuVersion,
GetDriverIfVersion, SetDriverDramAddrHigh/Low, RunDcBtc,
GetRunningSmuFeaturesLow/High, SetAllowedFeaturesMaskHigh/Low,
EnableAllSmuFeatures, DisallowGfxOff/AllowGfxOff, plus the table-transfer
and limit messages as they are added). Sending a message that is not in the
table is a compile-time error. Each new entry is added in its own commit
with the reason.

**R3. Limits move only inside the SMU's own range.** A power cap is set
only after reading the SMU's min/max/default for it. In the first version
the driver may only lower the cap from the default, never raise it. Clock
ceilings follow the same rule.

**R4. Every SMU response is untrusted.** Every message has a bounded wait.
On `0xfe` (unknown), `0xff` (failed), a timeout, or a `0xfd` that is not a
documented expected refusal (as SCPM's allowed mask is), the driver stops
sending SMU messages for that attach and leaves PMFW in automatic. No
retries in a loop. The failure is logged with the message, argument and
response.

**R5. Observe before controlling.** The first SMU feature after bring-up is
read-only: pull the metrics table (temperatures edge/hotspot/memory, power,
fan RPM, clocks, throttle status) through the driver table, idle and under
load, and log it. Every later policy change is an A/B against those numbers
with the mechanism counter checked in both arms.

**R6. Detach leaves a state PMFW would choose itself.** On detach or
unload the driver re-allows GfxOff, clears any cap or clock limit it set,
and restores the default workload profile. Today (3333d96c) the unloaded
driver leaves GfxOff disallowed, so GFX stays powered at idle until a power
cycle; that is the first R6 violation to fix.

**R7. Thermal events are interrupts.** Once the IH ring is up, the driver
takes the SMU and THM interrupt sources. On a throttle or critical event it
logs it and drops its own requests back to defaults. It does not poll
temperatures to make safety decisions.

**R8. One-way coupling to the energy policy.** The GPU member of the
energy federation reads the posture and its `water_fill` grant read-only
and maps them to SMU levers (§3). Nothing outside the AMD driver sends SMU
messages, and SMU state is reported to the federation as demand, never fed
into another device's control loop.

## 3. Mapping posture and cap onto PMFW

[`atrium-power-posture.md`](atrium-power-posture.md) gives each member
`demand(P)` and `actuate(grant, P)`. On gpusim the GPU levers are
driver-owned gating (`powergate.rs`). On real AMD silicon they become SMU
requests:

| Federation input | SMU lever |
|---|---|
| grant (mW, hard) | PPT power cap, clamped to the SMU's [min, default] (R3) |
| posture 0 powersave | GfxOff allowed, power-saving workload profile, lower soft max gfxclk |
| posture 5 balanced | GfxOff allowed, default profile, no clock limits |
| posture 10 performance | GfxOff disallowed while work is queued, compute/3D profile |
| demand(P) | from the metrics table's power reading and the roofline profile |

The deadline-aware pre-wake from the posture spec stays on at every
posture: with GfxOff allowed it is what hides the power-up latency.

## 4. Order of work

1. Metrics table read-only (R5), idle baseline with GFX up.
2. Allow-list table in `smu.c` (R2) and the failure policy (R4).
3. Detach restores defaults (R6).
4. IH-driven thermal events (R7).
5. Power cap and posture mapping (R3, §3), each A/B'd against step 1's
   baseline.
