# Lyra — audio architecture: the kernel-scheduled deadline graph

**Status:** design, 2026-06-13. The audio subsystem (service **Lyra**, daemon
**lyrad**). Companion to [`atrium-scheduler-federation.md`](atrium-scheduler-federation.md)
(the deadline lane Lyra is built on), [`atrium-display-architecture.md`](atrium-display-architecture.md)
(the timing-model lineage), and [`aqueduct.md`](aqueduct.md) (transport).
Grounds against the gpusim deterministic model (`engine/src/audio.rs` and its
graph extension). Naming settled in [`NAMING.md`](../NAMING.md): Lyra / lyrad.

Ambition set with the user (2026-06-13), all three at the deep end:
**pro-native co-equal from day one**, **first-class clock domains with
measured-drift resampling**, **effects/plugins as capability-jailed graph
nodes**.

## 0. Thesis — the audio graph *is* a kernel-scheduled deadline graph

Every other audio server runs its processing graph on a userspace real-time
thread that *hopes* the kernel scheduler cooperates — and the whole PREEMPT_RT
edifice, JACK's RT threads, and PipeWire's RT scheduling exist to coax a
general-purpose kernel into not glitching. **Atrium does not have to coax
anything: it owns the scheduler, and we already built and verified the piece
that matters** — a CBS-admitted declared-deadline lane that ran audio shape
(2.7 ms period) at **0 misses in 10000 periods under full load**
(`atrium-scheduler-federation.md` §1; phases I–K).

So Lyra inverts the usual structure. Instead of one RT thread running a graph:

> **Each graph node that must run per period is a CBS-admitted lane entity;
> lyrad is the deadline broker that admits it; and the kernel's EDF order within
> the period executes the graph in topological order by construction.**

The "audio callback" stops being a thread praying for a timeslice and becomes a
*reservation with a hardware-anchored deadline*. Three properties follow that no
other stack can offer together, because no other stack owns its scheduler, its
sandbox, and its clock timestamps at once:

1. **Low latency by default, safely.** Consumers get large buffers because of
   jitter. The lane *bounds* jitter (measured: tens of µs). The minimum reliable
   buffer — which the model proves equals `jitter × rate` — is therefore small
   and *known*, for **every** app, not just pro ones. Pro-grade latency becomes
   the default, not an ASIO escape hatch.
2. **Clock drift measured, not guessed.** We own the DMA position timestamps and
   the deadline lane, so inter-device clock drift is an *exact measured* quantity,
   not the estimate every other stack makes badly. First-class clock domains
   become tractable (§4).
3. **A graph of sandboxes.** Each node is a Portcullis jail with its own CBS
   reservation. A crashing or malicious plugin throttles or dies *in isolation*
   (overrun isolation, D5) — it cannot glitch other streams or take down lyrad.
   JACK's one-xrun-kills-everyone fragility is structurally impossible.

## 1. What we are designing against

Named failures, so the design is grounded, not naive:

- **PulseAudio's latency and rewind.** Timer-based scheduling with buffer
  rewinds was a complexity sink and a latency floor. Lyra is pull-driven by the
  hardware deadline; there is no rewind because there is no speculative buffering
  ahead of the lane.
- **PulseAudio's monitor-source privacy leak.** Any client could read the system
  output by default. In Lyra, tapping output is a separate, scarier,
  default-deny capability (§9).
- **Multi-stage resampling.** ALSA/Pulse stacks resample repeatedly. Lyra
  resamples **once per clock-domain boundary** and once at a client-rate→domain
  adapter, never in the hot mix path (§4).
- **JACK's all-or-nothing fragility and citizenship.** One xrun stalls everyone;
  every client must be a good RT citizen. Lyra's per-node CBS isolation removes
  both.
- **ALSA's config hell.** Routing/format policy as a static plugin-chain config
  file is unmaintainable. Lyra separates the RT engine (mechanism) from a policy
  layer that is capability- and manifest-shaped, not config-file-shaped (§7).
- **Clock drift by estimation.** Adaptive resampling driven by PLL guesswork
  ticks and glitches. Lyra drives it from kernel-measured drift (§4).

## 2. The core model — nodes, edges, domains

A **graph** is a DAG of **nodes** joined by typed audio **edges**. It runs once
per device period; the pull originates at the sink (the DAC's frame deadline) and
propagates upstream.

**Node kinds.**
- **Source** — a client stream (an app producing PCM), an *asset player* (a CAS
  blob decoded to samples), or a *capture tap* (a device input).
- **Process** — an effect/plugin (EQ, reverb, spatial, dynamics) or a *resampler*
  (rate/format/clock-domain conversion). Each runs in **its own Portcullis jail**
  with its own CBS reservation.
- **Mix** — sum N inputs to M outputs (gains, pan); the heart of the consumer
  path.
- **Sink** — a device output (DAC), a *capture sink* (to a capture-capable
  client), or a *monitor tap* (capability-gated, §9).
- **Domain-crossing** — an adaptive resampler at a clock-domain boundary (§4); a
  specialised Process node.

**Edges** are zero-copy shared-memory rings between adjacent nodes — the
slot/ring transport Fresco already uses, here mapped into both jails (doorbell +
shmem, exactly the Carillon shape used for the GPU). The producer's deadline
fires, it writes its output ring; the consumer's (later) deadline fires, it reads
it. No copy crosses a node boundary; large/static content rides Tessera CAS
(§5.3).

**Why a graph and not streams+mixer.** Streams-and-mixer (PulseAudio) cannot
express the pro path (sample-accurate effect chains, plugin delay compensation,
arbitrary routing) without bolting a graph on later — the trap that kept Linux
consumer and pro audio split for a decade until PipeWire. The graph is the
superset; the consumer case is simply *source → mix → sink*, a three-node graph.
Pro and consumer are the **same substrate** at different buffer sizes and node
counts, by construction.

## 3. Execution — the deadline lane as the graph executor

This is the spine. In a pull graph, the **sink** carries the hard deadline: the
DAC must have frame N ready by the exact instant the DMA engine consumes it
(`audio.rs::frame_deadline_ps`). Every upstream node must finish before its
consumer runs, so **topological depth maps to deadline**: the deepest source has
the *earliest* sub-deadline within the period; the sink has the *latest*
(= the DMA deadline). Therefore:

> **EDF over topological-depth deadlines executes the graph in dependency order
> for free.** The graph's data dependency *is* the lane's deadline ordering.

lyrad, as the deadline broker (it holds the `deadline_broker` capability, D9),
performs **admission and deadline assignment**:

1. Topologically sort the graph; assign each node a sub-deadline
   `d_node = DMA_deadline − Σ(downstream node budgets)` (a node must finish early
   enough that everything downstream of it still fits before the DMA deadline).
2. Admit each node's `(Q_node, T)` into the lane via `SPONSOR_FOR`, anchored to
   the device's frame grid (the vblank-anchor mechanism, retargeted to the audio
   frame clock). `Q_node` = the node's measured worst-case process time;
   `T` = the device period.
3. Reject the graph (or degrade it — drop an elastic effect, §8) if
   `Σ Q_node > U_lane · T`: the period cannot hold the chain. Admission is the
   guarantee, exactly as in the scheduler doc.

**Cross-node deadline propagation = K-b.** When a Process node runs on a client's
stream, it is doing the client's work under the client's deadline. The node
**adopts** the upstream entity (`LAMIOC_ADOPT`, K-b) so its runtime is charged to
the chain's budget and it inherits the band for selection — the audio chain
(app → effect → effect → mix → driver) is exactly the cross-jail, cross-IPC chain
K-a/K-b were built for. A greedy plugin burns the chain's budget and throttles
*itself* (CBS), never the device deadline.

**The whole graph runs inside one device period.** Because admission bounds
`Σ Q_node ≤ U_lane · T`, end-to-end latency is **one buffer period**,
sample-accurate, with bounded jitter — for an arbitrarily deep jailed-node chain.
That is the property that makes capability-jailed pro plugins viable: the
sandbox-and-IPC cost is paid inside the period budget, not added to latency.

## 4. Clock domains and measured-drift resampling

A **clock domain** is a maximal subgraph driven by one hardware clock — one
device's frame deadline. The onboard DAC is a domain; a USB mic is another; a
Bluetooth sink a third. Within a domain everything is sample-locked at the
domain rate; no resampling, no drift.

Domains drift relative to each other (every crystal is `nominal ± ppm`, and the
ppm wanders with temperature). Crossing a domain is an **explicit adaptive
resampler node** whose ratio is driven by **measured** drift:

> lyrad reads each device's true consumed-frame count against the system
> monotonic clock (the kernel hands us the DMA position — we own the driver), so
> `actual_rate = consumed_frames / elapsed_monotonic` is **exact**, not a PLL
> estimate. The A→B resampler ratio is `actual_rate_B / actual_rate_A`, updated
> each period by a slow control loop (drift changes slowly). No estimation, no
> hunting, no periodic glitch.

The resampler's filter length (its latency) is **declared and accounted** in the
graph's latency budget (§3 step 1) — so plugin delay compensation and A/V sync
stay sample-exact across domains. Resampling happens **only** here and at the
client-rate→domain adapter; the domain interior runs at one rate. This is the
CoreAudio "resample once at the HAL" discipline, generalised to N domains and
driven by exact drift.

**Modeled deterministically** (§10): two `AudioRing`s at slightly different
rates, a resampler node between, the referee asserting no underrun and bounded
drift error over a long run — the audio analogue of the display tear test.

### 4.1 The device interface — OSS bring-up, native endpoint

§3 anchors the sink deadline to "the exact instant the DMA engine consumes" a
frame, and §4 reads "each device's true consumed-frame count… because Atrium
owns the driver." That is the *endpoint*. The *bring-up* path is more pragmatic,
and the two must be reconciled honestly.

On FreeBSD the audio stack **is** OSS: `sound(4)` exposes `/dev/dsp*` directly
through OSS v4 ioctls (`<sys/soundcard.h>`) — there is no separate ALSA-like
layer, so "go through OSS" and "talk to the driver" are the *same thing*. The
HDA driver is `snd_hda` (matching QEMU's `intel-hda`; `snd_ich`/`snd_es137x` for
the AC97/ES1370 fallbacks). It is charter-clean — native FreeBSD, not a Linux
shim. But `sound(4)` interposes **vchans** (in-kernel software mixing) and
**feeder chains** (in-kernel rate/format conversion), and routing Lyra through
those would *double-mix and double-resample* — the precise failure §1 designs
against.

**Two paths, sequenced like the GPU driver (bring-up on what works → converge to
native):**

- **A — OSS `/dev/dsp` in bit-perfect mode (bring-up).** Disable vchans
  (`dev.pcm.N.bitperfect=1` / `hw.snd.maxautovchans=0`) and open at the exact
  hardware rate/format so no feeder fires; the kernel then just DMAs Lyra's
  already-mixed buffer to the codec. Lyra still owns the mix and resamples once.
  The lane anchor (§3) comes from `SNDCTL_DSP_GETODELAY`/`GETOPTR` — an
  *approximation* of the DMA play position, good enough to drive the daemon. For
  deterministic, headless gating the QEMU `wav` backend captures output to a
  file; `coreaudio` plays it live on the macOS host.
- **B — an Atrium-native HDA driver (endpoint).** As with the GPU, Lyra
  eventually talks the HDA DMA ring directly, with zero in-kernel mixer/feeder.
  This is what §3/§4 actually want: the *exact* DMA position **and the
  frame-interrupt timestamp** — the audio analogue of frescod's vblank anchor,
  which the measured-drift resampler (§4) and the sink deadline (§3) are sharper
  for. Justified precisely when the `GETODELAY` approximation becomes the
  limiting error.

So §3/§4's "Atrium owns the driver" is the **target invariant**; path A is the
bring-up that approximates it through OSS without violating the single-resample
or own-the-mix rules. The deterministic model (§10) is anchor-source-agnostic —
it proves the algorithm against an exact clock, and either path feeds it.

### 4.2 What Lyra subsumes — `sound(4)` is three layers, not one

"OSS" is loosely used for the whole FreeBSD audio stack, but `sound(4)` is three
distinct layers, and Lyra's relationship to each is different:

1. **Hardware drivers** (`snd_hda`, `snd_ich`, `snd_uaudio`) — the part that
   touches silicon: the DMA ring (HDA's BDL), the codec/widget graph, the
   fragment-consumed interrupt, jack detection. Irreplaceable in principle
   (something must drive the device).
2. **The `pcm` core** (`sys/dev/sound/pcm/`) — the device-independent middle,
   and it is, item for item, a **rigid in-kernel unsandboxed version of Lyra's
   graph**: `vchan.c` (software mixing), `feeder_rate.c` (a resampler),
   `feeder_eq.c` (an EQ), `feeder_volume.c` (per-stream volume),
   `feeder_matrix.c` (channel matrixing), `feeder_format.c` (format conversion),
   `mixer.c`/`feeder_mixer.c` (the mixer).
3. **`dsp.c`** — the `/dev/dsp` cdev + OSS v4 ioctls: the *interface* only; the
   machinery is all in layer 2.

The positioning, then, is neither "replace OSS" nor "coexist with it":

- **Lyra subsumes layer 2.** The kernel's mix/resample/EQ/volume/matrix is
  exactly what the graph does, and Lyra does it *better* on every axis this
  document argues: sandboxed (a jailed reverb cannot crash the mixer — there is a
  literal `feeder_eq.c` in the kernel today that Lyra makes a jailed plugin
  node), deadline-scheduled (the kernel mixer runs blind at interrupt time with
  no admission or overrun isolation), single-resample (§1; kernel feeders can
  resample twice), and *arbitrary* (graphs/effects/routing a fixed kernel mixer
  structurally cannot express). The kernel mixing the audio is the thing Lyra is
  designed to obsolete.
- **Lyra keeps layer 1**, and converges it to a **native Atrium HDA driver**
  (path B, §4.1) — the audio analog of the native GPU driver — which hands Lyra
  the raw DMA ring + the exact play-position/interrupt timestamp and nothing
  else. Until then, bit-perfect `/dev/dsp` *neuters* layer 2 (vchans/feeders
  off), which is why path A is already a near-pass-through.
- **Lyra demotes layer 3 to a compatibility shim.** `/dev/dsp` does not
  disappear; it stops being *the audio system* and becomes a thin lyrad-backed
  surface for legacy/ported apps, while native apps use Aqueduct audio-control
  (§5). This is the PipeWire move (keep the hardware drivers, replace userspace
  dmix/plug + PulseAudio + JACK with one userspace graph, expose compatibility) —
  with the charter twist that Atrium's compat surface is **OSS**, native FreeBSD
  rather than a Linux shim, so it is the *right* compat layer for a BSD-native
  OS, and Atrium ultimately owns the driver too.

This mirrors the rest of the platform exactly: as Fresco owns composition above a
minimal kernel scanout and atrium-gpu owns the submit path above a minimal
kernel GPU mechanism, **Lyra owns mixing/resampling/routing/effects/policy in a
sandboxed, deadline-scheduled userspace, and leaves the kernel only the
hardware-touching mechanism** — the in-kernel DSP layer is subsumed, not shared.

## 5. Transport

### 5.1 Control plane — Aqueduct class 5

Stream lifecycle, graph construction, routing, volume, format negotiation ride
**Aqueduct class 5 (`audio-control`)** over `lyrad.sock`, per-client slot rings
(the Fresco shape; opcodes `0x0A00–0x0AFF` reserved in `wire-format.md`).
Low-frequency, capability-passing, content-addressed. The client never sees the
graph engine's internals — only its own streams and its routed sink (§9).

### 5.2 Data plane — node edges

Zero-copy shared-memory rings between adjacent nodes (§2). For jailed nodes the
ring is shmem mapped into both jails with a doorbell (kqueue `EVFILT`) for the
rare out-of-band wake; steady state is lane-driven (the consumer's deadline fires
and it reads). One ring per edge, depth sized to the period.

### 5.3 Assets — content-addressed sounds

A **sound** — a notification ping, a sample, a decoded media buffer — is a
Tessera CAS blob, referenced by hash. This is genuinely Atrium-native and buys
three things at once:

- **Dedup:** the one notification ping 100 apps share is one blob.
- **Zero-copy:** the asset-player node reads the CAS-mapped page directly into
  the mix; no copy from client to server.
- **Capability:** an app is granted *"play this asset into your sink"* — a hash
  plus a grant — not raw DAC access. The system hands out play-grants for its
  sounds; an app cannot synthesise arbitrary output it was not granted.

Live streams are ephemeral and not usefully CAS'd frame-by-frame; the mixed
output *can* be checkpointed to CAS for deterministic replay/debugging (the model
discipline the rest of Atrium has).

## 6. Nodes and the plugin ABI (pro-native, sandboxed)

Pro-native means **sample-accurate** processing and **external plugin hosting**,
both first-class. A plugin is a **jailed graph node** behind a frozen **C ABI**
(charter: public APIs are C ABI):

```c
/* The node process implements this; lyrad drives it on the deadline lane. */
struct lyra_node_io {
    const float *const *in;   /* in[port][frame], domain-rate, deinterleaved */
    float *const       *out;  /* out[port][frame]                            */
    uint32_t            nframes;     /* this period's frame count            */
    uint64_t            frame_pos;   /* absolute sample position (sample-accurate) */
    uint64_t            deadline_ps; /* this period's hard deadline          */
    const struct lyra_ctrl_event *ctrl; /* sample-timestamped control/MIDI   */
    uint32_t            n_ctrl;
};
int lyra_node_process(void *state, struct lyra_node_io *io);  /* RT, no syscalls */
```

- **Sample-accurate** because `frame_pos` and per-event sample timestamps are
  exact; control/automation/MIDI events are sample-positioned on a control edge.
- **Sandboxed pro plugins** — no host has this. VST/AU run in-process and crash
  the host; a Lyra plugin runs in its own jail with its own CBS reservation. A
  third-party reverb cannot corrupt lyrad, read another stream, or miss anyone
  else's deadline. This leans entirely on the jailed-node + K-b machinery.
- **Plugin delay compensation (PDC):** each node declares its latency
  (filter/lookahead); lyrad sums per-path latency and aligns, so pro graphs stay
  phase-coherent — the thing DAWs need and consumer stacks ignore.

System/trusted DSP (the core mixer, the domain resamplers) is built in to lyrad
for latency; *third-party* plugins are always jailed. (This is the hybrid the
"capability-jailed nodes" choice still permits — the boundary is trust, and
trusted code is part of the engine, not a separate plugin.)

## 7. Policy and the session layer — separate from the engine

Mechanism/policy separation, the lesson PipeWire got right (engine vs
WirePlumber) and PulseAudio got wrong (policy hard-coded):

- **lyrad** is the RT graph **engine** — admit, schedule, mix, resample. Mechanism
  only. No routing opinions.
- The **session/policy layer** decides *which app → which sink*, default-device
  follow, **ducking** (lower media when a communication stream opens), per-app
  volume, and device-hotplug response. It is **not** a Lua config (PipeWire's
  choice) and **not** hard-coded (PulseAudio's): it is **capability- and
  manifest-shaped**. An app's manifest declares its audio **role**
  (`media` / `communication` / `notification` / `game` / `pro`); the role informs
  default routing and ducking; explicit user routing overrides; capabilities gate
  what is even possible. Policy is data the user and manifests own, enforced by a
  non-RT component, never baked into the engine.

(The session layer's exact packaging — a thread in lyrad vs a sibling daemon —
and its Latin name are open, §11.)

## 8. Energy — the (floor, elastic) federation member

Audio joins the P6 energy federation (`atrium-scheduler-federation.md` §4) as a
member with a **shape no other member has**. CPU and GPU demand is uniformly
compressible; **audio's is not**:

> Audio demand = **(non-compressible floor, compressible top)**. The floor is the
> DAC feed plus minimum resampling — throttle below it and you get the **underrun,
> the one glitch users will not forgive**. The top is effects, spatial audio, and
> high-quality resampling — degradable under power pressure.

So the federation's `water_fill` gains a small generalisation: a member may
declare a non-compressible floor, and the allocator guarantees
`grant_i ≥ floor_i`, splitting only `cap − Σ floor_i` across the elastic demands.
If `cap < Σ floor_i` the system cannot keep audio alive — a genuine thermal
emergency, surfaced, not silently glitched. Under pressure Lyra degrades
*gracefully and audibly-safely*: drop spatial, fall to a cheaper resampler, bypass
elastic effects — never starve the DAC. This is the right behaviour and it falls
out of the member shape.

## 9. Privacy and capabilities

Capability-gated, default-deny, Portcullis-enforced — designed against the
monitor-source leak:

| Capability | Grants | Default |
|---|---|---|
| `audio` | play to *your* routed sink; see only your own streams | deny |
| `microphone` | capture from a mic device | deny, user-visible |
| `audio_monitor` | tap the system output or another stream (loopback) | deny, **prominently** surfaced — the audio screen-record |
| *(asset grant)* | play a specific CAS asset (system-issued) | per-grant |

The global mix is **never** visible to a node without `audio_monitor`. Capture and
monitor are distinct (a conferencing app needs the mic, not the system tap). All
three appear in the manifest and at the user-facing grant surface — privacy by
construction, not by a config the user never sees.

## 10. The deterministic model (audio.rs lineage)

Like display-timing and the scheduler, Lyra is **proven in a deterministic
virtual-time model before any daemon or kernel code** — the gpusim engine
substrate (`Timeline`, the cost model, the referee pattern). `engine/src/audio.rs`
already models a single `AudioRing` with **underrun = referee fault** at the exact
frame deadline (4 proofs, including minimum-reliable-buffer = `jitter × rate`). The
graph extension adds:

- **Nodes with process cost** (from the cost model) and **ring edges**; the
  deadline-lane scheduler simulated over the Timeline.
- **Clock domains with drift** and resampler nodes (§4); referee asserts bounded
  drift error and no underrun over long runs.
- **Overrun isolation:** a slow node throttles itself; the chain still meets the
  sink deadline; other graphs unaffected.
- **The (floor, elastic) energy member** under a shrinking cap.
- **Per-sink minimum reliable buffer** as a function of the lane's bounded jitter.

What it proves, deterministically and pre-silicon: *an admitted graph meets every
sink deadline; a misbehaving node is isolated; drift is bounded; degradation under
power is audibly safe.* These are the gates for the kernel/daemon build.

## 11. Phases

Model-first, mirroring the scheduler discipline. Roadmap home: **D4** (foundation
apps need a baseline) — but the substrate here is the full design, with the
baseline as its first slice.

- **L0 — model.** Extend `audio.rs` to the graph: nodes, ring edges,
  topological-deadline EDF over the Timeline, the referee gates of §10. Pure
  deterministic engine work, no VM. *Gate:* admitted graph meets all sink
  deadlines; node overrun isolated; single-domain first.
- **L1 — clock domains in the model.** Two domains, measured-drift resampler,
  bounded-drift + no-underrun referee. *Gate:* §4 proofs.
- **L2 — lyrad skeleton + one device, one stream.** lyrad as a deadline broker:
  open a stream, sponsor its node on the lane (reuse `SPONSOR_FOR`), feed a real
  (modeled or HVF) output device, the three-node consumer graph
  (source → mix → sink). *Gate:* in-VM, a synthetic client at audio shape, 0
  underruns under spinner load (the metronome result, now through lyrad).
- **L3 — the node ABI + a jailed plugin.** The C node ABI (§6); one effect as a
  Portcullis-jailed graph node adopting the chain (K-b). *Gate:* a crashing
  plugin is isolated; PDC aligns; chain holds deadlines.
- **L4 — mixing, routing, the policy layer.** N clients → mix → device; the
  session/policy layer (roles, ducking, per-app volume, hotplug). *Gate:* the
  consumer experience; frame-of-audio variance; the federation member registered
  with its floor.
- **L5 — assets + capture + monitor.** CAS asset players (zero-copy); the
  `microphone` / `audio_monitor` capability split; per-app privacy. *Gate:*
  privacy enforced; asset dedup/zero-copy demonstrated.
- **L6 — the measurement story.** The second headline metric the scheduler doc
  promised: **underrun count / minimum reliable buffer under load**, Lyra vs a
  reference, plus pro-path latency. Ties back to P7.

The within-substrate ambition (pro-native, multi-domain, jailed nodes) is present
from L0 in the *model* and lands incrementally L2→L4 in the daemon, so the pro
path is never a retrofit — it is the substrate the consumer path is the simplest
case of.
