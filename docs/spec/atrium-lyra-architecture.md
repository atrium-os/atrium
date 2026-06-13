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

### 4.3 Default means exclusive — and why

"Lyra is the default audio subsystem" is the wrong framing if it allows a
co-equal raw path. Lyra's three load-bearing guarantees are *bypassable* unless
Lyra is the **sole** owner of the device:

- **Privacy enforcement** (§9). The `audio`/`microphone`/`audio_monitor`
  capability split only means anything if Lyra is the only thing that can touch
  the codec. If an app jail can `open("/dev/dsp")` itself, it captures the mic or
  taps the output with *no* capability check — the model is decorative. The
  enforcement *is* "only lyrad holds the device; apps get audio through a Lyra
  capability."
- **Glitch-protection** (§3). The deadline lane keeps audio clean under load only
  for streams that go through Lyra; a direct `/dev/dsp` writer is back to plain
  timeshare (the 272-underruns case).
- **Single-resample / own-the-mix** (§1). Lyra *and* a live kernel `vchan` mixer
  is the double-mix this design exists to avoid.

So **app jails are not granted raw device access** (no `/dev/dsp`, no codec node)
— only lyrad is. The OSS *API* survives as a **Lyra-backed compat shim**: a
legacy app opening `/dev/dsp` gets a lyrad-provided **`cuse(3)`** device (the
userspace-cdev framework webcamd uses — present in base) that enters it as an
ordinary, capability-gated Lyra client. Keep the contract, replace the guts. The
ASIO-style "give me the raw device for low latency" case is moot: Lyra's
bit-perfect + deadline-lane path already *is* the exclusive low-latency path, so
there is nothing to bypass *to*.

### 4.4 Any device, no driver change (path A); native where it earns it

Device diversity is solved by path A for free. Every FreeBSD audio driver —
`snd_hda` (HDA codecs), `snd_uaudio` (USB Audio Class, i.e. most external
DACs/interfaces), `snd_ich` (AC'97), and the rest — exposes the **same** OSS
`/dev/dspN`. So Lyra's bit-perfect backend is device-agnostic *by construction*:
open the device's `/dev/dsp`, set the exact format, write, and the driver DMAs
Lyra's buffer regardless of whether it is onboard HDA or a USB interface
(D-display-1 brought up HDA; a USB DAC is identical code). Several devices at
once is exactly the §4 clock-domain case — the USB DAC drifts against onboard,
reconciled by measured-drift resampling at the seam.

Lyra's device backend is therefore an abstraction — **a ring + a clock + a
format** — with two implementations:

- **bit-perfect OSS** (path A): the entire FreeBSD-supported device universe
  *today*, zero driver code, approximate clock (`GETOPTR`).
- **native Atrium driver** (path B): only for the high-value classes — HDA and
  USB-UAC together cover ~all real hardware; Bluetooth A2DP is userspace
  regardless — giving the exact DMA-position/interrupt timestamp. Written per
  class, over time, where it earns its keep (the GPU pattern).

**No driver change is needed for path A** — only configuration (bit-perfect) and
the exclusivity policy (§4.3). One cheap, optional middle path closes the §4.1
clock-approximation gap broadly without any native driver: a single generic
`sound(4)` ioctl exposing the exact DMA-position-at-interrupt timestamp would
give Lyra the exact clock across *all* existing `snd_*` drivers at once — a
minor, device-independent enhancement, not a rewrite. So the sequencing is:
exact-clock-everywhere via that ioctl is cheap and broad; full native ownership
is reserved for HDA and USB-UAC where the last increment of control matters.

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
for latency; *third-party* plugins are always sandboxed. (This is the hybrid the
"capability-jailed nodes" choice still permits — the boundary is trust, and
trusted code is part of the engine, not a separate plugin.)

### 6.1 Two confinement layers, and who creates the jail (settled 2026-06-14)

"Sandboxed" above is **two distinct mechanisms**, and conflating them was an
error worth nailing down:

- **Capsicum** — *opt-in self-confinement*: the node process calls `cap_enter()`
  on **itself**, after it has opened its rings + lane fd and `dlopen`'d the
  plugin, and before the untrusted `.so` runs. From then it holds only those fds:
  no `open`, no network, no new fds. This is **built** (`lyra-effect --capsicum`),
  and because the untrusted code is loaded *after* `cap_enter`, the plugin never
  runs un-confined.
- **The Portcullis jail** — *forced confinement*: `jaild` (the TCB, the sole
  caller of `jail_set`) creates a jail and `execve`s the node **inside** it; the
  node gets no say. This is the [[project_scheduler_federation_corrected]]/
  Portcullis mechanism (`docs/spec/portcullis.md`). This is **not yet built** for
  audio nodes.

They are complementary defence-in-depth, not alternatives. Capsicum already
delivers the core property (an untrusted `.so` cannot escape its process). The
jail adds what Capsicum cannot: it does not depend on lyra-effect being correct,
and — the real prize — it makes a plugin a **first-class Portcullis citizen**
(`atrium.toml` manifest, user-granted capabilities, `rctl` caps, unified
lifecycle). The privacy capabilities of §4.3 (`audio` / `microphone` /
`audio_monitor`) live in *that* grant path; a Capsicum-only plugin is invisible
to it.

**Responsibility split (settled):** Lyra never creates jails and never owns
policy. When the forced layer lands, **lyrad requests a node from `portcullisd`**
("launch `org.foo.reverb` as a graph node"); portcullisd does the
manifest/capability/grant policy and asks `jaild` to create the jail; the plugin
process is `execve`'d inside it by jaild and **connects back to lyrad over an
Aqueduct socket**, over which lyrad passes the ring + lane fds via `SCM_RIGHTS`
(POSIX shm is jail-scoped, so the by-name ring of §5.2 becomes fd-passed across a
jail boundary — the Carillon pattern). Capsicum still applies *inside* the jail.
Rejected: lyrad talking to `jaild` directly — it would fragment policy into the
RT audio daemon and widen jaild's trusted client set for no gain over routing
through portcullisd. Each component does its trust-rank's job: lyrad drives the
graph, portcullisd owns policy, jaild owns jail creation.

**Sequencing (committed):** the fd-passing transport + the portcullisd
"launch-a-node" request are the *same* machinery the §7 policy/session layer (and
the privacy capabilities) need, so the forced jail is built **with the L4/L5
session/policy layer**, not as a one-off. Until then, Capsicum is the baseline
and this is recorded as the deferred hardening it is — not a silent gap.

## 7. Policy and the session layer — separate from the engine

Mechanism/policy separation, the lesson PipeWire got right (engine vs
WirePlumber) and PulseAudio got wrong (policy hard-coded):

- **lyrad** is the RT graph **engine** — admit, schedule, mix, resample. Mechanism
  only. No routing opinions.
- **Choragus** (`choragusd`) is the **session/policy layer** — named for the
  citizen who arranged and directed the chorus in classical theatre (*Lyra plays;
  the Choragus arranges who plays*; NAMING.md). It decides *which app → which sink*, default-device
  follow, **ducking** (lower media when a communication stream opens), per-app
  volume, and device-hotplug response. It is **not** a Lua config (PipeWire's
  choice) and **not** hard-coded (PulseAudio's): it is **capability- and
  manifest-shaped**. An app's manifest declares its audio **role**
  (`media` / `communication` / `notification` / `game` / `pro`); the role informs
  default routing and ducking; explicit user routing overrides; capabilities gate
  what is even possible. Policy is data the user and manifests own, enforced by a
  non-RT component, never baked into the engine.

**Why audio needs this when video seems not to — the "audio window manager".**
Video has a policy layer too; it is the **window manager** (Pergola / WM), and it
sits to **Fresco** (composite + scanout, mechanism only) exactly as this layer
sits to lyrad. The reason it does not *look* like a separate need is one physical
fact: **audio is a single additive channel; video is a partitioned spatial
canvas.** Every app's audio *sums* into one stream to one output — the streams
collide in the same air — so *relative level* must be arbitrated continuously
(that is what ducking and per-app volume are). Video windows do not sum; the
compositor *places* them in space, so "who is prominent" is solved by layout and
focus, not by level. There is no video analog to ducking because windows do not
add. But every other audio-policy concern *does* have a video twin, already owned
by the WM: route-to-sink ↔ place-on-monitor; ducking/volume ↔ focus/dim/DND;
default-device-follow-on-hotplug ↔ window migration when a monitor is plugged;
bit-perfect/passthrough **exclusive** ↔ **fullscreen direct-scanout**. So audio
did not acquire a *new* kind of component — it needs its own policy layer only
because audio's arbitration cannot hide inside "placement" (there is no space to
place into; it all sums to one output), where video's can. The layer is, in
effect, the audio window manager — a first-class policy sibling of lyrad, not a
subthread of it.

**Modeled (gpusim `engine/src/lyra_policy.rs`, 7 tests).** `Session::resolve()` is
**pure** — given the streams, devices, and rules it computes the desired
`(sink, gain)` per stream with no scheduling, mixing, or audio-path side effect;
`diff()` turns a desired-state change into mechanism-agnostic `Change`s — a
`GainRamp` (the zipper-free smoother) or a `Reroute` (applied by the §12.2
glitch-free atomic-commit reconfiguration). Proven: Communication ducks Media/Game
and restores; Pro/Notification are never ducked; user volume is the base the duck
stacks on; Communication prefers a headset by role while an explicit user route
overrides; a hotplugged headset becomes default and streams follow (emitting a
single `Reroute`); an **exclusive claim** (bit-perfect / passthrough, §4.3)
refuses a second stream on the device; and policy emits *only* those
mechanism-agnostic changes — never a schedule or a mixed buffer. The
purity of `resolve()` is what lets the layer sit outside the RT engine.

(**Name settled 2026-06-14: Choragus.** Its exact packaging — a non-RT thread in
lyrad vs the sibling `choragusd` daemon — remains open, §11; the pure separation
modeled here permits either, though the "audio window manager" framing favours a
first-class component over a lyrad subthread.)

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

## 12. Comparison with existing systems, and the gaps not yet closed

A design audit against OSS, ALSA, JACK, PulseAudio, PipeWire, CoreAudio,
WASAPI/ASIO, AAudio, and sndio (2026-06-13). Lyra's distinctive wins are argued
above; this section records the **gaps the audit surfaced** — features and
lessons these systems have that the design must still close — so they are not
lost before L4–L6.

**Lessons already absorbed** (do not re-litigate): PulseAudio's timer/rewind
latency (Lyra pulls, no rewind, §3); JACK's one-xrun-kills-everyone (per-node CBS
isolation, §3, proven L0/L3); ALSA's config-file plugin-chain hell (capability +
manifest policy, §7); CoreAudio's resample-once-at-the-HAL (§4); PipeWire's
policy/engine split (§7) and one-graph-for-pro-and-consumer (§2); sndio's small,
sandboxed client API (Capsicum-clean by charter; keep the client surface ~6
calls).

**Gaps to close**, each with a disposition and the layer it lands in:

1. **Passthrough / untouchable formats — encoded bitstream *and* DSD.** Lyra
   assumes PCM end to end. Two real non-PCM cases: (a) **encoded bitstream**
   (AC3/DTS/Dolby) sent untouched over HDMI/SPDIF to an AV receiver that decodes
   it; (b) **DSD** (SACD: 1-bit sigma-delta at 2.8224 MHz+ — DSD64/128/256),
   which audiophile DACs play natively. Both are **untouchable**: they cannot be
   mixed, resampled, or volume-scaled without defeating their purpose (mixing DSD
   means PCM-converting and re-modulating, which no one wants). The clean model:
   a passthrough stream is a **degenerate graph — source → sink, exclusive,
   bit-perfect, no mix/DSP node** — and the §4.3 sole-ownership rule already does
   the work (lyrad grants the device to one exclusive passthrough client and
   denies others for the duration). Transport: **DoP (DSD-over-PCM)** packs the
   1-bit stream into 24-bit PCM frames with a marker (DSD64 → 24/176.4k), so it
   rides the *existing* bit-perfect OSS path with **no driver change** (the DAC
   unpacks DoP); **native DSD** and HDMI bitstream need a driver format
   (`snd_uaudio` / the native UAC + HDA drivers — a path-B, §4.4 item). The
   missing pieces are small: a per-stream "untouchable format" flag, the
   exclusive-claim policy, and DoP packing. **Disposition: real gap; model as a
   passthrough format class (L4/L5).**

2. **Channel layouts / surround / spatial audio.** Lyra's channel handling is a
   bare `frame_floats` count. CoreAudio/PipeWire/Pulse carry rich *channel maps*
   (FL/FR/C/LFE/…), surround configs (5.1/7.1), and object/scene spatial audio
   (Atmos, ambisonics). "Stereo float frames" cannot express surround or
   spatialisation. **Disposition: real gap; a first-class channel-map type + a
   spatialiser node kind (L4).**

3. **A/V sync, and the unified-vs-split media-graph decision.** PipeWire's
   defining bet was **one graph for audio *and* video** (camera, screen-share,
   audio). Atrium splits: Fresco/GPU/Carillon own visual buffers, Lyra owns
   audio. The split is deliberate and defensible — audio rings are tiny, GPU
   buffers are huge, and both are members of the **same energy federation**
   ("coordinated, not coupled") sharing one timing substrate (the deadline lane,
   virtual time). **Contract SETTLED 2026-06-14 (model: `gpusim
   engine/src/avsync.rs`), see §12.1 below.**

4. **Glitch-free dynamic reconfiguration.** PipeWire's hallmark: re-wire/re-admit
   the graph (add a node, change rate, plug a device) *while playing*, no
   dropout. **Protocol SETTLED + MODELED 2026-06-14 (`gpusim
   engine/src/lyra_reconfig.rs`), see §12.2 below.**

5. **Audio offload / low-power compressed playback.** A hardware codec plays
   compressed audio directly so the CPU sleeps — the mobile battery feature
   (AAudio offload). Ties to the **(floor, elastic) energy member (§8)** and the
   Insula mobile profile. **Disposition: real gap, high value for the mobile
   story; an offload sink node + the energy member's deep-idle path.**

6. **JACK transport.** A shared, sample-accurate transport/timeline (play / stop
   / locate, tempo, bars-beats-ticks, a transport master) that DAWs and
   sequencers sync to. We chose pro-native, so it matters. **Disposition: real
   gap; shared transport state on the control plane (§5.1), a pro milestone.**

7. **MIDI as first-class.** §6 has timestamped control/MIDI events on audio
   nodes, but the full story — a MIDI graph, hardware MIDI ports, sample-accurate
   MIDI routing independent of audio (sndio `mio`, JACK MIDI, CoreMIDI) — is
   thin. **Disposition: gap for the pro story; a MIDI edge type + ports (pro
   milestone).**

8. **Device rate-selection policy.** Choose the device's hardware rate
   (44.1/48/96) to *avoid resampling the dominant stream* ("follow the content").
   Lyra resamples clients to the device rate but does not pick it. **Disposition:
   policy gap (§7 session layer).**

9. **Network / remote audio.** Send audio to another Atrium box or a network sink
   (Pulse/PipeWire RTP, AirPlay-shape). Aqueduct *can* carry it (it is the
   transport) but it is not designed. **Disposition: scope decision; deferred,
   rides Aqueduct when it lands.**

**Not gaps** (covered or correct scope): per-app volume / routing / ducking /
default-follow (§7); shared/exclusive + loopback + monitor (§9 + bit-perfect);
module system (graph nodes *are* it); decode (MP3→PCM) is app-level or an
asset-player node — Lyra plays PCM (or passthrough, gap 1); freewheel/offline
render (the deterministic model already *is* freewheeling — the daemon just needs
the mode).

**Read:** the audit found **feature gaps, not foundation gaps** — every item
above is absorbed by the existing graph + lane + federation substrate (passthrough
= an exclusive degenerate graph + an untouchable-format flag; spatial = a node +
a channel-map type; A/V sync = a cross-member timing contract; reconfig = a
glitch-free re-admit; offload = a sink node + the energy member; transport/MIDI =
control-plane state + an edge type). Priority order for the post-baseline phases:
passthrough/DSD (1) and channel/spatial (2) and A/V sync (3) and glitch-free
reconfig (4) first; offload (5) with the mobile profile; transport (6) and MIDI
(7) with the pro milestone; rate policy (8) and network (9) as they come.

### 12.1 The A/V sync contract — Lyra ⟷ Fresco (settled 2026-06-14)

The decision and the contract for gap 3. Two graphs, **one federation** — Lyra and
Fresco stay independent graphs with independent mechanisms (explicitly *not*
PipeWire's single merged graph); the federation is a **shared time reference**,
and the *player* — not either subsystem — reconciles them.

- **Shared clock = `CLOCK_MONOTONIC`** (the engine's picosecond virtual time in
  the model). Both subsystems *already* anchor to it: Lyra's deadline-lane
  `anchor_ns` is the DMA-interrupt timestamp; Fresco's vblank events are
  monotonic. So presentation events are directly comparable with no new clock —
  the contract just *states* that both timestamp against it. This is "coordinated,
  not coupled" applied to time: read-only shared reference, independent mechanisms.
- **Audio is the master clock.** Audio plays out the DAC at a fixed (drifting)
  rate and cannot be stretched without artefacts; video repeats/drops a frame
  near-invisibly. So audio free-runs and **video chases**.
- **Lyra exposes an audio presentation clock**: stream-frame ↔ the monotonic
  instant it reaches the speaker, built from `played_frames` (GETOPTR, ground
  truth — the consumed-frame hardware clock), the DMA anchor, and the reported
  output latency (GETODELAY buffered frames **+** the fixed codec/analog group
  delay). The native HDA path makes the anchor exact. (The audio analog of
  Wayland `wp_presentation` / Vulkan `VK_GOOGLE_display_timing`.)
- **Fresco exposes present-feedback + target-present-time**: the actual vblank
  instant a committed frame became visible, and (where supported) "present this
  frame at-or-after monotonic time T". It already has vblank/flip-done on kqueue
  and the scanline-accurate timing model; this surfaces what the engine computes,
  reporting the pipeline+panel latency too.
- **The player owns the policy.** It holds both PTSs, reads Lyra's audio clock to
  learn stream-time at the speaker, computes each video frame's ideal present
  instant, and lands it on the Fresco vblank nearest that instant. Neither
  subsystem references the other.

**VRR-aware from the start:** on a variable-refresh display Fresco can present a
frame at the *exact* target instant within the refresh range (no fixed grid),
which drives skew to ~0; fixed-refresh is the ±½-refresh fallback. The contract
carries a target-present-time so a VRR panel uses it and a fixed panel snaps.

**Latency honesty is the load-bearing requirement.** Bounded skew needs each side
to report its *output* latency (audio buffer + codec; display pipeline + panel —
an HDMI sink can add tens of ms); the player subtracts both to align "heard now"
with "seen now". Report what the driver/EDID give; expose the residual as a knob.

**Proven (gpusim `engine/src/avsync.rs`, 5 tests)** by composing the two real
engine clocks (Lyra DAC `DeviceClock` @48 kHz, Fresco refresh `DeviceClock`
@refresh) in one virtual-time reference: audio-master + nearest-vblank holds
lip-sync within ±½ refresh (~8.3 ms @60 Hz) **forever** under DAC drift; a naïve
wall-clock player (never reads the hw clock) drifts unbounded past the perceptual
window (EBU R37 ≈ +40/−60 ms) — the cross-modal twin of the §4 measured-vs-assumed
result; **independent** drift in *both* domains is absorbed (the federation
point); VRR drives skew to ~0; latency compensation re-centres a standing offset.
The wire-level query/feedback ABI on each side is the implementation follow-up
(with the L4 session layer / the native display present-feedback path).

### 12.2 Glitch-free reconfiguration — atomic commit at a period boundary (settled 2026-06-14)

The protocol for gap 4, and a second instance of the same federation move as the
display: **reconfigure by atomic commit at a timing boundary.** Fresco commits a
new framebuffer config atomically at vblank; Lyra commits a new *graph* atomically
at a period boundary. Build + admit the new topology in the **control plane**
(off the audio path), then flip it in between periods — the RT thread only ever
does a pointer swap.

- **Prepare-then-flip.** Allocate the new node rings, carry over persistent nodes'
  streaming state, and run [`Graph::admit`] on the new topology — all in the
  control plane. Then arm the swap; at the next period boundary the active
  `(graph, schedule)` pointer flips. The audio thread never blocks on reconfig
  work, so the ring never starves. (Doing the rebuild *on* the audio thread —
  the anti-pattern — stalls a period past the double-buffer slack and underruns.)
- **Admission is the safety gate.** The new graph must pass `Σ Q ≤ U_lane·T`
  *before* the swap is armed. If it doesn't fit, the reconfiguration is
  **rejected** and the old graph plays on — zero dropout either way. A change that
  would overrun the lane fails cleanly instead of glitching. (Force-swapping past
  the gate to an over-budget graph starves — the counter-proof of why the gate
  exists.)
- **Atomic at the boundary.** Every period runs wholly the old or wholly the new
  topology — never a half-old/half-new period.
- **Continuity / crossfade.** Where the swap is bit-continuous (insert a node
  whose dry path is unchanged; ramp a gain via the zipper-free smoother) it is a
  hard cut at the boundary. Where the output genuinely differs (re-route to a
  different device/clock domain, remove a sounding node) a short **equal-power
  crossfade** over the seam bounds the adjacent-sample step — no click. A latency
  change re-aligns PDC (§6) at the swap, absorbed by the crossfade.

**Proven (gpusim `engine/src/lyra_reconfig.rs`, 6 tests)** against the real
admission ([`Graph::admit`]) and the real underrun referee (`AudioRing`):
prepare-then-flip adds an effect mid-stream with **zero** underruns; inline rebuild
on the audio path starves (why prep is off-path); an over-budget reconfiguration
is rejected and the old graph plays on with zero dropout; force-swapping past the
gate starves; the swap is a single atomic transition at a boundary; an equal-power
crossfade more than halves the worst-case step of a hard re-route cut. The
control-plane re-admit + state-carry-over wire protocol (over §5.1) is the L4
implementation follow-up.

## 13. Spatial acoustics / audio ray-tracing (future direction)

A future, ambitious capability, recorded for scope — it ties §12's spatial
extension, the energy member (§8), the GPU scheduler, and Tessera CAS into one
feature, and Atrium is unusually well-positioned for it because it owns the GPU,
the audio graph, and a *shared* compute budget at once (the three things usually
owned by three different teams). Far beyond the current L-phases; here so the
idea is durable and correctly scoped.

**What it is.** Simulate how sound propagates through a 3D scene — occlusion,
early reflections, late reverberation, and arrival *directions* — rather than
bolting on a generic reverb (the Steam Audio / Project Acoustics / VRWorks Audio
problem). Every real-time system splits it the same way, and that split is what
makes it tractable: **ray-trace the room occasionally, render the audio
continuously.**

- **The slow solver** — ray/path tracing over scene geometry → *acoustic
  parameters*: a set of arrival paths (direction, delay, gain, filter) plus a
  reverb tail, updated at ~10–60 Hz as source/listener/geometry move. A **GPU
  compute workload**, not per-sample.
- **The fast renderer** — applies those parameters every audio period: per-path
  delay/filter, **spatial panning of each arrival** (`spatial.rs::pan` — each
  ray-traced direction is exactly a `pan(azimuth, layout)`), and convolution with
  the reverb impulse response. Hard real-time, on the deadline lane.

**Why the Atrium substrate fits (four points):**

1. **One budget for acoustics and graphics.** The solver is a GPU compute job in
   the *same energy federation* as the audio (§8, P6). "Spend more GPU on better
   acoustics" vs "on graphics" is **one `water_fill` decision**, not two
   subsystems fighting — no other stack unifies the acoustics-compute and
   graphics-compute budget.
2. **The renderer is already-built graph nodes.** A per-path delay/filter node,
   a (partitioned-FFT) convolution node, and the **spatial panner this session
   built** — the directional-rendering primitive audio ray-tracing feeds into
   exists today.
3. **Graceful degradation falls out of the (floor, elastic) member.** Ray-traced
   reverb is the textbook *elastic* load: under power/thermal pressure, degrade
   full path-traced acoustics → fewer rays → a cheaper parametric reverb →
   eventually dry + basic panning (the floor). The DAC never starves; the
   acoustics simplify. Exactly the member shape §8 proved, and the right
   behaviour (lose reflection detail, never glitch).
4. **CAS-cached impulse responses.** A computed IR for a given room + listener
   position is a Tessera CAS blob — deduplicated and cached by hash; return to a
   spot and the IR is a cache hit.

It is also **deterministically modelable** in virtual time (the solver + renderer
prove out pre-silicon, like the rest of the substrate).

**Honest hard parts:** real-time convolution of long IRs is expensive but a known
technique (uniformly-partitioned overlap-add FFT — the bespoke FMA-fused compute
backend is the right tool); **diffraction** (UTD/BTM edge bending) is the
genuinely hard acoustic problem, approximated everywhere including here; **HRTF**
for binaural needs head-related transfer functions (generic works, personalised
is open research); solver **accuracy vs ray budget** is a real dial — which is
precisely why the elastic energy member is its home.

**Scoping:** **app-opt-in, not a system default.** Ray-tracing means something
only when there is a 3D scene with positioned sources (games, VR, spatial-audio
apps); the app submits geometry + object/listener positions, Lyra + the GPU
render it. A notification ping or music has no scene and uses the ordinary graph
(plus `spatial.rs` panning if it wants placement). Anticipated by the
wire-format's reserved "audio ray-tracing" autonomous task (`0x0200`).

**Decomposition (when it is built):** a GPU acoustic-solver job (a federation
compute member, elastic) → acoustic parameters → Lyra renderer nodes (per-path
delay/filter + convolution + `spatial.rs` panning + optional HRTF) on the
deadline lane. Nothing about it threatens the design — another node kind plus a
compute job on the proven substrate.
