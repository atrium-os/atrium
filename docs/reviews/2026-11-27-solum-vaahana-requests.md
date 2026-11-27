# What Solum needs from Vaahana

**From:** the Atrium Solum workstream (spec: `docs/spec/solum.md` in atrium-os/atrium,
accepted 2026-11-27). **Date:** 2026-11-27.

Solum is a thin Rust type-1 hypervisor that runs underneath the Atrium (FreeBSD) kernel.
Shankar asked us to develop and test it on Vaahana instead of QEMU. Below is what Solum
needs from the emulator, ordered by the Solum build step that first needs it (S0 is a
hypervisor that does nothing, S7 is the arm64 port). "Must" means the step can't be
tested without it. Every item is about Vaahana as Solum's *development platform*; none
of these numbers will be quoted as product performance.

## 1. Must have, for S0–S2 (amd64 first)

1. **Hardware virtualisation exposed to the guest:** Intel VT-x with EPT, unrestricted
   guest, VPID, the VMX preemption timer, posted interrupts and APICv. AMD-V with NPT,
   ASID and AVIC later. Solum runs at VMX root inside Vaahana and starts the Atrium
   kernel as its own guest (L2). A clear "unsupported VMX feature" error beats silently
   wrong behaviour.
2. **EPT with 2 MiB and 1 GiB pages**, plus the EPT violation and misconfiguration exit
   information (guest-physical and linear address, access type). Solum maps the main
   domain with huge blocks.
3. **An IOMMU with interrupt remapping:** Intel VT-d (DMAR) with queued invalidation,
   superpages and interrupt remapping (scalable mode with first and second stages, if
   possible). AMD-Vi later. Faults have to be reported the way the hardware does, and an
   IOTLB invalidation wait has to actually wait.
4. **PCIe with ECAM, several IOMMU groups and ACS**, at least one device that supports
   FLR, and one that can't be reset. Solum passes whole groups through and resets on
   every move.
5. **An NVMe controller and a NIC** that DMA through the IOMMU (for Tessera, the gates, and
   Solum's debug-dump write path), and an **xHCI** controller with USB keyboard, storage
   and audio devices (for the first driver domain, S3).
6. **A GDB remote stub that can break into the guest at VMX root**, single-step it, set
   hardware breakpoints, and read memory both by guest-physical address and through a
   chosen EPT. Ideally it can also show the L2 guest's vCPUs while stopped in Solum.
7. **A serial UART** (16550) on a fixed port, separate from the main console, for Solum's
   own console and its debug stub.
8. **A dump-guest-memory equivalent** (the whole guest RAM plus vCPU state, taken while
   stopped), so a hung guest can be analysed offline (AGENTS.md).

## 2. Must have, for S2a (ACPI and sleep) and S4 (vault)

9. **ACPI with S3 and s2idle**: FADT with PM1a/PM1b control registers (or the
   hardware-reduced sleep control register), FACS with a waking vector, `RESET_REG`, port
   0xCF9, GPEs and `_PRW` wake from a USB keyboard. Solum traps the sleep and reset paths
   and owns the waking vector, so resume has to really go through the waking vector.
10. **SMM and SMIs**: the APMC port 0xB2 raising an SMI. SMM isolation (an STM or SMM
    Supervisor equivalent) would be a bonus. We test that Solum filters SMI ports.
11. **A TPM 2.0** (CRB on amd64, TIS on arm64), with its state in a file, PCR extends from
    the firmware, and the **TCG Memory Overwrite Request** bit honoured by the firmware
    (RAM wiped after an unclean reset).
12. **UEFI firmware with secure boot and measured boot**, enrolling our own keys, with an
    option to load or refuse unsigned option ROMs.

## 3. Must have, for S7 (arm64)

13. **Arm EL2 exposed to the guest** (nVHE is enough; VHE is a bonus), with stage-2
    translation, VMIDs, and the EL2 physical and virtual timers.
14. **GICv3** with distributor, redistributors and an **ITS**, trappable from EL2
    (ICH_HCR_EL2 traps including ICC_SGI1R_EL1). GICv4.1 direct injection would be a
    bonus.
15. **SMMUv3** with stage 2 (and ideally nested stage 1 + stage 2), event queue and
    command queue with sync.
16. **PSCI** (CPU_ON, CPU_OFF, SYSTEM_SUSPEND, SYSTEM_RESET) through SMC, which Solum
    intercepts.
17. The same GDB stub as item 6, able to stop at EL2.

## 4. Nice to have

18. **Intel PT and Arm SPE/TRBE models**, including the "PT uses guest-physical addresses"
    VMX control and MDCR_EL2 E2PB/E2TB, so we can test that Solum keeps trace output
    away from its own memory.
19. **Cache and bandwidth partitioning models** (Intel CAT/MBA, Arm MPAM) for the
    safety-profile compatibility work. Behaviour only, no timing accuracy.
20. **MBEC (Intel) / XNX (Arm) execute controls**, for the kernel text freeze.
21. **IPI virtualisation (IPIv)**, so we can test both the IPIv and the trapped-ICR path.
22. **Fault injection:** make a device ignore FLR, stall DMA, or raise an IOMMU fault on
    command; corrupt a DRAM word with an ECC event. This tests restart and crash paths.
23. **Deterministic record and replay** of a whole run, which helps reproduce races in
    Solum and the software-lockstep idea in the safety study.
24. **A host-side API** (a socket or a script hook) to send an NMI, press the power or
    sleep button, and read guest-physical memory, for check scripts.

## 5. How we'll use it

Each Solum step has a gate in `docs/spec/solum.md` §10, and the checks will run on
Vaahana first, then on the R9700 box for numbers. Exits per second, isolation probes
(a main read of a vault page must fault) and the 100-cycle sleep gate are all scripted
against items 6, 8, 9 and 24.

Questions for the Vaahana project:
- Which items exist today, and which are planned?
- Does Vaahana run on the Mac (arm64), on Linux (amd64), or both, and does it accelerate
  through HVF or KVM, or is everything emulated?
- Is nested virtualisation (item 1 or 13) emulated in software, or passed down to the host?
