# Atrium on the R9700 box: bare-metal bring-up

Status: in progress (started 2026-11-12). Supersedes the "never write the box's disk" plan in
[x86_64-port.md §5](x86_64-port.md#5-phase-e--native-boot-over-ipmi): the box was
reprovisioned with no OS, and Shankar chose dual boot (2026-11-12).

## 1. The box

Hostkey bare metal: Xeon E-2288G, 62 GB RAM, one disk, AMD Radeon AI PRO R9700 (Navi48,
GC 12.0.1, PSP/SMU 14.0.3, SDMA 7.0.1, DCN 4.0.1, 32 GB VRAM, 256 MB BAR without resizable
BAR). Managed through Hostkey's Invapi panel: the Console tab gives the IPMI KVM (Java or
HTML5, access lasts 2 h), Mount ISO + Boot from ISO works on Supermicro X9–X11 boards, and
own ISOs can be added by static URL (2 files, 7 GB each). Hostkey's terms forbid changing
IPMI/BIOS settings ourselves, so nothing here needs the BIOS: boot selection is done with
UEFI variables from the running OS.

## 2. Disk layout: dual boot

| partition | contents |
| --- | --- |
| ESP (1 GB, FAT) | Ubuntu's shim/grub, `EFI/atrium/loader.efi`, `EFI/atrium/net.conf`, `EFI/atrium/authorized_keys` |
| Ubuntu `/` (~40%) | Ubuntu Server 24.04: amdgpu oracle/bench runs, Orbis jobs |
| `atrium` (rest, GPT type freebsd-ufs) | Atrium root, UFS2 labelled `atriumroot` |

Ubuntu stays first in `BootOrder`. Atrium is booted **once** with `efibootmgr --bootnext`;
a hang, panic or IPMI power cycle returns the box to Ubuntu. Nothing can leave the box
without a working OS.

## 3. Install

1. Ubuntu Server 24.04 from the Invapi console: custom storage layout (ESP, `/` ext4 about
   40%, rest left free), OpenSSH, keys imported from GitHub.
2. Build the bundle in a cloud session: `scripts/r9700/make-image.sh` (stock 16-CURRENT
   `base.txz` + the fork kernel `ATRIUM-NODEBUG` + `overlay/` + `atrium-gpu-probe` + the
   atrium-gpu-amd modules, not loaded). Output `atrium-r9700-<date>.tar`: `root.ufs.zst`,
   `loader.efi`, `SHA256`.
3. On Ubuntu: `scripts/r9700/install-from-ubuntu.sh plan`, then `partition` (once),
   `install <bundle>`, `boot-once`, reboot. Watch the boot on the IPMI KVM.
4. `atrium_netbymac` (rc.d, before `netif`) mounts the ESP, finds the NIC by Ubuntu's MAC,
   names it `wan0` and gives it Ubuntu's address. The root image is the same for every box;
   only the ESP files are box-specific.

Updating Atrium later is `install <new bundle>` + `boot-once` again from Ubuntu (or from
Atrium itself once it can write the partition, which it should not while running from it).

## 4. GPU driver on silicon: stages

`atrium-gpu-amd`'s client half is hardware-neutral, but its hardware half was written
against gpusim's simplified model: firmware load, MES init, reset, per-VMID page-table
bases, HQD select and modeset/EDID use registers only gpusim has (`0x2xxxx`/`0x3xxxx` in
`atrium_gpu_amd.h`). Silicon needs the real sequence. Stages, each checked before the next:

1. **Boot.** Atrium up on the box, console on the KVM, ssh in. vgapci + efifb own the GPU.
2. **Read-only probe** (`atrium-gpu-amd/probe`, `atrium-gpu-probe`). Firmware headers
   (`fw`), the IP discovery table (`pci <dev> --vram-top` with no driver bound; on Ubuntu
   compare against amdgpu's view). Parsers are the kmod's own Rust (`rs/fw.rs`,
   `rs/discovery.rs`), host-tested in `rs-host-tests` and checked against the real
   gc_12_0_1/psp_14_0_3/sdma_7_0_1/smu_14_0_3/dcn_4_0_1 files.
3. **PSP and firmware.** Register bases from discovery, PSP bootloader (SOS sub-binaries
   from `psp_14_0_3_sos.bin`), TMR, then CP/MES/IMU/RLC/SDMA microcode through the PSP.
   Firmware is loaded as opaque blobs; nothing is disassembled (licence).
4. **GMC, IH, MES, first dispatch.** Real GMC/MMHUB, IH ring, MES API, the HQD dequeue
   handshake and TLB-invalidate ack (TODO in `cp.c` / `rs/queue.rs`), one compute dispatch
   checked against the oracle corpus.
5. **Acceptance.** `atrium_gpu_test` and `test_gpu_isolation` (82 checks) on silicon; this
   gates jaild's attestation of the driver.

Per-IP code is chosen by discovery version, not by device id (never hardcode RDNA4).

## 5. Log

- **2026-11-12, stage 1 done.** First bare-metal boot: bundle `03715a5` booted via
  `boot-once` in about 80 s; `wan0` = igb1 with Ubuntu's address; root grew to 554 GB.
  The R9700 is `vgapci0` at pci0:3:0:0 (PCIe Gen5 x16, BAR5 512 KiB at 0x98100000,
  256 MiB BAR0, plain MSI only); the boot console is the BMC's ASPEED (efifb 800x600),
  not the R9700. The BMC (ASUS, fw 1.13) sits on Hostkey's management network and is not
  reachable from the box.
- **2026-11-12, stage 2 discovery.** `atrium-gpu-probe pci 3:0:0 --vram-top` under Atrium
  (vgapci does not own the registers) read RCC_CONFIG_MEMSIZE 0x7f70 (32624 MiB) and the
  discovery table: v1.3, IP table v3, one die, 46 IPs, checksums good. GC 12.0.1, PSP/SMU
  14.0.3, SDMA 7.0.1 (two instances, GC's bases), MMHUB/ATHUB 4.1.0, OSSSYS 7.0.0, HDP
  7.0.0, NBIF 6.3.1, VCN 5.0.0, DMU 4.1.0 (the firmware files say dcn_4_0_1: compare with
  amdgpu's view on Ubuntu), 8 UMCs. It found two parser bugs that synthetic fixtures hid
  (table signature is "IPDS", VCN hw id is 12); the real table is now a golden test
  (`rs-host-tests/data/r9700-discovery.bin`).
- **2026-11-12, PSP/SMU state at power-on.** `atrium-gpu-probe pci 3:0:0 --status` (reads
  only, MP0/MP1 bases from discovery): PSP C2PMSG_35 = 0x80000000 (bootloader ready),
  C2PMSG_81 = 0 (no sOS), C2PMSG_64 = 0; SMU C2PMSG_90 = 1 (last message OK, from the
  VBIOS). So the driver must run the whole PSP bootloader sequence (KDB, SPL, SYS_DRV, SOS
  from `psp_14_0_3_sos.bin`) before anything else; nothing is loaded behind it.
