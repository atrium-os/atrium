# A functional-safety variant of Atrium on Solum (an ADAS OS)

**Date:** 2026-11-27. **Asked by:** Shankar: "if we wanted to create a variant of Atrium for
a functional safety application like an ADAS OS, then we would need additional
protections. What are they and can we achieve it without losing performance? Do we need
two flavours or can the same one work for both? For example, lock step computing, the
hypervisor can compare the outputs. We'll analyse this but if it turns out it costs too
much, we won't."
**Spec:** [solum.md](../spec/solum.md). Study only; nothing is proposed for the build
order.

## 1. Answer

- **The FreeBSD-based Atrium itself can never be the safety-rated part.** No assessor will
  rate a general-purpose kernel of millions of lines of C, plus the GPU stack, at any ASIL.
  Every safety OS on the market (QNX for Safety, PikeOS, INTEGRITY, the hypervisor-based
  automotive stacks) uses the same pattern: **mixed criticality**. A small, certified base
  separates a non-safety rich OS (QM, "quality managed") from small safety partitions,
  and proves **freedom from interference** (ISO 26262-6 Annex D) in space, time and
  communication.
- **Solum is already most of that base.** Spatial isolation (stage-2 tables, IOMMU), a
  signed static manifest, links with epochs and bounded records, no device emulation, and
  a small Rust core are all what such a hypervisor needs. What it lacks is temporal
  isolation (guaranteed CPU time, cache and memory bandwidth), end-to-end protected
  communication, hardware fault detection, safe states and health monitoring, and above
  all certification evidence (§3).
- **Performance:** the main domain (Atrium as today) keeps near-native speed per core. A
  safety configuration costs it the **cores, cache ways and memory bandwidth reserved for
  the safety partitions**, with a fixed amount taken, not a percentage slowdown on
  everything. Lockstep costs only the protected tasks, not the system (§4).
- **Lockstep:** use hardware lockstep where the SoC has it (every ADAS SoC does: Arm
  Cortex-R52 safety islands, and Cortex-A78AE/A720AE cores in split-lock mode). Solum's
  software lockstep, with two copies in two domains and outputs compared before release,
  is feasible and cheap for **small, deterministic control tasks** (2x their CPU only). It
  is the wrong tool for perception on a GPU or accelerator: there the pattern is
  **doer/checker**, where a simple certified monitor checks the plausibility of the
  complex unrated output (§4).
- **One flavour or two:** **one Solum codebase and one Atrium, with two build and
  manifest profiles**: `desktop` (today's design) and `safety` (static partitioning,
  reserved cores, time partitioning, extra checks, no warm restart). The certified
  artefact is the `safety` configuration of Solum plus the safety partitions' executive.
  Features the safety profile doesn't use are compiled out (`cfg`), so the certified code
  stays small. Not a fork (§5).
- **Cost:** the engineering is moderate. **Certification is the expensive part:** an ISO
  26262 process from requirements to MC/DC coverage, a qualified toolchain, a safety case,
  and an external assessor. It is a multi-year, multi-person programme, and only worth it
  with an automotive partner and target SoC. **Recommendation:** don't pursue it now. Keep
  Solum's design compatible with it (§6), which costs almost nothing, and revisit if a
  customer appears.

## 2. What an ADAS OS has to show

| standard | what it asks |
|---|---|
| ISO 26262 (road vehicles, ASIL A to D) | a safety lifecycle; freedom from interference between elements of different ASIL; hardware fault metrics (SPFM, LFM, PMHF); for software at ASIL D, requirements traceability, MC/DC coverage, defensive programming, qualified tools |
| ISO 21448 (SOTIF) | that the *intended* function is safe, including perception limits. Mostly outside the OS |
| ISO/SAE 21434 | automotive cybersecurity; Solum's secure and measured boot and the vault map onto it |
| IEC 61508 | the parent standard (SIL 1 to 4), used outside automotive |

ADAS functions are typically ASIL B to D. Perception stacks are usually decomposed (ASIL D
= B(D) + B(D)), so no single complex component carries ASIL D.

## 3. The additional protections, and how Solum provides each

### 3.1 Freedom from interference in space

Already in the design: stage-2 per domain, IOMMU per device, static IOMMU domains, links
with offset addressing, sealing, and per-record epochs. Additions:

- **Static only.** In the safety profile no page, device or link moves after boot; no
  donation at run time; driver-domain restarts only for QM domains.
- **ECC** on DRAM and caches required (a hardware requirement; Solum checks it is on and
  reports corrected and uncorrected errors to the health monitor).
- **No shared mutable state** between a safety partition and any QM domain except its
  links.

### 3.2 Freedom from interference in time (the main new work)

The desktop design runs the main domain on all CPUs and lets driver domains borrow time.
A safety partition needs guaranteed time:

- **Reserved cores** for safety partitions, never shared with the main domain or QM
  driver domains. This avoids having to prove a scheduler and makes the timing argument
  short.
- **Time partitioning** where a core is shared between safety partitions: a fixed cyclic
  schedule (ARINC 653 style major and minor frames) in the manifest, enforced with the
  preemption timer or the Arm generic timer at EL2.
- **Cache partitioning:** Intel CAT, AMD L3 QoS, Arm MPAM cache portions. The safety
  partitions get dedicated ways, so the main domain cannot evict them.
- **Memory-bandwidth regulation:** Intel MBA, Arm MPAM memory-bandwidth limits, or, where
  those are missing, MemGuard-style regulation with performance-counter budgets on the
  QM cores. This is the one that can cost the main domain throughput under heavy load:
  the cap applies only while a safety partition is near its budget (§4).
- **Interrupt isolation:** no interrupt from a QM device lands on a safety core (IOMMU
  interrupt remapping, GIC affinity owned by Solum, both already in §4.3 and §4.4 of the
  spec).
- **Bounded Solum paths:** every hypercall and exit handler has a known worst-case
  execution time, with no unbounded loops and no allocation. This is a code-review rule
  for Solum from now on, and it costs nothing in the desktop build.
- **Shared devices:** none between safety and QM in the safety profile. A camera used by
  both goes to the safety side, which forwards frames to the main domain over a link.

### 3.3 Freedom from interference in communication

Links already bound and copy records. Add **end-to-end protection** on every link that
crosses a criticality boundary: a CRC, a sequence counter, a data ID and a timeout per
record (the AUTOSAR E2E profiles are the reference). The receiver detects corruption,
loss, repetition, reordering, delay and masquerade, as ISO 26262 requires. The cost is
a CRC per record, which is negligible.

### 3.4 Hardware fault detection

- **Lockstep** for the safety computation (§4).
- **Watchdogs:** an external hardware watchdog serviced only by the safety partition's
  health monitor, never by Solum alone and never by the main domain.
- **Self-tests:** the SoC vendor's software test libraries for cores and memory, run at
  boot and periodically in the safety partitions' own time.
- **Clock and voltage monitors:** SoC hardware. Solum only routes their interrupts to the
  safety side.

### 3.5 Safe state, degradation and health monitoring

- A **health monitor** in the safety partition that collects watchdog, ECC, lockstep
  mismatch, E2E error and deadline-miss events, and decides the reaction from a
  manifest table: restart a partition, degrade the function, or go to the safe state.
- The **safe state** for ADAS is usually "hand back to the driver and warn", reached
  through the safety partition's own outputs (CAN or Ethernet to the vehicle), without
  depending on the main domain.
- **Main-domain failure** is a QM event: the safety side keeps running and only loses the
  information it got from main (maps, the UI). This is exactly what Solum's crash
  protection already gives.

### 3.6 Boot

- The safety partitions start **first** and are up in a bounded time, before the main
  domain begins to boot (many ADAS requirements ask for a rear camera within about 2 s).
  Solum's boot already launches small domains before the main kernel (spec §4.7).
- Measured and secure boot as in the spec, which also covers ISO/SAE 21434.

### 3.7 Certification evidence

This is not a protection but the dominant cost:

- Solum's safety profile and the safety-partition executive developed under an ISO 26262
  process: hazard analysis, safety requirements traced to code and tests, MC/DC coverage,
  fault injection, a safety manual stating the assumptions users must meet.
- **Qualified toolchain:** Ferrocene (Rust, Apache-2.0/MIT) is qualified as a compiler for
  ISO 26262 ASIL D; its certified `core` subset reached ISO 26262 ASIL B / IEC 61508 SIL 2
  in 2025–2026 ([Ferrocene](https://ferrous-systems.com/ferrocene),
  [26.02 notes](https://ferrous-systems.com/blog/ferrocene-26-02-0/)). So Solum's safety
  code should be `no_std`, use only that `core` subset, and avoid `alloc`.
- Formal verification of the core invariants (page ownership, IOMMU and stage-2
  agreement, scheduling) with Kani or Verus would strengthen the case and shorten the
  testing argument; it is optional.
- An external assessor (TÜV SÜD, exida, or similar).

## 4. Lockstep: what it costs and where it fits

| approach | detects | cost | fits |
|---|---|---|---|
| **Hardware lockstep** (DCLS: Cortex-R52 safety islands; A78AE/A720AE split-lock) | random hardware faults in the core, cycle by cycle | the paired core does no other work; no software cost | the safety partition's core work on any ADAS SoC. First choice |
| **Solum software lockstep** (two domains on two cores, same inputs, outputs compared) | random faults in either core, and transient faults; not faults in shared memory or interconnect (ECC covers those) | 2x the protected task's CPU, plus a comparison per output (microseconds) | small deterministic control tasks on SoCs without enough lockstep cores |
| **Doer/checker** (a complex QM or ASIL B doer, a simple ASIL D checker) | wrong outputs from the complex part, whatever the cause, including software faults | the checker's CPU, usually small | perception and planning on GPUs or accelerators, where lockstep is impractical |
| **Diverse redundancy** (two different implementations) | systematic software faults as well | 2x development | the highest ASIL functions where the doer is software |

### 4.1 How Solum's software lockstep would work

1. The manifest declares a **redundant pair**: one task image, two domains on two cores,
   ideally in different clusters (to avoid common-cause faults), optionally with a fixed
   time offset.
2. **Input replication:** Solum delivers each input record (a sealed sensor frame, a CAN
   message) to both copies, with the same timestamp. Neither copy reads time, interrupts
   or shared state directly: the task must be **deterministic given its inputs**
   (single-threaded per frame, no wall clock, no uninitialised memory). That is a
   contract on the task, written into the safety manual.
3. **Output comparison:** each copy writes its output records to its own link. A
   **comparator** checks them byte for byte, within a deadline, before anything reaches
   the actuator link. A match means release; a mismatch or a missed deadline goes to the
   health monitor (restart the pair, degrade, or go to the safe state).
4. **Who compares:** best is a hardware-lockstep safety island (Cortex-R52), so the
   comparator itself is lockstepped. Without one, Solum compares, which puts Solum's
   comparator in the ASIL D path. That is acceptable only because Solum is certified in
   the safety profile anyway.

Cost: 2x the CPU of the protected task, a copy of each input (already sealed, so this
is a mapping into the second domain, not a copy), and a comparison per output. For a
control loop of a few hundred microseconds this is small. For the main domain it costs
nothing beyond the reserved cores.

Not lockstepped: the main domain, the GPU, and any non-deterministic workload. Comparing
a whole general-purpose OS's outputs in software would need deterministic replay of all
interrupts and DMA, at a cost far beyond "near native". No product does this, and it is
not recommended.

## 5. One flavour or two

| | `desktop` profile | `safety` profile |
|---|---|---|
| Solum source | same | same, with features compiled out |
| main domain (Atrium, jails, Fresco) | all CPUs | the cores left after reservations; same binaries, QM |
| domains | dynamic restarts, moves allowed by manifest | static; QM domains may restart, safety partitions follow the health-monitor table |
| scheduling | main pinned; driver domains borrow time | reserved cores, cyclic schedule for safety partitions |
| cache and bandwidth | best effort (optional partitioning) | partitioned and regulated |
| links | epochs, bounded records | plus E2E protection across criticality boundaries |
| warm restart of main | yes | yes (main is QM); never of Solum |
| crash dumps | debug policy only | never on a production vehicle |
| certification | none | ISO 26262 for Solum safety profile and the safety executive |
| hardware | any VT-x/AMD-V/EL2 machine with an IOMMU | an automotive SoC with ECC, lockstep cores and MPAM; arm64 |

So the answer is the same product with two profiles. The rule that keeps this cheap is
that **anything the safety profile needs that the desktop doesn't** (cyclic scheduler,
E2E, health monitor, MemGuard) is a separate `cfg` feature, and **anything the desktop
needs that safety doesn't** (dynamic moves, dumps, graphics domain) is compiled out of the
safety build. A certified build then contains only the code its evidence covers.

## 6. What to do in Solum now (nearly free)

1. Write Solum's core as `no_std`, with no `alloc` in hypercall and exit paths, and keep to
   the `core` subset Ferrocene certifies, where practical.
2. Bound every handler (no unbounded loops); add a WCET note to each handler's doc
   comment.
3. Leave room in the manifest for reserved cores, cache and bandwidth partitions, and a
   cyclic schedule, even if `desktop` doesn't parse them yet.
4. Reserve the E2E header fields in the link record format (a CRC, sequence number and
   data ID), unused by `desktop`.
5. Keep the safety-relevant core (memory ownership, stage-2/IOMMU agreement, interrupt
   routing) separable from the desktop features, so it could be verified on its own.

Out of scope until there is a partner: the safety executive, the health monitor, the
cyclic scheduler, software lockstep, MemGuard, the ISO 26262 process and the assessor.

## 7. Decision for Shankar

1. **Don't build the safety variant now;** keep Solum compatible with it (§6). *Recommend
   yes.* Revisit with an automotive partner and a target SoC (TI TDA4, Renesas R-Car,
   NVIDIA Orin or Thor, Qualcomm Ride).
2. **If built: one codebase, two profiles; the main domain stays QM;** hardware lockstep
   first, Solum software lockstep for small deterministic tasks, doer/checker for
   perception. *Recommend yes.*
