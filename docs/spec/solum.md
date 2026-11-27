# Solum: a thin hypervisor for Atrium

**Status: proposed 2026-11-27.** Nothing here is built. Shankar asked: "thin hypervisor
over bare metal for Atrium without affecting performance. Can bhyve be this hypervisor?
Should we build one with Rust instead to tailor it for Atrium with jails?" The goals, in
his words: "crash protection, security, abstracting devices with standardised interfaces,
passthrough when required for performance with necessary safeguards", and "performance
should be near native, hence the 'thin' part".
**Owner:** this thread until accepted; then a new hypervisor workstream, with the kernel
thread for the fork changes.
**Related:** [gpu-security-model.md](gpu-security-model.md),
[fresco-f4-split.md](fresco-f4-split.md) §F4e, [fresco-recovery.md](fresco-recovery.md),
[tpm-boot.md](tpm-boot.md), [arca.md](arca.md),
[tessera-encryption.md](tessera-encryption.md), [kernel-hardening.md](kernel-hardening.md),
[jail-sysfil.md](jail-sysfil.md), [device-assignment.md](device-assignment.md),
[wifi.md](wifi.md), [bluetooth.md](bluetooth.md), [hospes.md](hospes.md),
[x86_64-port.md](x86_64-port.md), [r9700-bare-metal.md](r9700-bare-metal.md),
`docs/reviews/2026-11-07-linux-compat-scoping.md`,
`docs/reviews/2026-11-07-freebsd-kernel-security-rust.md`.

## 1. Short answer

- **bhyve cannot be the shipped hypervisor.** It is type-2: `vmm.ko` lives inside a
  FreeBSD host kernel and the device models are a C process on it. Under Atrium it would
  put a whole second FreeBSD kernel at the bottom (bigger, not thinner), it has no PCI
  passthrough on arm64 (15.0 added arm64 bhyve without it), and it cannot protect a guest
  from its host. It is still useful twice: as the **prototype** of this architecture on
  amd64 before any Solum code exists (§8, P0), and for Linux guests on machines without
  Solum.
- **Build Solum: a small Rust type-1 hypervisor tailored to Atrium.** Atrium (FreeBSD +
  our kmods + every jail) runs as the **main domain** at near-native speed. Solum owns only
  what it must: CPU virtualisation, memory ownership, the IOMMU, interrupt routing, and a
  tiny scheduler. It has no device emulation of its own.
- **"Thin" decides every device.** Each device class gets one of two modes (§5):
  - **Passthrough** to the domain that uses it, for the hot paths: the GPU and display, the
    NVMe disk, wired NICs. Atrium's native drivers run on the real hardware with IOMMU
    confinement, so the cost is the second stage of address translation and nothing else:
    no exits on I/O, interrupts delivered straight to the guest.
  - **A driver domain or driver process behind a standard interface**, for devices that
    are slow but crash-prone and parse hostile input: Wi-Fi (Aura and its firmware), USB
    host controllers, Bluetooth radios, audio codecs, card readers. The interface is not
    an emulated device: it is the protocol Atrium's own service for that class already
    speaks (Lyra rings, Tactus events, HCI, file operations, Ethernet frames), carried on a
    Solum **link** (§5.2). At these data rates the hop is not measurable; a crash restarts
    the domain and Atrium carries on. virtio stays for guests we don't write (the Linux
    lane). Study: `docs/reviews/2026-11-27-solum-device-interfaces.md`.
- **Graphics gets an Atrium-specific option:** the standard interface for the GPU is
  Fresco itself. A later **graphics domain** (frescod plus `atrium-gpu-amd` with the GPU
  passed through) keeps rendering native, and a GPU driver crash becomes a Fresco server
  restart that apps already survive by replay (fresco-recovery.md). No virtual GPU.
- **Security** comes from four things: drivers that parse hostile input leave the main
  kernel; the main kernel can drop most hardware drivers (it needs the link driver plus
  the passthrough drivers); a protected **vault** domain holds Arca's root keys and locked
  personas' keys out of the main kernel's reach; and Solum freezes kernel text and signs
  module loads.
- **Crash protection** at three levels (§6): a driver domain restarts alone; a graphics
  domain restarts and clients replay; a main-kernel panic is dumped by Solum and the main
  domain warm-restarts without firmware, with storage and display domains still up.
- **Jails, jaild, portcullisd, Tessera, Laminar and Fresco's protocol do not change.**
  Apps stay jails inside the main domain; nothing about the app model moves into VMs.
- **Recommendation:** accept the architecture, prototype its topology on bhyve on the
  R9700 box (P0, no new code), then build Solum on amd64 first (S0, a hypervisor that does
  nothing, with a stop rule if it misses the near-native gates in §9), then arm64.

## 2. Goals and non-goals

| goal (Shankar) | what it means here | how (section) |
|---|---|---|
| near-native performance | main-domain CPU, memory, GPU, disk and wired network within the §9 gates of bare metal | identity-like stage-2 with large blocks, passthrough for hot devices, no exits in steady state (§4, §5) |
| crash protection | a crashing driver does not take the desktop down; a main-kernel panic does not lose the machine's state silently | driver domains, graphics domain, Solum crash dump and warm restart (§6) |
| security | a compromised driver or main kernel reaches less | domains, smaller main kernel, vault, kernel text freeze (§7) |
| standardised device interfaces | the main kernel sees the same few interfaces on any hardware | Atrium class protocols on Solum links for every abstracted class, the same on machines without Solum; Fresco for graphics; virtio only for foreign guests (§5) |
| passthrough with safeguards | hot devices go direct, and cannot DMA, interrupt or reset their way out | IOMMU with interrupt remapping required, whole-group assignment, config-space filter, reset policy (§5.3) |

Non-goals: running several general-purpose OSes side by side as equals (a Qubes-style
desktop), live migration, overcommitting memory, and moving apps out of jails into VMs.

## 3. Options considered

### 3.1 bhyve

- **What it is.** `vmm.ko` (VT-x/EPT, AMD-V/NPT; on arm64 an EL2 world switch with stage-2
  and GICv3 emulation) plus `bhyve(8)`, a Capsicum-sandboxed C process holding every device
  model (virtio net/blk/9p/rnd/input/console, NVMe, AHCI, xHCI, HDA, framebuffer, TPM).
  PCI passthrough (`ppt`) on amd64 only. Jail support via `allow.vmm` (and
  `allow.vmm_ppt`, which must never reach an untrusted jail). arm64 and RISC-V arrived in
  [FreeBSD 15.0](https://www.freebsd.org/releases/15.0R/relnotes/), without passthrough.
  BSD-2.
- **As the bottom layer** the topology would be: a stripped FreeBSD host, Atrium as a
  guest with the GPU passed through, driver domains as further guests. Its limits for
  Shankar's goals:
  - *thin*: the host kernel is a full FreeBSD kernel, and every exit to a device model
    goes through it to a user process.
  - *crash protection*: any device the host keeps a driver for takes everything down when
    it crashes; avoiding that means passing every device to some guest.
  - *security*: the host can read every guest; nothing protects the vault from it.
  - *passthrough*: amd64 only; AMD GPU passthrough is fragile (reset), and arm64, our first
    architecture, has none.
  - *two schedulers*: the host's and Atrium's, with Atrium's vCPUs as host threads.
- **Verdict:** a good prototype vehicle on amd64 (P0) and the VM engine for Linux guests
  where Solum is not running. Not the product.

### 3.2 Other existing hypervisors

| candidate | licence | why not |
|---|---|---|
| Xen | GPL-2 | licence; dom0 model puts a full kernel in the TCB |
| pKVM, KVM | GPL-2 | licence; read for design only |
| seL4 (+ CAmkES VMM) | kernel GPL-2 | licence |
| Jailhouse, NOVA | GPL-2 | licence |
| Hafnium | BSD-3 | Arm only, built as an FF-A secure-partition manager next to TF-A; C |
| Bao, ACRN | Apache-2.0, BSD-3 | static partitioning for embedded / automotive; no dynamic memory or device moves; C |
| Cloud Hypervisor, Firecracker, crosvm, OpenVMM | Apache-2.0/BSD-3, Apache-2.0, BSD-3, MIT | VMMs: user processes on KVM, MSHV, WHP or HVF, not a bottom layer. Their virtio crates serve the Linux lane, and the virtio-net code the net link (§5.2) |

### 3.3 Why a tailored Rust hypervisor

- Atrium needs exactly one big domain at full speed and a few small ones; a general
  hypervisor carries scheduling, emulation and management we would switch off.
- The interfaces are ours already: the class protocols of Atrium's own services, Fresco for graphics, Aqueduct-style
  rings for the vault, jaild-style signed policy for which domain owns what.
- Rust by default for new kernel-level code (LANGUAGE-POLICY, D2); permissive licence.
- Both architectures from one design: EL2 on arm64, VMX root (then SVM) on amd64.
- Expected size: on the order of 15 to 20 thousand lines of Rust for the core, with the
  driver back ends living in domains, not in Solum.

## 4. Solum's shape

### 4.1 Layers

```
 main domain (Atrium)           driver domains            vault        graphics domain (later)
 FreeBSD + kmods + jails        net-wifi, usb, audio ...  (no_std)     frescod + atrium-gpu-amd
 link driver, passthrough       real driver + link back end
 drivers for GPU/NVMe/NIC
 ------------------------------------------------------------------------------------------
 Solum (EL2 / VMX root): stage-2 tables, IOMMU, interrupt routing, scheduler, hypercalls,
                         boot measurement, crash capture
 ------------------------------------------------------------------------------------------
 hardware
```

### 4.2 Memory

- Every physical page has one owner: Solum, a domain, or "shared between two named
  domains". Solum's own tables track it.
- The main domain's stage-2 is fixed at boot: its memory is a few large contiguous ranges
  mapped with 1 GiB (or 2 MiB) blocks, so the second-stage walk costs almost nothing on a
  TLB miss and Solum never takes a stage-2 fault in steady state. No ballooning, no
  overcommit, no demand paging by Solum.
- Small domains get fixed reservations at boot from a signed domain manifest (§4.6).
- A link's arena is pages shared by exactly two sides, named in the manifest; neither side
  can map anything else of the other (§5.2).

### 4.3 CPU and scheduling

- The main domain gets one vCPU per physical CPU, each pinned, so Laminar sees the real
  topology and keeps all its scheduling decisions. Solum never preempts it for timeslicing
  on its own account.
- Driver domains have one vCPU each, no pinned core. They run when there is work: an
  interrupt for their device or a link kick from the main domain. Solum runs them on the
  CPU that took the event, for as long as they have work, at higher priority than the
  main domain's vCPU there, then returns. This is the only scheduling Solum does: a
  per-CPU priority run queue with a budget per domain so a looping driver domain cannot
  starve the main one (it is stopped and restarted, §6.1).
- amd64: no external-interrupt exiting for the main domain, so Solum takes the CPU back
  for driver domains and watchdogs with the **VMX preemption timer** (armed only while a
  driver domain has pending work or a watchdog is due) and with **NMIs** it sends itself
  (NMI exiting on). Posted interrupts where the CPU has them. MSR and I/O bitmaps pass
  everything except virtualisation control, IOMMU and a few MSRs; VMX hidden in CPUID.
  **The interrupt-command register is not passed through**: with a direct x2APIC, main could
  IPI, INIT or NMI a CPU while it runs the vault or a driver domain. Main's ICR writes go
  through APICv's virtual APIC with **IPI virtualisation (IPIv)** where the CPU has it
  (Sapphire Rapids and later), and exit to Solum otherwise; on CPUs without IPIv that is
  one exit per IPI, which §9's `cargo build` gate has to absorb (it is measured, not
  assumed).
- arm64: nVHE, the main domain at EL1 with direct access to its own GIC redistributors and
  CPU interface. The **distributor (GICD) and the ITS are trapped and emulated** by Solum for
  the routing and enable state of interrupts that belong to other domains, so main cannot
  reroute or mask a driver domain's SPIs or LPIs; main's own interrupts go straight through
  the emulation's fast path. **SGIs (`ICC_SGI1R_EL1`, main's IPI path, the counterpart of
  the x2APIC ICR) are trapped** through `ICH_HCR_EL2` while any other domain runs on some CPU,
  so main cannot send an SGI to a CPU running the vault or a driver domain; with only main
  running, the trap is off. Stage-2 faults on those pages, HVC, and SMC (checked against a
  PSCI allowlist and forwarded) are the traps.
- So "no device emulation" holds, but Solum does emulate parts of the **interrupt
  controller** (GICD and ITS on arm64, the ICR without IPIv on amd64). Those emulations
  are small, fixed register sets and are part of Solum's fuzzed surface (§7.4).

### 4.4 Interrupts

Every interrupt source belongs to the domain that owns the device. Passthrough devices'
MSI/MSI-X go through IOMMU interrupt remapping straight to the owning vCPU (posted on
amd64, direct injection via GICv4 where present, list registers otherwise). Solum refuses
to pass a device through on a machine without interrupt remapping.

### 4.5 Hypercalls

The main domain's whole attack surface on Solum. A fixed, small set: share/unshare pages
with a named domain, kick a domain, query domain state, restart a driver domain the
manifest lets it restart, load a kernel module region (§7.3), read Solum's version and
measurement. Arguments are frame numbers and small integers, bounds-checked; Solum parses
no variable-length data at all. Driver domains get an even smaller set: kick, interrupt
ack, and DMA-map requests limited to their own pages.

### 4.6 The domain manifest

Which domains exist, their memory, which PCI functions each owns, which links join
them, and who may restart whom: one TOML file, signed with the Opifex base key role (which depends on the deferred key
ceremony: until then, development builds sign with a development key and say so),
measured by the loader with Solum (tpm-boot.md §5). It is rendered from validated tokens
like other Atrium policy files. The main domain cannot change it at run time; it can only
ask for the moves the manifest allows (for example "this USB device may go to the Linux
VM").

### 4.7 Boot

UEFI → Atrium loader → Solum (a separate image, measured into a PCR, verified under secure
boot) → Solum starts the vault and driver domains from images in the boot set → the main
domain kernel starts with stage-2 already on; it never runs unconfined. With
`solum.disable=1` on a dev boot, or a CPU without the extensions, the loader starts the
kernel directly as today: same kernel, with native drivers for everything.

### 4.8 Hardware virtualisation: what Solum uses

Solum is built on the CPUs' own virtualisation support; there is no other way to be thin.
Without it, the options are binary translation or a paravirtualised kernel, each slower
and far larger. Solum needs the base features, uses the rest when present, and the
machine runs without Solum when the base features are missing (§4.7).

| need | Intel | AMD | Arm | use in Solum |
|---|---|---|---|---|
| base (required) | VT-x, EPT, unrestricted guest | AMD-V (SVM), NPT | EL2, stage-2 translation, VMID | trap-free main domain on its own stage-2 |
| large stage-2 blocks (required for near native) | EPT 2 MiB / 1 GiB | NPT 2 MiB / 1 GiB | stage-2 block descriptors | main domain's memory mapped in a few huge blocks (§4.2) |
| IOMMU with interrupt remapping (required for passthrough) | VT-d with IR | AMD-Vi with IR | SMMUv3 (stage 2, or nested stages) | §5.3 safeguards |
| interrupt delivery without exits | APICv, posted interrupts | AVIC, x2AVIC | GICv4.1 direct vLPI and vSGI injection | passthrough interrupts straight to the owning vCPU (§4.4); link kicks on newer parts |
| tagged TLBs | VPID | ASID | VMID | no TLB flush on a domain switch |
| execute-only and per-mode execute for kernel text | MBEC | GMET | stage-2 XN per EL (FEAT_XNX) | §7.3 kernel text freeze |
| page-table protection | HLAT, paging-write | — | — | later: protect the main kernel's page tables (§7.3 left this out) |
| fast domain switch without an exit | VMFUNC EPTP switching | — | — | candidate for vault calls and link kicks; measure in S1 before relying on it (it has known side-channel caveats) |
| cache and memory-bandwidth partitioning | RDT (CAT, MBA) | QoS (L3 CAT) | MPAM | keep the vault and the driver domains from evicting the main domain's cache; also blunts cache side channels (§11) |
| confidential computing | TDX (Xeon) | SEV-SNP (EPYC) | CCA / RME (few parts yet) | optional: where present, the vault could run so that even Solum cannot read it. Not on client CPUs today, so not required |

What Solum deliberately does **not** use: nested virtualisation for the main domain (the
Linux lane's guests are Solum domains, not guests of guests), and the CPU's VM-exit-heavy
emulation paths (no emulated devices).

## 5. Devices

### 5.1 Per class

| class | mode | interface the main domain sees | why |
|---|---|---|---|
| GPU + display (R9700) | passthrough to main (S-phase); graphics domain later (§5.4) | the real device / Fresco | bandwidth, MES user queues and doorbells must be direct; near native is non-negotiable here |
| NVMe | passthrough to main | the real device | Tessera's I/O path; an extra hop would cost latency at NVMe rates |
| wired NIC | passthrough to main | the real device | throughput and latency; option: net domain if the NIC's driver is untrusted |
| Wi-Fi (Aura) | net driver domain | Ethernet-frame link (virtio-net header) | firmware and 802.11 frame parsing are hostile input; crash-prone; Wi-Fi rates are far below the link's ceiling |
| USB host controllers | usb driver domain | Tactus events for HID, Lyra rings for USB audio, file operations for sticks (FAT/exFAT parsed in the domain), HCI for Bluetooth, per-device USB transfers for jailed user-space drivers (CCID) | descriptor and file-system parsing from any plugged device; hot-plug faults |
| Bluetooth radio | with USB (it is USB) | as above | firmware loader and HCI parsing |
| HD audio / codecs | audio driver domain (or driver process) | Lyra stream rings and clock | crash-prone codec drivers; audio rates are tiny |
| cameras (USB UVC, MIPI on arm64 boards) | usb domain or a driver process | a **frame stream**: Lyra-style rings of sealed frame buffers (format, size, stride, timestamp on the shared clock), with control records for mode, exposure and focus | UVC descriptor and payload-header parsing, MJPEG left undecoded for the consumer's jailed decoder; a camera is a privacy device, so the stream opens only through a grant (an app capability plus an indicator drawn by frescod's trusted chrome lane, not by Forum, so no app or shell bug can hide it), never by default |
| SD / card readers, serial, sensors | driver domains or processes | file operations, byte streams | low rate, rare drivers |
| TPM | vault | none (main domain asks the vault) | keys (§7.2) |
| platform (timers, interrupt controller, power) | Solum / main direct | real hardware | performance and simplicity |

A "driver domain" is a minimal FreeBSD kernel from our fork (a new `ATRIUM-DD` config with
only that domain's drivers, no network stack unless it is the net domain, no jails, no
user accounts) plus one Rust back-end process serving the class protocol over a link. It
is for the C drivers we keep. A driver rewritten in Rust instead becomes a **driver
process**: a jailed user-space process in the main domain with the device's BARs mapped,
an IOMMU domain of its own that Solum programs, and interrupts as events. It restarts at
process cost and needs no guest kernel (study §6.1). Both kinds serve the same protocols,
so main's services need no code for either. Their protection differs: a driver process
confines its device's DMA and crashes cheaply, but sits on the main kernel's syscall
surface and in memory the main kernel controls, so it is not a boundary for hostile
input that a domain should parse (study §6.1).

### 5.2 Links, not virtio

Study: `docs/reviews/2026-11-27-solum-device-interfaces.md`. In short, virtio assumes a
trusted device side that can see all of the driver's memory, an emulated transport, and
rare resets. A driver domain is untrusted, Solum emulates nothing, and restarts are
routine. So:

- A **link** joins two sides named in the manifest. Its **arena** is a fixed set of pages
  shared by those two only: rings of fixed-size records (packed-ring layout and event-index
  suppression borrowed from virtio), and a data area addressed by **offset and length**,
  bounds-checked on every use. No physical addresses ever cross.
- **Bulk data** moves by copy (small records, network frames), by **seal** (the producer
  writes in place, then Solum makes the pages read-only to both sides until released, so
  nothing can change after it has been checked), or by **donation** (pages change owner).
  The thresholds are measured in S1.
- **Kick:** a doorbell hypercall that injects a virtual interrupt into the peer, suppressed
  while the peer is already busy.
- **Epoch:** Solum increments it when either side restarts. Everything in flight fails, and
  each class protocol says what it replays (as Fresco clients replay after a frescod
  restart).
- The **protocols** are Atrium's (study §5.3), registered as Aqueduct classes. On a machine
  without Solum the same driver service runs as a jailed process and serves the same
  protocol over a socket, so lyrad, tactusd, Vinculum and Capsa see one interface either
  way.
- **virtio** stays for guests we don't write (the Linux lane, §8), with back ends jailed in
  the main domain, where Atrium is the trusted side.

### 5.3 Passthrough safeguards

A device passes through only if all of these hold, checked by Solum at boot and on every
move:

1. An IOMMU with interrupt remapping is present and on (VT-d with IR, AMD-Vi, SMMUv3).
2. The whole IOMMU group goes to one domain (the R9700's group is its switch ports, the
   GPU and its HDMI audio function, x86_64-port §6), and ACS isolates the group, or the
   manifest pins the whole bridge.
3. DMA is mapped only to the owning domain's own pages; peer-to-peer and ATS are off
   unless the manifest names the pair.
4. Config space is filtered: BAR sizing and placement, the MSI/MSI-X capability and table,
   the expansion ROM, and the power-management and reset capabilities go through Solum;
   vendor capabilities pass.
5. Reset on every move and every domain restart (FLR, then secondary bus reset, then a
   per-device quirk); a device that cannot be reset is pinned to one domain for the boot,
   never moved.
6. Firmware the device loads comes from the signed boot set or the owning domain's own
   verified files (tpm-boot.md, gpu-security-model §6), never from another domain.
7. **No persistence through device firmware.** A domain that owns a device could rewrite
   its flash (NVMe Firmware Download/Commit, a NIC's EEPROM, a GPU's VBIOS or option ROM),
   and an option ROM runs in UEFI before Solum on the next boot. So: Solum filters NVMe
   admin commands on passthrough controllers only where it can (it cannot see a
   passed-through controller's queues), so the real controls are firmware ones. The loader
   requires signed option ROMs or turns option ROMs off (Atrium needs none: the console
   is efifb or the native driver); NVMe firmware slots are locked with the controller's
   own write-protect where it has one; and the vault's seal includes PCR 2 and 3 (option
   ROM code and configuration), so a changed option ROM opens nothing. A legitimate VBIOS
   or option-ROM update therefore needs the recovery key once (§7.2). **Left over:** a
   device whose flash can be rewritten from its own driver and that runs that firmware
   without UEFI measuring it (NVMe or NIC firmware) can keep a foothold *in the device*,
   with DMA confined by the IOMMU. That is in the threat model, not solved.
8. **IOMMU mappings for the main domain are static.** Main's passthrough devices get one
   IOMMU domain that maps all of main's memory, built at boot with superpages (1 GiB or
   2 MiB), mirroring main's stage-2 **permissions as well as its layout**: frozen kernel
   text, read-only data and module text slots (§7.3) are read-only or unmapped for main's
   devices, so a main kernel with a write bug cannot have a disk or NIC DMA over its own
   frozen code. Solum updates this domain at freeze time and on every module load, in the
   same step as the stage-2 change. Main's `busdma` then does no hypercall per map, which is
   what makes the §9 NVMe and NIC gates possible. Solum changes that table only when pages
   leave main or change protection (donation, a link arena, a **seal**: a sealed page leaves
   main's static domain as well as the producer's device domains) and invalidates the IOTLB,
   waiting for completion, before reporting the change done. Where the hardware has nested translation (VT-d scalable mode
   second stage, SMMUv3 stage 1 + stage 2), main may keep its own first-stage tables for
   in-guest isolation between its devices without exits. Driver domains and driver
   processes get their own small IOMMU domains.

### 5.4 The graphics domain (later)

frescod and `atrium-gpu-amd` move into their own domain with the GPU passed through.
Apps in the main domain still speak Fresco; the transport becomes shared pages plus a
kick instead of a Unix socket (Fenestra already proves the scene stream crosses a
boundary well). Rendering stays native, because the driver runs on the real GPU. What it
buys: a GPU driver fault or a GPU hang takes down the graphics domain only, and clients
replay their scenes (fresco-recovery.md §3). What it costs: one hop per scene update, not
per pixel. Gate it on the §9 frame-time numbers; until then the GPU stays in the main
domain.

## 6. Crash protection

### 6.1 Driver domains

Solum notices a crash (the domain's kernel panics and calls Solum, a watchdog on its
budget, or a vCPU stuck in a fault loop), captures its memory to a buffer, resets its
devices (§5.3 step 5), and restarts it from its boot image. Every link to it moves to a
new epoch; each record carries the epoch it was written under, and a record from an old
epoch is refused. A link's arena is not reused for the new epoch until the device that
could DMA into it has been reset, so stale DMA from the dead domain cannot land in new
buffers. The main domain's protocol clients see "epoch changed" and replay. Effects: Wi-Fi
drops and rejoins, a USB stick remounts, audio glitches; nothing else stops. Three crashes
in a minute and the domain stays down, and Curia says which device is gone (as for a
missing daemon today).

### 6.2 Graphics domain

As 6.1, then frescod's epoch increases and clients replay (fresco-recovery.md §2–§3). The
screen goes dark for the restart time.

### 6.3 Main domain

On a main-kernel panic or a hard hang (watchdog), Solum freezes the main domain's vCPUs,
resets the passthrough devices, and restarts the main kernel from the boot set without
going through firmware. The vault keeps running, so each persona's unlock still needs the
passcode after the restart, as at boot.

**Dumps are off by default.** A main-domain dump holds the keys and data of every unlocked
persona, so writing one anywhere would break the at-rest rule (AGENTS.md). Two modes:

- **Default:** no memory dump. Solum keeps a small fixed-size crash record (panic string,
  registers, backtrace frames, Solum's exit log), with no main-domain data pages, in its
  own memory, and hands it to the restarted kernel.
- **Debug policy** (set in the signed manifest, for development machines): the dump is
  encrypted to a **debug key** whose public half is in the manifest and whose private half
  is never on the machine, then written to a reserved dump partition by a **minimal NVMe
  write path inside Solum** (one admin queue and one I/O queue on the passthrough
  controller, after the controller reset, polled, writes only to the dump partition's LBA
  range). This is the one device driver Solum carries, and it exists only in builds with
  debug policy. It replaces today's "capture with QEMU's `dump-guest-memory` before
  resetting" (AGENTS.md) on real hardware.

If the GPU does not reset cleanly (x86_64-port §6), the restarted main domain comes up on
efifb, not the R9700, until the next full reboot; Curia says so.

## 7. Security

### 7.1 What moves out of the main kernel

The Wi-Fi stack and firmware loader, USB host controllers and every device's descriptor
parsing, audio codecs and card readers. The main domain kernel config can drop those
drivers on Solum machines, which matters: the 2026-11-07 review counted 44 advisories
reachable from app jails, and the driver share of them goes away or into a domain that
holds no secrets and can be restarted.

What does not change: jails still share the main kernel. The per-jail syscall and ioctl
filter (jail-sysfil.md) stays the defence there. Solum does not make apps into VMs.

### 7.2 The vault

A no_std Rust domain of a few thousand lines: key derivation, AEAD unwrap, signing, and
the TPM client, with the TPM's MMIO assigned to it. It holds Arca's user root key and
machine store key after unlock, the signing keys Arca uses but never releases, and each
persona's key-group wrapping key. arcad keeps policy, the courier and the IPC with apps
and talks to the vault over one shared ring of fixed-size records. The rule "apps use
secrets, never read them" extends to "arcad uses keys, never reads them".

- **Tessera:** when a persona unlocks, the vault unwraps its volume key for the main
  kernel (Tessera's crypto runs there). When it locks, the kernel wipes the key; only the
  vault can unwrap it again. A main-kernel compromise while a persona is locked gets
  nothing of that persona.
- **TPM:** on Solum machines the vault is the sole TPM owner (amends tpm-boot.md
  decision 1). Sealing binds PCRs that include Solum's measurement, so a boot without Solum
  cannot open the vault's keys.
- **Anti-hammering in the vault:** passcode checks happen in the vault, rate-limited by
  the TPM's dictionary-attack counter (tpm-boot.md §4.2), so a main kernel that holds the
  vault's ring still cannot guess passcodes faster than the lockout allows.
- **Booting without Solum** (`solum.disable=1`, a CPU swap, Solum removed) cannot open
  keys sealed to Solum's PCRs. Every persona key is therefore wrapped twice: once by the
  vault's sealed key, and once by the recovery key that tpm-boot.md §4.2 already requires.
  **Every** non-Solum boot asks for the recovery key; no arcad-owned seal for a
  `solum.disable=1` boot is kept on disk (one would let anyone who picks that boot entry
  open the keys without the vault), and the next Solum boot checks and deletes any such
  seal it finds. The same recovery path covers a changed PCR 2 or 3 after a GPU VBIOS or
  other option-ROM update (§5.3 item 7) and a manifest whose debug policy was turned on
  (§6.3): the manifest is measured, so changing it changes the PCRs. Setup and Curia say
  which mode the machine is in.
- **Honest limits.** An attacker who owns the main kernel:
  - can ask the vault to sign or unwrap while they hold the machine, as with a TPM;
  - sees the passcode the next time the person unlocks a persona, because input, Forum
    and Vestibulum run in the main domain, and can then have the vault unwrap that persona.
    "Locked personas stay locked" holds only until the next unlock on the compromised
    boot;
  - cannot copy the keys off, guess passcodes past the TPM lockout, or keep using the keys
    after a clean, measured reboot (with item 7 of §5.3's firmware residue as the
    exception). A trusted input path (a vault-owned keyboard and a secure display region)
    would close the passcode gap; it is out of scope here.

### 7.3 Kernel code integrity

After boot the main kernel asks Solum to freeze its text and read-only data in stage-2
(read-only and executable; everything else non-executable at EL1/ring 0). A kernel write
bug can then no longer patch code.

Signed module loads must not make Solum a parser of variable-length data (§4.5), and the
signature must cover what actually runs, after relocation. So modules are **linked at a
fixed address** at build time: Opifex's signing step relocates each `.ko` for its slot in a
reserved module region and signs the final image and its slot. At load time the kernel
copies the image into the slot's pages and makes one hypercall with the slot number and
the frame list. Solum hashes those pages (fixed size, page-aligned, no parsing), checks
the hash against a **signed module table** loaded and measured with Solum at boot (a
fixed-size array of slot, text length, rodata length, data length and hash records;
the hash covers the whole image). Then it freezes the module's text and read-only data
(read-only, executable for text) and leaves its `.data` and `.bss` pages writable and
non-executable. Modules not in the table never become executable. The costs: a module set
is fixed per base release, which already matches Opifex's signed base updates; and each
module image is tied to one exact kernel build and one load address, so the module region sits
at a fixed address per build and module addresses are not randomised on Solum machines
(no KASLR for modules; the same holds for the kernel image if its base is fixed for the
signed table).

Not compatible with the freeze, and off on Solum machines: DTrace `fbt` and `kinst`
probes (they patch kernel text), kernel live patching, and anything else that writes
code. DTrace `sdt` probes and other providers that do not patch text keep working. Debug
boots (`solum.freeze=0`) allow `fbt` for development. Kernel data and page tables stay
unprotected; HLAT-style protection (§4.8) is later work.

### 7.4 Solum's own surface

The hypercall ABI (§4.5) is fuzzed with cargo-fuzz on the handlers built for the host. So are the other inputs Solum reads: the firmware tables it parses at boot (ACPI DMAR, IVRS, IORT, MADT, and the device tree on arm64), PCI capability lists, and the interrupt-controller register emulations (§4.3). Each is variable-length firmware or device input, so each parser bounds every count and length before use and refuses rather than guesses.
Solum has no network, no file system and no parser of variable-length input. A debug
build logs to a serial port Solum owns; a release build exposes only its version and
measurement.

## 8. Interaction with the rest of Atrium

- **Jails, jaild, portcullisd:** unchanged inside the main domain. jaild gains
  `allow.vmm`-style grants only for the Linux lane's VM host, which on Solum machines asks
  Solum for a guest domain instead of loading FreeBSD's `vmm`.
- **dispensatord** (device-assignment.md) stays the policy point for which jail gets which
  device. On Solum machines it sees devices through their links (a stick is a file
  link the usb domain offers) and asks Solum only for moves the manifest allows.
- **Fresco / frescod / F4e:** unchanged in the S-phases. F4e's per-window VMs are GPU
  address spaces, unrelated to Solum domains.
- **Tessera:** unchanged on disk and in the kernel; its keys come from the vault (§7.2).
- **Arca:** splits into arcad (policy) and the vault (keys), §7.2.
- **Laminar:** unchanged; it schedules pinned vCPUs that map 1:1 onto CPUs.
- **Linux lane** (linux-compat-scoping §6): on Solum machines, Linux guests are Solum
  domains whose virtio back ends run in a jailed process in the main domain (bhyve's
  model, on Solum instead of `vmm.ko`); elsewhere, bhyve.
- **Kernel hardening:** `vmm` stays out of ATRIUM kernels (kernel-hardening.md); Solum
  machines do not need it.

## 9. Performance gates

Main domain under Solum against the same kernel and binaries on bare metal, at least 3
runs per arm, exits counted by a positive-controlled counter:

- CPU and memory: `lat_syscall` and app launch through jaild < 1 %; a large `cargo build`
  < 2 %.
- Tessera on passthrough NVMe: sequential and 4 KiB random I/O < 2 % throughput, < 5 µs
  p99 latency added.
- Wired network on a passthrough NIC: throughput and RTT within run-to-run noise.
- frescod on the R9700: p50 and p99 frame time on the noisy-neighbour and Navigator scroll
  runs, and GPU submit latency, within noise.
- Interrupt latency on passthrough devices: < 5 µs added at p99.
- Exits per second in steady state: near zero; a steady stream is a bug.
- Link classes (Wi-Fi, USB, audio, cameras): line rate of the device, audio latency within
  one period, input latency < 1 ms added.
- Switch costs, measured and counted against the gates above: the side-channel flushes on
  each switch into a driver domain or the vault (IBPB, L1D flush, VERW where the CPU needs
  them), the kick round trip, and how often the vault's SMT co-scheduling takes a sibling
  away from main (§11).
- IPI cost on CPUs without IPIv (§4.3).

## 10. Build order

| step | what | gate |
|---|---|---|
| **P0** | prototype the topology on bhyve on the R9700 box (amd64): FreeBSD host, Atrium as a guest with the R9700 and NVMe passed through, Wi-Fi/USB in a second guest over virtio (bhyve has no links) | the §9 numbers for passthrough vs bare metal, and the cost of a hop for the slow classes, measured before Solum code exists. No product code |
| **S0** | null Solum on amd64: boots from the loader, launches the unchanged kernel on large-block EPT, everything passed through | boots to frescod on the R9700; §9 CPU, NVMe, GPU gates. **Stop rule:** if missed and not fixable, stop lane S |
| **S1** | memory ownership, hypercalls, domain manifest, the link (arena, kick, epoch, seal, donate) with a test domain that echoes | a main-domain read of another domain's page faults (positive control first); copy vs seal vs donate and kick latency measured, thresholds fixed |
| **S2** | IOMMU ownership and §5.3 safeguards; main's static superpage IOMMU domain (§5.3 item 8), small domains for the rest; option-ROM and firmware-slot policy | a DMA to another domain's page faults; group, reset and config-filter checks; GPU gates still met |
| **S3** | first driver domain: usb (xHCI) serving Tactus, file and USB-transfer links; restart path (§6.1) | kill the domain mid-copy: the stick remounts, nothing else stops |
| **S4** | vault with the TPM (§7.2); arcad's vault ring | arca-vm-test.sh passes; vault memory unreadable from root in the main domain; sealing fails without Solum |
| **S5** | net domain (Aura Wi-Fi), audio domain | crash and restart gates per class |
| **S6** | kernel text freeze and signed module loads (§7.3); main-domain crash dump and warm restart (§6.3) | a text write from a test kmod faults; a forced panic dumps and restarts |
| **S7** | arm64 (EL2 nVHE) | S0–S6 gates under QEMU TCG with `virtualization=on`, then on arm64 hardware with EL2 |
| **S8** | graphics domain (§5.4) | frame-time gates; GPU fault restarts with client replay |

P0's numbers cover passthrough and the virtio hop only; the vault, seal, kick and
interrupt-emulation costs exist only in Solum and are first measured in S1 and S2.

Test platforms: amd64 on the R9700 box (Linux KVM nested VMX for function, native PXE boot
for numbers; x86_64-port §5). arm64 under cloud QEMU TCG for function only. The Mac dev VM
runs under HVF, which offers EL2 to guests only on M3 or later Macs with macOS 15+, and
QEMU's HVF nested-virtualisation support is still in review, so the Mac is not a Solum
platform yet.

## 11. Risks

- **Near-native depends on hardware features:** interrupt remapping, posted interrupts or
  GICv4, large stage-2 blocks, clean device reset. Machines without them run without Solum
  (§4.7), not slower.
- **GPU reset:** unknown whether the R9700 resets cleanly between domain restarts
  (x86_64-port §6); until known, the GPU is pinned to one domain per boot.
- **Availability:** a hostile main kernel can starve domains; Solum guarantees
  confidentiality and integrity of other domains, plus the driver-domain budget, not the
  main domain's fairness to them.
- **Side channels** between the main domain and the vault or a driver domain: flushes on
  each switch (IBPB, L1D, VERW) cost time on every kick, and keeping the vault's SMT
  sibling idle means Solum **does** preempt main's vCPU on that sibling while the vault
  runs. That is an exception to §4.3's "never preempts", limited to vault calls, and both
  costs are counted in §9. No SMT
  sibling sharing while the vault runs; it runs rarely.
- **Firmware** (SMM, PSP, ME) stays above Solum, as in tpm-boot.md.
- **Fork changes:** `dmar`/`smmu` in a static-domain mode for main, the module-load hypercall, the link driver, the `ATRIUM-DD` config. Small, but ours to carry.
- **Size of the job:** a hypervisor is a security product. It needs its own review thread
  pass and fuzzing before any default boot uses it.

## 12. Licences

Solum, the vault and the back ends are ours (BSD-2). Likely crates: `x86_64`/`x86`,
`aarch64-cpu` (MIT/Apache-2.0), rust-vmm `virtio-queue` and device crates for the Linux lane and the net link
(Apache-2.0/BSD-3), each through `scripts/licence-audit.py`. bhyve (P0 only) is BSD-2. GPL
hypervisors are read for design only.

## 12a. Review, 2026-11-27

The code review thread's adversarial pass (15 findings) is folded in: firmware
persistence and option ROMs (§5.3 item 7, seal to PCR 2/3), seal TOCTOU and bounded
release (study §5.1–§5.2), the vault's real limits, anti-hammering and the non-Solum boot
path (§7.2), static superpage IOMMU domains for main (§5.3 item 8), fixed-address signed
modules and the DTrace `fbt`/`kinst` conflict (§7.3), interrupt-controller emulation, the
preemption timer and IPIs (§4.3), dumps off by default with a debug-key mode and Solum's
own NVMe write path (§6.3), switch and side-channel costs in §9, driver processes stated
as weaker (§5.1, study §6.1), per-record epochs and arena reuse only after device reset
(§6.1), firmware-table fuzzing (§7.4), the camera indicator in frescod's trusted lane
(§5.1), the deferred key ceremony (§4.6), and P0's limits (§10).

## 13. Decisions for Shankar

1. **Build Solum, a tailored Rust type-1 hypervisor; bhyve is the prototype and the
   non-Solum VM engine, not the product.** *Recommend yes.*
2. **Two device modes:** passthrough with the §5.3 safeguards for GPU, NVMe and wired NICs;
   driver domains or driver processes for Wi-Fi, USB, Bluetooth, audio and other slow,
   crash-prone devices (§5.1 table). *Recommend yes.*
2a. **Links carrying Atrium's class protocols, not virtio, between Atrium and its own
   driver domains;** virtio only for foreign guests (§5.2 and the study). *Recommend yes.*
2b. **Driver processes** (jailed user-space drivers with a Solum-programmed IOMMU domain)
   for drivers rewritten in Rust; FreeBSD driver domains only for C drivers we keep.
   *Recommend yes.*
3. **Fresco is the graphics interface;** a graphics domain later (S8), no virtual GPU.
   *Recommend yes.*
4. **Apps stay jails in the main domain;** no app micro-VMs. *Recommend yes.*
5. **Vault domain owns Arca's root keys and the TPM on Solum machines** (amends tpm-boot.md
   decision 1). *Recommend yes.*
6. **Order: P0 on bhyve, then S0 on amd64 with the stop rule, arm64 at S7.** *Recommend
   yes.* Alternative: arm64 first, but no arm64 hardware we have can run it at speed today.
7. **Machines without the hardware features run Atrium without Solum,** with native drivers
   as today. *Recommend yes.*
7a. **Build on the CPUs' virtualisation support** (VT-x/EPT, AMD-V/NPT, Arm EL2/stage-2,
   with the IOMMU), using the optional features in §4.8 where present; confidential
   computing (TDX, SEV-SNP, CCA) only as an extra for the vault. *Recommend yes.*
7b. **Cameras are a frame-stream link, opened only through a grant** (§5.1). *Recommend
   yes.*
8. **Name: Solum** (Latin: ground, floor, the base the atrium stands on). *Recommend yes.*
