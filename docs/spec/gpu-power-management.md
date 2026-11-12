# GPU power management on silicon (atrium-gpu-amd, SMU 14 / RDNA4)

> **Status:** draft 2026-11-12, for Shankar's review. Written after stage 3 of the R9700
> bring-up ([r9700-bare-metal.md](r9700-bare-metal.md) §4.3, §5), where the driver first talked
> to the SMU. Gates stage 4: no GPU workload runs on silicon until §6 P1 (telemetry) is in.

## 1. Who controls what

The work splits three ways. The dangerous parts are not in the driver.

| Layer | Owns | Can the driver change it? |
|---|---|---|
| **Hardware** | Thermal trip / critical temperature shutdown (THM), voltage regulator protections | No |
| **SMU firmware (PMFW)**, AMD-signed, loaded through the PSP | V/F curves, voltage, current and power limits (PPT/TDC), thermal throttling, fan curve, clock DPM, gating | Only through mailbox *requests*, which it clamps |
| **PSP, with SCPM** (secure power management) | The power-play table, the feature mask, the limits' min/max envelope | No. The R9700 runs SCPM: on 2026-11-12 the SMU refused SetAllowedFeaturesMask with 0xFD |
| **Driver (us)** | Enabling features at start, teardown, GfxOff policy, requests inside the envelope (power cap, clock floors/ceilings, workload profile), telemetry, reacting to thermal events | Yes. That is this spec |

So nothing the driver sends can raise a limit past the board's signed table, and the
hardware trips regardless. What the driver can still get wrong:
- leave the card in a wasteful state, e.g. GfxOff disallowed at idle (we do today after a
  bring-up run);
- skip the orderly teardown the PMFW expects;
- fly blind: not see temperature, power or throttling;
- disable a protection feature or take manual fan control. Under SCPM some of this may still
  be accepted, so it is a hard rule below.

Interface sources: Linux's MIT-licensed `smu_cmn.c`, `smu_v14_0.c`, `smu_v14_0_2_ppt.c`,
`smu_v14_0_2_ppsmc.h`, `smu14_driver_if_v14_0.h`. Chosen by MP1's discovered version (14.x),
never by device id.

## 2. Hard rules (enforced in code)

1. **Message allowlist.** The driver sends only the messages in §3, through one function
   that takes an enum of allowed messages. No raw message ids exist outside that table, so
   anything else cannot be expressed. Adding a message is a reviewed spec change.
2. **Never sent:**
   - DisableAllSmuFeatures, DisableSmuFeatures*, EnableSmuFeatures* (piecemeal);
   - any OverDrive table write, TransferTableDram2Smu of PPTABLE / OVERDRIVE /
     CUSTOM_SKUTABLE;
   - manual fan (PWM/RPM) set;
   - voltage offsets;
   - SetHardMinByFreq above the default.

   Throttlers, FW_CTF, FAN_CONTROL, VR0HOT and THROTTLERS stay as the PMFW enabled them. After
   enable, the driver checks those feature bits (§4) and refuses to bring the GPU up if one is
   off.
3. **Requests only downward at first.** Power cap: only ≤ the default, ≥ the SMU-reported
   minimum. Clock limits: soft max only, never above default; no hard minimum. Later raises,
   for posture "performance", stay within the SMU-reported max and need a spec change.
4. **Every response is checked.**
   - Pre-poll and post-poll have a timeout (2 s).
   - 0x01 is OK.
   - 0xFD (bad prerequisite) is handled per message where expected: SetAllowedFeaturesMask
     under SCPM. Anywhere else it is an error.
   - 0xFE, 0xFF or a timeout put the SMU link in **failed** state. No further messages are
     sent, the PMFW is left in its own automatic control, the event is logged, and the GPU is
     marked device-lost (the reset path decides recovery).
   - The driver never retries in a loop.
5. **One sender.** All SMU traffic goes through one serialised path (a mutex, sleepable
   context), never from an interrupt handler.
6. **Leave a state the PMFW would choose itself.** On detach, unload, reset prepare or
   shutdown:
   1. undo every request we made (cap and clocks back to default, GfxOff allowed);
   2. send PrepareMp1ForUnload.
7. **Read-only towards the rest of Atrium.** Posture and energy cap come in through the
   federation interface ([atrium-power-posture.md](atrium-power-posture.md),
   [energy-policy.md](energy-policy.md)). The driver maps them to the requests in §5.
   Telemetry goes out read-only. Nothing in the scheduler talks to the SMU, and SMU state is
   never fed back into the scheduler's control loop.

## 3. Messages (SMU 14.0.2/3 ids)

| Phase | Message (id) | Notes |
|---|---|---|
| Start | TestMessage (0x01), GetSmuVersion (0x02), GetDriverIfVersion (0x03) | log versions; driver-if is used for the metrics layout (§4) |
| Start | SetDriverDramAddrHigh/Low (0x0E/0x0F) | 1 MiB driver table in VRAM, inside the driver's reserved area |
| Start | RunDcBtc (0x36) | as amdgpu |
| Start | SetAllowedFeaturesMaskHigh/Low (0x05/0x04), all ones | 0xFD under SCPM is expected and skipped |
| Start | EnableAllSmuFeatures (0x06) | |
| Start | GetRunningSmuFeaturesLow/High (0x0C/0x0D) | checked against the required set (§4) |
| Run | TransferTableSmu2Dram (0x12), arg TABLE_SMU_METRICS (5) | telemetry, §4 |
| Run | AllowGfxOff (0x28) / DisallowGfxOff (0x29) | §5 |
| Run | SetPptLimit (0x32) / GetPptLimit (0x33) | cap, only downward (rule 3) |
| Run | SetSoftMaxByFreq (0x1A), GetMinDpmFreq / GetMaxDpmFreq (0x1D/0x1E) | posture powersave only |
| Run | SetWorkloadMask (0x24) | profile per posture; default "3D fullscreen" / "compute" as amdgpu |
| Stop | PrepareMp1ForUnload (0x2E) | last message before detach / reset / shutdown |

## 4. Telemetry (P1, before any workload)

- **Required features after enable.** The running-features mask must have these bits:
  - THROTTLERS (23)
  - FW_CTF (28)
  - FAN_CONTROL (29)
  - VR0HOT (27)
  - FW_DSTATE (17)
  - MEM_TEMP_READ (46)

  If any is missing, the driver logs it and refuses to attach. The R9700 on 2026-11-12
  reported 0x0488f19e38fffcfb, and all six are set.
- **Metrics.** TransferTableSmu2Dram(TABLE_SMU_METRICS) into the driver table, then a read
  through BAR0. Fields:
  - AvgTemperature[] (edge, hotspot, memory, VRs)
  - AverageSocketPower, AverageTotalBoardPower
  - AvgFanRpm / AvgFanPwm
  - CurrClock[] and the average gfx/mem clocks
  - ThrottlingPercentage[]
  - MetricsCounter

  **Layout risk:** the PMFW reports driver interface 0x33, but the newest Linux header we
  have (`smu14_driver_if_v14_0.h`) is 0x2E. The table is parsed by a Rust module with the
  offsets for each interface version it knows. Values are checked for plausibility:
  - temperatures 0–125 °C
  - MetricsCounter advancing between reads
  - fan RPM ≤ 10 000

  An unknown version or an implausible value reports "telemetry unavailable" and is never
  guessed. Until the 0x33 layout is confirmed, the driver also reads the edge temperature
  from the THM block's current-temperature register as a cross-check.
- **Exposed** as sysctls `dev.atrium_gpu.0.power.*`: temperatures, power, fan, clocks,
  throttle reasons, features, versions. The federation member `gpu0` publishes power and
  residency read-only (energy-policy §Shared).
- **Polling.** Every 1 s while busy, 5 s idle, from a taskqueue, never from the ISR.

## 5. Policy

| Posture (power-posture spec) | GfxOff | Workload mask | Cap | Soft max gfxclk |
|---|---|---|---|---|
| 0 powersave | allowed | power saving | min(default, budget grant) | 70 % of default max |
| 5 balanced (default) | allowed | 3D / compute | min(default, budget grant) | default |
| 10 performance | allowed (held off only while work is queued) | 3D / compute | min(default, grant) | default |

- **Federation.** `actuate(grant, P)` maps as the table; `demand(P)` comes from the metrics
  table's power reading and the roofline profile. On gpusim the same levers are driver-owned
  gating (`powergate.rs`). The posture spec's deadline-aware pre-wake stays on at every
  posture: with GfxOff allowed it is what hides the power-up latency.
- **GfxOff is allowed by default.** The driver disallows it only around register sequences
  that need GC powered (bring-up, reset, MES queue map), as a scoped guard that always
  re-allows. The bring-up test hook leaving it disallowed is a known gap, closed in P2.
- **Energy cap.** The budget grant from `water_fill` becomes SetPptLimit. It is clamped to
  [SMU min, default] and changed at most once a second, with hysteresis.
- **Thermal events.** Once the IH ring is up (stage 4), the driver hooks THM DIG_THERM L2H/H2L
  and the SMU's interrupt source. On L2H or a throttle event it logs, raises a kqueue note on
  the ctl node, and drops its own requests to default. It never fights the PMFW's throttling.

## 6. Phases

- **P1 telemetry (gates stage 4).**
  - The allowlisted sender: `smu.c` reworked, message enum, failed-link state.
  - Required-features check.
  - Metrics read (Rust parser, plausibility checks, THM cross-check).
  - Sysctls.
  - Box test: idle baseline, then the same values under the first dispatches in stage 4.
- **P2 lifecycle.**
  - GfxOff allowed after bring-up, plus the scoped guard.
  - Detach / unload / reset sequence (rule 6).
  - Box test: load, unload, load again, with the features mask and metrics unchanged.
- **P3 thermal interrupts** (after IH in stage 4).
- **P4 posture and cap** (federation member `gpu0` on silicon). Box test: a cap step-down
  shows in AverageSocketPower within 2 s.

Every box run reads the metrics before and after, and its log goes in
[r9700-bare-metal.md](r9700-bare-metal.md) §5.

## 7. Non-goals

- Overclocking, undervolting, overdrive and fan curves (rule 2).
- Driving power for VCN/JPEG/DCN beyond what EnableAllSmuFeatures does: those features
  power on demand under the PMFW.
- BACO / runtime D3: later, a separate spec section once suspend exists.
