# GPU power management on silicon

> **Status:** draft 2026-11-12, for Shankar's review. Written after stage 3 of the R9700
> bring-up ([r9700-bare-metal.md](r9700-bare-metal.md) §4.3, §5), where the driver first talked
> to the SMU. Rev. 2 (same day): split into a vendor-neutral contract (Part A) and per-vendor
> backends (Part B, AMD first), after the 2026-11-12 incident (§A6). Gates stage 4: no GPU
> workload runs on silicon until P1 (telemetry) and P2 (lifecycle) are in.

---

## Part A: the vendor-neutral contract

### A1. Who controls what

Every modern discrete GPU has a power-management controller running its own signed firmware:
AMD's SMU (PMFW), Intel's GuC SLPC plus pcode, NVIDIA's GSP, Apple's own. The driver only
sends it requests. The levers are similar everywhere; the messages, tables and signing rules
are not.

| Layer | Owns | Can the driver change it? |
|---|---|---|
| **Hardware** | Thermal trip / critical shutdown, voltage-regulator protections | No |
| **PM firmware** | V/F curves, voltage, current and power limits, throttling, fan curve, clock DPM, gating | Only through *requests*, which it clamps |
| **Platform security** (AMD: PSP with SCPM) | The signed limits table and which features exist | No |
| **Driver backend** | Starting the controller, teardown, idle policy, requests inside the envelope, telemetry, reacting to thermal events | Yes. Part B |
| **Driver power layer** (vendor-neutral, `rs/power.rs`) | The rules below, the telemetry record, posture/cap → lever mapping, the idle check | Yes. Part A |

What a driver can still get wrong, on any vendor:
- leave the card in a wasteful or hot state (§A6 is the real example);
- skip the orderly teardown the firmware expects;
- fly blind: not see temperature, power or throttling;
- disable a protection or take manual fan or voltage control.

### A2. Levers (what a backend offers)

Four, all requests to the firmware, all optional per backend:

| Lever | Neutral form | Range |
|---|---|---|
| Power cap | watts | [firmware min, firmware default] |
| Idle policy | `Allowed` / `HeldWhileQueued` | default `Allowed` |
| Gfx clock ceiling | MHz (soft max) | ≤ firmware default max |
| Workload profile | `PowerSaving` / `Default` | |

`power::plan(posture, grant, limits)` maps posture and the energy grant to levers, clamped so
it can only lower a limit (tested for every posture and grant). Raising anything past the
firmware default (posture "performance" headroom) needs a spec change.

### A3. Hard rules (every backend proves these)

1. **Message allowlist.** A backend sends only the requests its Part B table lists, through one
   function that takes an enum. No raw ids exist outside the table. Adding one is a reviewed
   spec change.
2. **Protections stay as the firmware set them.** Never disable a thermal, current, VR or fan
   protection; never manual fan, voltage offsets, overdrive, or a limits-table write. After
   start the backend checks that its required protections are running and refuses the GPU if
   one is not.
3. **Requests only downward** (A2).
4. **Every response checked.** Timeouts on every wait. An unexpected answer or a timeout puts
   the link in **failed** state: nothing more is sent, the firmware stays in its own control,
   the GPU is marked device-lost. No retry loops.
5. **One sender**, serialised, sleepable context, never from an interrupt handler.
6. **Leave the card as the firmware would have it.** On detach, unload, reset prepare or
   shutdown: restore the defaults (`power::defaults`), let the card idle, then the firmware's
   own unload handshake.
7. **The card must be able to idle.** The driver never powers up a block it cannot bring back
   to idle itself. A bring-up hook that would is behind a separate opt-in
   (`hw.atrium_gpu.gc_up_ok`), and every box run checks idle telemetry afterwards (§A6).
8. **Read-only towards the rest of Atrium.** Posture and cap come in through the federation
   ([atrium-power-posture.md](atrium-power-posture.md), [energy-policy.md](energy-policy.md));
   telemetry goes out read-only. Firmware state is never fed back into the scheduler's control
   loop.
9. **The idle sequence is per family, and proven by telemetry, not assumed.** What takes a
   block from powered to idle differs by vendor and family. On GC 12.0.x it is clock gating
   then GfxOff; another family may need power gating first, a CP running, or a different
   firmware handshake. So:
   - each backend keeps its idle sequence in a table keyed by the block's discovered IP
     version, the same way as the SMU generations;
   - after every power-up of a block, the driver reads telemetry and must see the card idle
     (`power::idle_suspicious` false) within a few seconds;
   - if it doesn't, or there is no sequence for the version, the driver logs it, resets the
     card to its post-POST state and refuses to attach.

   A card the driver cannot prove idle is never left powered.
10. **Unknown means read-only.** A controller generation without a Part B row gets no messages.
   A telemetry layout the backend doesn't know is not decoded (no "probably" readings).

### A4. Telemetry record

One C/Rust record, `struct atrium_gpu_power_telemetry` = `power::Telemetry`:

- temperatures: edge, hotspot, memory, VR (°C)
- package power, board power, the enforced power limit (W)
- fan rpm and %
- gfx busy %, gfx and memory clocks (MHz)
- throttle classes: THERMAL / POWER / CURRENT / OTHER, plus the backend's raw throttler ids
  for logs
- the controller's sample counter (informational)

Missing sensors read `0xffff`. Every sample passes `power::check` before anyone sees it:
- edge temperature is required (1–150 °C);
- hotspot ≥ edge;
- power ≤ 1500 W;
- fan ≤ 10 000 rpm;
- percentages ≤ 100;
- clocks ≤ 10 GHz.

A failing sample is dropped and reported as "telemetry unavailable".

Exposed as sysctls `dev.atrium_gpu.N.power.*`. The federation member `gpuN` publishes power and
residency read-only. Polled every 1 s while busy and every 5 s idle, from a taskqueue.

### A5. Policy

| Posture | Idle | Profile | Cap | Gfx soft max |
|---|---|---|---|---|
| 0–2 powersave | allowed | power saving | min(default, grant) | 70 % of default |
| 3–7 balanced (5 default) | allowed | default | min(default, grant) | default |
| 8–10 performance | held only while work is queued | default | min(default, grant) | default |

- **Cap changes** happen at most once a second, with 5 W hysteresis. A restore to the default
  is never delayed.
- **Idle check.** `power::idle_suspicious`: busy above 20 %, power above 25 % of the limit, or
  power/current throttling, while nothing is queued. The driver logs it and raises a kqueue
  note on the ctl node. It never fights the firmware.
- **Thermal events.** Once interrupts are up, the backend hooks the thermal and firmware
  interrupt sources. On a high-temperature or throttle event it logs, raises a note, and drops
  its own requests to the defaults.

### A6. Incident 2026-11-12: a card left at its limit

After the stage 3c run (IP firmware loaded, RLC booted, SMU features enabled, which powered GC
up), the R9700 sat with no work for about 15 minutes at:
- 299 W of its 300 W limit;
- hotspot 98 °C, edge 56 °C;
- fan 73 %;
- gfx 3.19 GHz at "100 % busy", PPT0 and hotspot throttlers active.

Signs on the host: the PCH rose from 28 to 42 °C with the CPU idle. CP_STAT was 0 and the CP
engines were halted. AllowGfxOff was accepted, but the card stayed in that state. The
firmware's protections held it inside its limits, so there was no damage. It was powered off
from FreeBSD (`shutdown -p`) at 17:02.

Likely cause (unconfirmed): GC was up with the RLC running but none of the driver's gfx init
done. amdgpu at this point runs gfx_v12_0 hw_init: CP resume, clock gating (RLC_CGCG_CGLS_CTRL
CGCG/CGLS), power gating. Without those, the firmware sees gfx as busy and DPM holds it at the
top clock.

What changed:
- rule 7, with `hw.atrium_gpu.gc_up_ok`;
- the idle check;
- P2's box test is now exactly this case.

---

## Part B: backends

A backend is chosen by the discovered controller IP version, never by device id. It supplies:

| Op | Meaning |
|---|---|
| `start` | Bring the controller to running, check the required protections (A3.2), read `Limits` |
| `telemetry` | One `Telemetry` sample, already checked |
| `set_levers` | Carry out `Levers` (only the levers it has) |
| `idle_guard` / `idle_release` | Hold the block powered for a register sequence; always released |
| `stop` | A3.6: defaults, idle, unload handshake |
| `reset` | Put the card back in its post-POST state (AMD: mode 1 reset); used on detach whenever the driver went past read-only |

These join the driver's existing `atrium_gpu_backend_ops` seam in P2, where the lifecycle wiring
lands. Until then the AMD backend is `smu.c` + `rs/smu_metrics.rs` behind the test hook.

### B1. AMD: SMU generations

Everything that differs between SMU generations is data:
- **`smu_gens[]` in `smu.c`**, matched on MP1 major.minor and a revision range: mailbox
  registers, message ids for the allowlist enum, the required-feature mask, table ids. An id of
  0 means the generation lacks that message; the sender refuses it.
- **`LAYOUTS` in `rs/smu_metrics.rs`**, matched on the PMFW's driver interface version: offsets
  of the `SmuMetrics_t` fields read, and the throttler-id → class map. Each row records its
  evidence.

Adding a GPU family is adding rows, with the evidence. Interface sources are Linux's
MIT-licensed `smu_cmn.c`, `smu_v1x_0*.c`, `*_ppsmc.h`, `smu1x_driver_if_*.h`.

**SMU 14.0.2 / 14.0.3 (RDNA4).** The R9700 runs SCPM: on 2026-11-12 the SMU refused
SetAllowedFeaturesMask with 0xFD.

| Phase | Message (id) | Notes |
|---|---|---|
| Start | TestMessage (0x01), GetSmuVersion (0x02), GetDriverIfVersion (0x03) | driver-if picks the metrics layout |
| Start | SetDriverDramAddrHigh/Low (0x0E/0x0F) | 1 MiB driver table in VRAM, inside the driver's reserved area |
| Start | RunDcBtc (0x36) | as amdgpu |
| Start | SetAllowedFeaturesMaskHigh/Low (0x05/0x04), all ones | 0xFD under SCPM is expected and skipped; anywhere else 0xFD is an error |
| Start | EnableAllSmuFeatures (0x06) | powers GC up: rule 7 |
| Start | GetRunningSmuFeaturesLow/High (0x0C/0x0D) | checked against the required set |
| Run | TransferTableSmu2Dram (0x12), arg TABLE_SMU_METRICS (5) | telemetry |
| Run | AllowGfxOff (0x28) / DisallowGfxOff (0x29) | idle policy, scoped guard |
| Run | SetPptLimit (0x32) / GetPptLimit (0x33) | cap (P4; not in the table yet) |
| Run | SetSoftMaxByFreq (0x1A), GetMin/MaxDpmFreq (0x1D/0x1E) | soft max (P4; not in the table yet) |
| Run | SetWorkloadMask (0x24) | profile (P4; not in the table yet) |
| Stop | PrepareMp1ForUnload (0x2E) | last message when staying loaded is over (P2; not in the table yet) |
| Detach | debug mailbox Mode1Reset (2), C2PMSG_53/75/54 | amdgpu's reset method for MP1 14.0.2/3; no response handshake, 1 s without MMIO, then PSP bootloader ready and CONFIG_MEMSIZE readable |

Never sent: DisableAllSmuFeatures, Disable/EnableSmuFeatures*, OverDrive or
PPTABLE/CUSTOM_SKUTABLE writes (TransferTableDram2Smu), manual fan, voltage offsets,
SetHardMinByFreq above default.

Required features:
- FW_DSTATE 17
- THROTTLERS 23
- VR0HOT 27
- FW_CTF 28
- FAN_CONTROL 29
- MEM_TEMP_READ 46

The R9700 reported 0x0488f19e38fffcfb, which has all six.

Metrics layouts:
- **0x2E**: from the header.
- **0x33**: the R9700's PMFW 0x685000. The same offsets were confirmed against a raw table
  read on the box (fixture `rs-host-tests/data/smu14_if33_r9700_hot.bin`):
  - dGPU_W_MAX 300 W;
  - socket and board power 299 W under PPT0;
  - moving-average power 303 W;
  - nine temperatures 40–98 °C;
  - fan 3470 rpm / 73 %.

  MetricsCounter does not advance per read on this PMFW, so it is not a plausibility check.

Throttler classes:
- ids 0–11: thermal
- ids 12–13 (TDC): current
- ids 14–17 (PPT): power
- ids 18–20: other

---

## Phases

- **P1 telemetry.** Done 2026-11-12 (55cb52b2, then this revision):
  - the allowlisted sender with failed-link state;
  - the required-features check;
  - the metrics read with the neutral record, plausibility checks and idle check.

  Still to do:
  - the THM cross-check;
  - sysctls.
- **P2 lifecycle (gates stage 4).** Step 1 (built): on detach after any hook that went past
  read-only, a mode 1 reset puts the card back to its post-POST state (PSP bootloader, no
  sOS, GC off), which is where it idled before the driver touched it. `hw.atrium_gpu.keep_state=1`
  skips it, `hw.atrium_gpu.mode1_reset=1` runs it alone. It also means a bring-up run no longer
  needs a power cycle before the next one. Box test 1: reset alone on a cold card. Box test 2:
  stage 3 with `gc_up_ok=1`, then the reset; GC off afterwards, and the board stays at its
  idle temperature for 5 minutes. Step 2 (box-verified 2026-11-12, efa15aa7): once GC is
  up, gfx_v12_0's SRM + clock gating, then AllowGfxOff (`gfx12_pm.c`): ~280 W to 9 W, then 7 W
  at gfx 0 MHz. Then, for staying attached:
  - gfx init enough for GC to idle (clock and power gating, as gfx_v12_0);
  - GfxOff allowed after bring-up, with the scoped guard;
  - detach / unload / reset sequence (A3.6) with PrepareMp1ForUnload;
  - the backend ops on `atrium_gpu_backend_ops`.

  **Box test:**
  1. Cold boot.
  2. Load with `gc_up_ok=1`.
  3. Unload.
  4. Within 10 s the telemetry shows idle (busy ≤ 20 %, power well under the limit, no
     PPT throttling) and stays there for 5 minutes.
  5. Load again: the features mask and metrics are unchanged.
- **P3 thermal interrupts** (after IH in stage 4).
- **P4 posture and cap** (federation member `gpu0` on silicon). Adds SetPptLimit, soft max and
  SetWorkloadMask to the table. **Box test:** a cap step-down shows in package power within
  2 s.

Every box run reads the telemetry before and after, and its log goes in
[r9700-bare-metal.md](r9700-bare-metal.md) §5.

## Non-goals

- Overclocking, undervolting, overdrive and fan curves (A3.2).
- Driving power for video/display engines beyond what the firmware does on its own.
- BACO / runtime D3: later, a separate section once suspend exists.
