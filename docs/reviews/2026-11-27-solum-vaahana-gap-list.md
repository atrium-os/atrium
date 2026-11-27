# Vaahana's answer to Solum's requests (summary)

**Date:** 2026-11-27. **Source:** Vaahana's "Vaahana for Solum: gap list and plan"
(Vaahana main at e10b71b, code reading only, nothing run), which Shankar passed on. The
PDF is in the project files under `solum/vaahana-solum-gap-list.pdf`. It answers
[2026-11-27-solum-vaahana-requests.md](2026-11-27-solum-vaahana-requests.md).

## The main point

Vaahana passes nested virtualisation down to the host rather than emulating it:

- **amd64:** Vaahana's x86 KVM machine on the Hostkey box (i9-9900K). It relies on KVM's
  nested VMX: EPT with 2 MiB and 1 GiB pages, unrestricted guest, VPID and the
  preemption timer come from KVM. That CPU has **no APICv, posted interrupts or IPIv**,
  and KVM doesn't offer MBEC to L1. AMD-V needs an AMD host.
- **arm64:** HVF's EL2 on the M4 Max (macOS 15+, M3 and later). Not enabled in Vaahana
  yet. Still to be probed: whether nVHE is allowed, how the GIC behaves at EL2, and
  whether there is an ITS.
- The x86-on-Mac machine (translated by pptk) has no VMX and won't get one.

## Status per request

| # | item | status |
|---|---|---|
| 1 | nested VT-x | partial, unverified (KVM nested; needs nested=1 and nested-state reset on reboot); PI, APICv and IPIv impossible on the 9900K |
| 2 | EPT huge pages and exit info | partial, unverified (from KVM) |
| 3 | VT-d with IR | **missing**; the biggest item |
| 4 | PCIe groups, ACS, FLR | partial: ECAM, one host bridge, no root ports or ACS, no emulated FLR (VFIO R9700 does real FLR) |
| 5 | NVMe, NIC, xHCI | partial: virtio-blk and virtio-net only, nothing behind an IOMMU |
| 6 | GDB stub at VMX root | partial: per-vCPU stub, steps, breakpoints; no GPA or EPT-walk reads, no L1/L2 distinction |
| 7 | separate UART | partial: only COM1, which is the console |
| 8 | memory dump | missing |
| 9 | ACPI S3, PM1, FACS, GPE | partial: hardware-reduced FADT, 0xCF9 works; no PM1, FACS, S3 resume or wake |
| 10 | SMM, 0xB2 | missing (KVM supports SMM; needs OVMF with SMM) |
| 11 | TPM with MOR | missing (swtpm/libtpms are BSD-3) |
| 12 | UEFI secure and measured boot | missing on x86 (PVH/Linux boot only); arm64 EDK2 without secure boot |
| 13–17 | arm64 EL2, ITS, SMMUv3, PSCI over SMC, EL2 gdbstub | missing or partial |
| 18–24 | PT/SPE, CAT/MPAM, MBEC/XNX, IPIv, fault injection, replay, control socket | missing (replay not practical under KVM) |
| 25 | Solum backend | fits (the arm64 machine already splits device models from run loops); not built |

Vaahana's order of work: (1) nested VMX checks, COM2, a control socket and memory dump;
(2) the gdbstub additions; (3) OVMF with a full FADT/FACS; (4) PCIe topology, then NVMe;
(5) virtual VT-d; (6) S3, SMM, TPM, secure boot; (7) xHCI; (8) arm64 EL2 and friends;
(9) the Solum backend after S1, then fault injection.

## What it changed in Solum

- **Interrupt delivery (solum.md §4.4).** On a CPU without posted interrupts, external-
  interrupt exiting has to be on once driver domains exist, so every device interrupt
  exits. The spec said otherwise and is corrected: near-native interrupt rates need posted
  interrupts (or GICv4.1 on arm64), and §9 measures both kinds of machine.
- **The guest-domain API (§8.1)** records Vaahana's wishes for S1.
- **Test platforms (§10)** name the Hostkey box and the M4 Max's HVF EL2.

## Answers to Vaahana's open questions

- **UEFI or PVH?** UEFI. Solum is loaded by the Atrium loader under UEFI and measured
  there (solum.md §4.7). Vaahana's step 3 (OVMF) is therefore needed for S0 and should run
  alongside step 1, not after it. We'd rather not add a PVH entry to Solum just for the
  emulator.
- **An AMD box, or a newer Intel one?** A newer Intel machine with APICv, VT-d posted
  interrupts and ideally IPIv (Sapphire Rapids or later) is worth it, because the
  near-native interrupt gates can only be shown on such hardware, and the Hostkey box lacks it
  (the R9700 box's Coffee Lake Xeon very likely does too; to be confirmed). An AMD box (AVIC, AMD-Vi) can wait until the AMD-V port.
