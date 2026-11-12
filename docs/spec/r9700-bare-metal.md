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

## 4.1 Stage 3 plan: PSP bootloader load

Sources: the hardware interface as Linux's MIT-licensed `psp_v14_0.c`, `amdgpu_psp.c`
and `mp_14_0_2_offset.h` describe it (register offsets, command codes, order). The
firmware is loaded as opaque blobs; only container headers are parsed (`rs/fw.rs`).

1. **Gate.** Only for MP0 14.x from discovery; anything else is refused by name and
   version. Skip if C2PMSG_81 != 0 (sOS already running). Never on gpusim.
2. **Buffers in VRAM.** The PSP needs a 1 MiB, 1 MiB-aligned staging buffer (later also
   the ring and fence buffers). We use VRAM through BAR0, amdgpu's
   `debug_use_vram_fw_buf` path, so no GART or system-memory DMA is needed yet. Its GPU
   address is `(MMMC_VM_FB_LOCATION_BASE << 24) + offset`, as the VBIOS programmed it
   (read by `atrium-gpu-probe --status`).
3. **Components, in order**, each from `psp_14_0_3_sos.bin`'s descriptor table, each only
   if present: KDB (0x80000), SPL (0x10000000), SYS_DRV (0x10000), SOC_DRV (0xB0000),
   INTF_DRV (0xD0000), HAD_DRV (0xC0000), RAS_DRV (0xE0000), IPKEYMGR_DRV (0xF0000),
   SPDM_DRV (0x20000000), then SOS (0x20000). For each: wait for C2PMSG_35 bit 31
   (bootloader ready), copy the blob to the buffer, write `mc >> 20` to C2PMSG_36 and the
   command to C2PMSG_35, wait for bit 31 again. After SOS: wait 20 ms, then for C2PMSG_81
   to change (sOS sign of life).
4. **Then** the KM ring (C2PMSG_69/70/71, command in C2PMSG_64, response bit 31) and the
   GFX command interface (TMR setup, then each IP's microcode through the PSP).
5. **Memory training** (PSP_BL__DRAM_*_TRAIN) is skipped: the VBIOS trained GDDR6 at
   POST (the discovery table was read from VRAM fine). Revisit if VRAM misbehaves.
6. **Failure = stop.** Timeouts leave the device unattached and vgapci-free; recovery is a
   reboot (the PSP's state is not resettable from the host without a full reset, which is
   stage 4 work).

The step runs as a separate, explicitly loaded test module first (`atrium_gpu_psp_test`),
not in the normal attach, so a hang cannot happen at boot.

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
  7.0.0, NBIF 6.3.1, VCN 5.0.0, DMU 4.1.0 (correct: the hardware reports 4.1.0 and amdgpu renames
  it to 4.0.1 for its DC code, hence the dcn_4_0_1 firmware name), 8 UMCs. It found two parser bugs that synthetic fixtures hid
  (table signature is "IPDS", VCN hw id is 12); the real table is now a golden test
  (`rs-host-tests/data/r9700-discovery.bin`).
- **2026-11-12, PSP/SMU state at power-on.** `atrium-gpu-probe pci 3:0:0 --status` (reads
  only, MP0/MP1 bases from discovery): PSP C2PMSG_35 = 0x80000000 (bootloader ready),
  C2PMSG_81 = 0 (no sOS), C2PMSG_64 = 0; SMU C2PMSG_90 = 1 (last message OK, from the
  VBIOS). So the driver must run the whole PSP bootloader sequence (KDB, SPL, SYS_DRV, SOS
  from `psp_14_0_3_sos.bin`) before anything else; nothing is loaded behind it.
- **2026-11-12, driver identifies silicon in-kernel.** `atrium_gpu_amd_pci.ko` (`639ddca`)
  loaded on the box and handed the R9700 with `devctl set driver -f vgapci0
  atrium_gpu_amd_pci`: it read the discovery table, printed GC 12.0.1, PSP/SMU 14.0.3, SDMA
  7.0.1, MMHUB 4.1.0, OSSSYS 7.0.0, HDP 7.0.0, NBIF 6.3.1, DCN 4.1.0, VCN 5.0.0, ISA gfx1201,
  and refused to attach (ENXIO, "silicon bring-up not implemented yet"). `devctl set driver
  -f pci0:3:0:0 vgapci` gave the device back; `kldunload` clean; no panic.
- **2026-11-12, stage 3 step 1: sOS up.** With `kenv hw.atrium_gpu.psp_bootload=1`, the base
  module (`fff21d6f`) loaded `psp_14_0_3_sos.bin` (linux-firmware, 435632 bytes, sha256
  28469a08…) into the PSP bootloader in 9 steps (KDB, SPL, SYS, SOC, INTF, HAD, RAS,
  IPKEYMGR, SOS), staging through VRAM+64 MiB = MC 0x8004000000. Afterwards C2PMSG_81 counts
  (sOS alive), C2PMSG_64 = 0x80c00000 (ring mailbox ready), C2PMSG_63 0xffffffff -> 2. No
  hang; device handed back to vgapci, module unloaded. The sOS keeps running until a cold
  boot, so later loads take the "already running" path. Next: KM ring, TMR, then IP
  microcode through the PSP.
