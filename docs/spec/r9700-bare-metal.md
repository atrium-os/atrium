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

## 4.2 Stage 3c plan: IP microcode through the PSP

Same sources as §4.1, plus `amdgpu_ucode.c` (which region of each file is an image) and
`gfx_v12_0.c` (what the driver waits for). Selected by discovered versions: GC 12.x,
SDMA 7.x, SMU (MP1) 14.x, PSP 14.x. Opt-in: `kenv hw.atrium_gpu.psp_ipfw=1` together
with `psp_bootload=1`.

1. **After the ring:** FB_FW_RESERV_ADDR / _EXT_ADDR (0x50 / 0x51), read only: where the
   PSP keeps its firmware at the top of VRAM, logged for the VRAM allocator. Then
   LOAD_TOC as before. No SETUP_TMR: PSP 14.0.x sets up its TMR at boot (amdgpu's
   `boot_time_tmr`); the TOC reply's TMR size 0 confirms it.
2. **Files** from `/boot/firmware/amdgpu/`, names from the versions:
   `smu_14_0_3.bin`, `sdma_7_0_1.bin`, `gc_12_0_1_{pfp,me,mec,uni_mes,imu,rlc}.bin`.
   amdgpu picks `*_kicker.bin` variants for a few boards by PCI id and revision; ours
   (rev c0) takes the plain files. Choosing kicker images without a device id is open.
3. **Plan** (`rs/ip_fw.rs`, host-tested on the real files): 20 images, in amdgpu's order.
   SMU first, then SDMA (TH0), RS64 PFP/ME/MEC, their stacks (MEC P0 and P1), unified MES
   code and data for both pipes, IMU I/D, RLC restore lists (GPM, SRM), RLC IRAM/DRAM,
   RLC_G last. About 3.7 MiB, staged 4 KiB aligned at VRAM + 68 MiB, one HDP flush.
4. **Load:** one LOAD_IP_FW (0x06) per image; any non-zero status stops. Then
   AUTOLOAD_RLC (0x21), and wait up to 2 s for `RLC_RLCS_BOOTLOAD_STATUS` bit 31 and
   `CP_STAT` = 0 (GC segments 1 and 0 from discovery).
5. **Once per cold boot.** If the RLC already reports bootload complete, nothing is sent.
   Reloading needs a mode-1 reset first (amdgpu resets on init when it finds the sOS
   running); that is stage 4 work. A failure part-way leaves the card for a power cycle.

## 4.3 Stage 3d plan: SMU bring-up

The stage 3c run left GC unpowered: `atrium-gpu-probe --status` (33b3972e) read every GC
register as 0, `GB_ADDR_CONFIG` included (never 0 on a powered GC). Only the always-on IMU
answers: `GFX_IMU_CORE_CTRL` 0x8 (core out of reset), `GFX_IMU_GFX_RESET_CTRL` 0x30
(`HARD_RESETB` still 0, so GFX is held in reset). amdgpu's PSP-load path never touches the IMU
on dGPU. Between the PSP step and GFX's wait for autoload it runs the SMU's `smu_hw_init`,
which is where GFX gets powered.

`smu.c`, opt-in `kenv hw.atrium_gpu.smu_init=1`, needs MP1 14.x. The mailbox is MP1 segment 1:
C2PMSG_66 message, 82 argument, 90 response; 1 means OK. The messages are those of
`smu_hw_init` / `smu_smc_hw_setup` on 14.0.2/3. No pptable write (14.0.2 has none), and no
SCPM-gated messages. In order:
- TestMessage, GetSmuVersion, GetDriverIfVersion
- SetDriverDramAddr High/Low (1 MiB at VRAM + 84 MiB)
- RunDcBtc
- SetAllowedFeaturesMask High/Low (all, as `smu_v14_0_2_init_allowed_features`)
- EnableAllSmuFeatures
- GetRunningSmuFeatures
- DisallowGfxOff, for bring-up, so GC stays readable

After that, the step waits for RLC bootload complete and logs `GB_ADDR_CONFIG`. It needs no
reload of the IP firmware, which the PSP already holds this boot. Until `GB_ADDR_CONFIG`
reads non-zero, all-zero GC registers mean "off", not "idle".

## 4.4 Stage 4 plan: GMC, IH, CP/MES, first dispatch

Gated on power P1 + P2 (done 2026-11-12: telemetry, GC idle sequence proven by telemetry,
mode 1 reset on detach). Every step below is another opt-in hook, runs after stage 3 +
the idle sequence, re-checks idle in telemetry, and ends in the mode 1 reset, so a box run
never leaves the card powered or hot. Each step logs its registers and is checked before
the next. Interfaces are Linux's MIT-licensed files, selected by discovered IP version:

| Step | What | Linux source | IP |
|---|---|---|---|
| 4a | **Hubs and GART.** FB location, AGP/system apertures, VMID0 GART page table in VRAM (inside the driver's area, clear of the PSP reservations at 0x7f3e00000 / 0x7d6c00000), L2 + TLB setup, context 0 enable, TLB invalidate on engine 17 with ack | gmc_v12_0.c, gfxhub_v12_0.c, mmhub_v4_1_0.c | GC 12, MMHUB 4.1 |
| 4b | **IH ring.** Ring in system memory through the GART, rptr/wptr, MSI-X; a self-test interrupt | ih_v7_0.c, amdgpu_ih.c | OSSSYS 7.0 |
| 4c | **GFX remainder.** Golden registers, constants (SH_MEM per VMID, compute VMIDs), RS64 config, gfxhub enable; CP stays halted | gfx_v12_0.c | GC 12 |
| 4d | **MES.** MES and KIQ pipes unhalted, MES ring, SET_HW_RESOURCES, the MES API for ADD/REMOVE_QUEUE; the HQD dequeue handshake and TLB-invalidate ack (`cp.c` / `rs/queue.rs` TODOs) | mes_v12_0.c, amdgpu_mes.c | GC 12 (MES 12) |
| 4e | **First dispatch.** One compute queue through MES, doorbell, EOP fence to the IH; result compared against the oracle corpus | gfx_v12_0.c compute path | GC 12 |

The gpusim-only registers in `atrium_gpu_amd.h` (0x2xxxx/0x3xxxx) stay gpusim's; silicon
gets its own per-IP files (`gmc12.c`, `ih7.c`, `mes12.c`), picked by discovery version.
After 4e, the driver stays attached on silicon for the first time, so P2's remaining parts
(the scoped GfxOff guard, the attached teardown: CP/MES quiesce, defaults, then the reset)
land with it.

## 4.5 Stage 5 plan: the driver attached on silicon, then acceptance

Through 4e every hook brings the card up, tests and resets it inside one attach that ends
in ENXIO. Stage 5 needs the driver to stay attached and serve clients, so the gpusim-shaped
half of the gpu module (`queue.c`/`rs/queue.rs` per-queue SIM block, SIM VMID select,
`regMES_INIT`, SIM firmware load, clear engine) gets silicon implementations behind the
same `atrium_gpu_backend_ops`, picked by discovered IP version. The front-end, the
isolation rules and the tests' ABI stay as they are. Display stays gpusim-only for now
(the box's console is the BMC VGA).

| Step | What |
|---|---|
| 5a | **Attached lifecycle.** Attach runs stages 3–4 (PSP, SMU, GC idle, GART, IH, GFX, MES) and stays up: GfxOff disallowed only while work is queued, telemetry watch, A3 rules. Detach: queues removed, MES and CP halted, PrepareMp1ForUnload, mode 1 reset. Box test: load, idle 60 s proven by telemetry, unload cold. |
| 5b | **Per-client VMs on silicon.** VMIDs 1–7 owned by the kernel (MES keeps 8–15); real GCVM_CONTEXTn page-table bases and the gmc_v12 PDE/PTE format; TLB invalidate with ack per VMID; UTCL2 faults from the IH into the existing VM-fault path. |
| 5c | **Queues: MES user queues with an Atrium interface** (decided 2026-11-12). MES schedules (priorities, time slicing, preemption, per-queue reset) and assigns VMIDs per process; the driver supplies each client context's page-table base. Interface: a queue is a file descriptor (submit right; SCM_RIGHTS / Capsicum; close = REMOVE_QUEUE); completion is a kqueue filter woken by the IH EOP; explicit timeline syncobjs only; the doorbell page is a revocable managed-pager mapping; per-uid/jail quotas on queues and VRAM; priorities from the power/posture federation; only the queue's fd reports device-lost on a hang. The kernel is the only MES client; map_legacy_kq stays for the kernel's own queues (scrub, self-test). |
| 5d | **VRAM.** The allocator's range clear of the PSP reservations; scrub-on-free through DMA_DATA on a kernel queue (the 4e path). |
| 5e | **Tests on silicon.** Dispatches that use gpusim's SIM_COMPUTE_KERNEL get a real-shader variant (oracle-built, like write_42); WAIT_REG_MEM parking works unchanged. |
| 5f | **Acceptance.** `atrium_gpu_test` and `test_gpu_isolation` (82 checks) on the box. |

Decided 2026-11-12 (Shankar): MES user queues, not kernel queues carrying the client's
VMID, because the hardware's scheduler is MES and fixed HQDs cap clients at a handful.
Not amdgpu's interface: the fd/kqueue/capability surface above. 5b changes with it: MES
owns VMID slots (8–15); the driver owns page tables per context. The isolation tests move
from "a queue shared between VMs" to "a queue fd can only reach its own context", tested
through fd passing, close and jail death.

### 4.5.1 Step 5c-2b plan: queue fds on MES user queues

5c-2a proved the client node on silicon over the kernel queue. 5c-2b replaces that path:
every client queue is a MES user queue, and the kernel queue carries only the kernel's own
work (self-test, scrub).

- **Queue fd.** `QUEUE_CREATE` on the client node takes a VM fd, an engine (compute first,
  gfx after), a priority class and flags, and returns a queue fd holding a reference on the
  VM. The backend adds the MES user queue then (5c-1's ADD_QUEUE: MQD and process/gang
  contexts in the GART, the VM's page-table base, VMID chosen by MES). The fd is the right to
  submit; it passes over SCM_RIGHTS and is limited by Capsicum rights like any fd. Its last
  close is REMOVE_QUEUE, after which its pages are freed. Per-uid (and per-jail) caps on live
  queues, alongside the BO caps.
- **Queue memory.** Ring, rptr/wptr writeback and EOP buffer are kernel-allocated system
  pages mapped into the client's VM at a kernel-reserved VA range above the BO window (the
  arena never hands it out). The ring and the wptr word are also mappable into the client's
  CPU address space from the queue fd (user mode only).
- **Kernel-mediated submit** (`QUEUE_SUBMIT` on the queue fd; ABI v2 §5.7). The kernel
  appends IB + RELEASE_MEM (fence + EOP interrupt) to the queue's ring, writes wptr and rings
  the queue's doorbell. The EOP IV of a MES user queue names its doorbell (src_data[0]), so
  the IV is routed to exactly one queue; the fence decides how much retires. The fence lives
  in the client's own VM, so a client can lie only about its own work: it can make its own
  syncobjs signal early, never another queue's, and never a free (below).
- **User-mode queue** (flag `USER_MODE`, client must be allowed user queues; ABI v2 §5.9).
  `QUEUE_MAP` on the queue fd returns mmap offsets of the ring, the wptr page and the
  doorbell page; mmap is on the queue fd itself (fo_mmap), so the capability is the fd, not
  the client node. One queue per doorbell page (the userqueue range, one page apart). The
  doorbell is a managed-pager object revoked (pages removed) on REMOVE_QUEUE, reset or close.
- **Frees.** A BO unbind writes the invalid PTEs and flushes the VMID with ack. For a VM with
  live user queues the backing is then held until MES has preempted that process's queues
  once (SUSPEND then RESUME of its gang), so nothing translated before the flush is still in
  flight; never on a fence value a client can write (ABI v2 §5.11).
- **Faults and hangs.** A UTCL2 fault IV names the VMID; the kernel asks MES which queue runs
  under it (or keeps the VMID-to-queue map from the HQD scan), kills that VMID's waves,
  resets the queue through MES (per-queue reset), marks only that queue fd lost
  (`ERR_FAULT`, its owed completions EIO, its doorbell revoked) and leaves every other queue
  running. A hang is a fence or rptr behind wptr for hang_ms: the same, with `ERR_HANG`.
- **Priorities.** The queue's class maps onto MES gang priority and quantum; the power/posture
  federation sets the class ceiling per uid/jail (5c-2b: fixed NORMAL; the mapping later).
- **Test.** si-client-test grows queue tests: two queues in two VMs at the same VAs; a queue
  fd passed to a child over SCM_RIGHTS submits, and stops working after the parent's
  REMOVE (close by all holders); a faulting queue loses its fd while the other keeps running;
  user mode: ring + doorbell from userspace, then revoke (SIGBUS after close).
- **Order of work.** (1) QUEUE_CREATE/QUEUE_SUBMIT/close on MES user queues with IV routing
  by doorbell, kernel-mediated submit only; the old SUBMIT path on silicon goes to EOPNOTSUPP.
  (2) faults/hangs per queue with MES queue reset. (3) user mode: fo_mmap of ring, wptr and
  doorbell, revocation. (4) the free rule. Each step is one box run.

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
- **2026-11-12, stage 3 step 2: PSP command ring.** `46affd4f` / `c9611fb9`: the KM ring
  is created (C2PMSG_64 0x80c20000) and destroyed cleanly, also a ring left by an earlier
  load. LOAD_TOC (2304 bytes) completes through the fence with status 0 and TMR size 0.
  With the response pre-filled with 0xdeadbeef, the PSP wrote status, session id and TMR
  size (all 0) and left the rest, so 0 is the PSP's answer: the TMR was set up at boot
  (`boot_time_tmr`), and no SETUP_TMR is sent. The sOS keeps running across loads.
- **2026-11-12, stage 3c run: PSP accepts all microcode, GC stays off.** `69d11bc0` with the
  box's linux-firmware files: all 20 LOAD_IP_FW and AUTOLOAD_RLC returned status 0. The RLC
  never reported bootload complete. The PSP firmware reservation is 50 MiB at VRAM
  0x7f3e00000 (top), plus 4 MiB at 0x7d6c00000; the VRAM allocator must keep clear of both.
  A read-only status probe then showed GC unpowered (§4.3).
- **2026-11-12, stage 3d first run.** `1e6fd692`: the SMU answers. TestMessage, GetSmuVersion
  (0x685000), GetDriverIfVersion (0x33), SetDriverDramAddr and RunDcBtc all returned OK.
  SetAllowedFeaturesMaskHigh returned 0xFD (bad prerequisite). Read that as SCPM: the PSP owns
  the feature mask, and amdgpu skips it when `scpm_enabled`. The next build carries on to
  EnableAllSmuFeatures. GC is still unpowered.
- **2026-11-12, stage 3 DONE: GC powered and booted.** With `3333d96c` the SMU listed no
  features running before the enable. It refused the allowed mask again (SCPM), and
  EnableAllSmuFeatures returned OK. Running features after the enable: 0x0488f19e38fffcfb.
  After DisallowGfxOff, the RLC reported bootload complete at once (0x8000003f), from the
  microcode the PSP had loaded earlier in the boot. Readings afterwards:
  - GB_ADDR_CONFIG is 0x08200545.
  - GRBM_STATUS is 0x382c.
  - RLC_CNTL is 1.
  - GFX_IMU_GFX_RESET_CTRL is 0x7f, so the resets are released.
  - CP ME, MEC and MES are halted, waiting for a driver.

  The working order on PSP 14.0.3 / SMU 14.0.3 / GC 12.0.1:
  1. sOS
  2. KM ring
  3. LOAD_TOC
  4. LOAD_IP_FW x20
  5. AUTOLOAD_RLC
  6. SMU features enable
  7. RLC boots

  Next is stage 4 (GMC, IH, MES, first dispatch). GFX stays powered, with GfxOff disallowed,
  until a power cycle.
- **2026-11-12, power P1 metrics read (55cb52b2): the card was at its limit while idle.** The
  first SMU metrics read decoded (0x2E offsets, which the 0x33 table matches; see
  [gpu-power-management.md](gpu-power-management.md) §B1) to:
  - 299 W socket and board power, against a 300 W limit;
  - hotspot 98 °C, edge 56 °C, memory 59 °C, VR 71 °C;
  - fan 3470–3649 rpm (73–76 %);
  - gfx 3191 MHz at 100 % busy;
  - throttlers active: 1 (hotspot) and 14 (PPT0).

  The CP was halted and CP_STAT was 0. The host PCH went from 28 to 42 °C with the CPU idle.
  AllowGfxOff was accepted, but nothing changed. The likely cause is that GC had been up
  since the 16:48 run with no gfx clock or power gating set up. The firmware's protections
  held it inside its limits. The box was powered off from FreeBSD at 17:02:52 to let it cool.

  The hex dump also printed 28 bytes past its buffer (fixed). Changes after this:
  - [gpu-power-management.md](gpu-power-management.md) rev. 2: a vendor-neutral contract with
    per-generation backends, rule A3.7 "the card must be able to idle", and the idle check;
  - `hw.atrium_gpu.gc_up_ok=1` is now required for any hook that powers GC up;
  - stage 4 also waits for power P2.
- **2026-11-12 17:25, power P2 step 1 box test (17d71224): mode 1 reset works.**
  - **Reset alone on a cold card:** PSP bootloader ready, no sOS, memsize 32624 MiB, GC off.
  - **Stage 3 from cold, then the reset** (`gc_up_ok=1`): all 20 IP images and the RLC
    autoload went as before. The features were 0x0488f19e38fffcfb after EnableAll. The
    telemetry showed:
    - right after the RLC booted: 13 W, gfx 0 MHz, edge 30 °C;
    - 1 s later: **262 W, gfx 3149 MHz, 100 % busy**, hotspot 59 °C, with the CP halted.

    GC powered up with no gfx init goes to full clock at once. The mode 1 reset then put the
    card back to post-POST: bootloader ready, sOS 0, GB_ADDR_CONFIG 0, RLC_CNTL 0. The PCH
    stayed at 28–30 °C for 5 minutes afterwards. Bring-up runs no longer need a power cycle.
  - **Open:** the first telemetry sample after the features are enabled read mem 3 °C and
    VR 6 °C, probably before the sensors settle.
  - **Next:** P2 step 2, the gfx init that lets GC idle while attached (CG/PG, CP).
- **2026-11-12 18:28, power P2 step 2 box test (efa15aa7): clock gating fixes the hot idle.**
  Stage 3 from cold, then gfx_v12_0's idle setup one step at a time:
  - **RLC up, no gfx init:** 246 W, gfx 3095 MHz, 100 % busy.
  - **SRM enabled:** 281–282 W, so SRM alone changes nothing.
  - **Clock gating on** (MGCG_OVERRIDE 0x7ff → 0x40, CGCG_CGLS 0x363f, CP idle interrupts on):
    283 W, then 1 s later **9 W, gfx 43 MHz, 3 % busy**, hotspot 73 → 37 °C.
  - **AllowGfxOff:** 7 W, gfx 0 MHz, 0 % busy. The gfx registers read 0 afterwards, so gfx
    was off.

  The mode 1 reset then returned the card to post-POST, and the PCH stayed at 29–30 °C for
  5 minutes. Clock gating plus GfxOff is now the default once GC is up (`gfx_cg=0` skips it).

  **Open:**
  - The first sample after the features are enabled reads mem 1 °C; the sensor isn't valid
    yet.
  - Throttler id 1 (hotspot) shows a small non-zero percentage at idle.
  - MetricsCounter jumps around (#1, #951, #905), so it is not a sequence number.
- **2026-11-12 18:44, idle proven by telemetry (0efb297d).** The family row "GC 12.0.0/1:
  SRM, clock gating" was picked. Idle check:
  - sample 1: 187 W, 87 % busy (gating hysteresis), not accepted;
  - sample 2: 7 W, 0 % busy, accepted ("card proven idle").

  The reset ended with "PSP bootloader ready, card cold" (sOS 0, GB_ADDR_CONFIG 0). The PCH
  stayed at 28–29 °C for 5 minutes.
- **2026-11-12 18:57, stage 4a passed (0e8eb372).** Layout: VRAM 32640 MiB at MC
  0x8000000000–0x87f7ffffff (FB offset 0); GART 512 MiB at MC 0; table at MC 0x8005800000.
  Both hubs (MMHUB 4.1, GFXHUB GC 12.0) went from CONTEXT0_CNTL 0x3fffc00 to 0x3fffc01, with
  PT base 0x5800001 (FB + 88 MiB, valid), L1_TLB 0x501 → 0x1859 and L2_CNTL 0x80602 → 0x80e01.
  Fault status was 0 before and after, and the TLB flush on engine 17 was acked on both hubs.
  After teardown the card was back at 7 W, 0 % busy, by sample 2. The gate passed, but the
  verdict was not logged; the check now logs it ("card proven idle after stage 4"). The reset ended with
  the card cold, and the PCH stayed at 28–30 °C for 5 minutes.
- **2026-11-12 19:11, stage 4a again plus 4b, IH self-test inconclusive (47128722).**
  4a repeated exactly. IH ring 0 was programmed (RB_CNTL 0x40610000 → 0x4013011d: enabled,
  64 KiB, wptr writeback, GPU VA space) and turned off cleanly. Writing the MP1 software
  interrupt produced no IV: WPTR and the writeback stayed 0, and IH_STATUS stayed 0xc0847
  (all idle bits, input included), so nothing reached the IH. Whether the host can raise that
  interrupt at all is unknown, and the GART write path is still unproven. **The card has no
  MSI-X capability, only MSI with 1 message** (pciconf). The driver now falls back to MSI, then
  to polling, and every IH client shares one vector (8f0c26c7). The IH is proven by the first
  CP EOP interrupt instead (4d/4e). Idle after stage 4 was accepted at 16 W, 15 % busy (below
  the 20 % and 25 %-of-limit thresholds). The reset left the card cold; PCH 28–30 °C.
- **2026-11-12 19:26, stages 4a–4c (362f9eef).** 4a and 4b as before. With MSI the vector
  was allocated (the card's one MSI message) but never fired, and the MP1 SW_INT register read
  0 after the host's write: the host cannot raise it, so that self-test is dropped as evidence.
  4c ran to "stage 4c: 0", but **the CP was not halted**. It came back from the GfxOff cycle
  with CP_ME_CNTL 0xa000 (no PFP/ME halt) and CP_MEC_RS64_CNTL 0x3c000000 (pipes active, no
  halt), whereas after the RLC autoload it was 0x1514a000 / 0x40030000. Nothing ran (CP_STAT 0,
  15 W), but 4c now halts the CP first, as amdgpu's cp_*_enable(false), verifies it and
  refuses otherwise. Other readings: RS64 start 0x7000000003000 in all three headers (checked
  per file); GB_ADDR_CONFIG 0x08200545, whose NUM_SHADER_ENGINES field decodes to 16, so it is
  an address-swizzle field, not the topology; the SA disable 0xff00 leaves 8 SAs (4 SE × 2);
  no RBs disabled; SH_MEM 0xc00c, bases 0x20001000 (VMID 1–7) and 0x10002 (VMID 8–15). Idle
  after stage 4: 15 W, accepted; reset left the card cold; PCH 28–30 °C.
- **2026-11-12 19:46, stage 4c with halt-first passed (8ab60114).** Before: CP_ME_CNTL 0xa000,
  CP_MEC_RS64_CNTL 0x3c000000. After the halt: 0x1400a000 / 0x40030000, the same as after the
  RLC autoload. After the RS64 start programming and pipe resets: 0x1400a000 / 0x40000000,
  still halted, and "stage 4c done; CP halted". 4a and 4b unchanged. Idle after stage 4:
  15 W, accepted. Reset left the card cold; PCH 28–29 °C. Stages 4a–4c are box-verified; the
  IH write path still waits for the first CP interrupt (4d/4e).
- **2026-11-12 20:05, stage 4d code ready (mes12.c), not yet run on the box.** Kenv
  `hw.atrium_gpu.mes=1`; needs 4c to pass. Order is now 4a, 4c, then 4b with 4d run inside it
  while the IH ring is live. Unified MES as amdgpu does it: KIQ = me 3 pipe 1; both pipes
  started at the uni_mes start address; pipe 1 HQD by register, SET_HW_RESOURCES(_1);
  pipe 0 mapped by ADD_QUEUE (map_legacy_kq) sent to pipe 1, then SET_HW_RESOURCES(_1),
  aggregated doorbells and QUERY_SCHEDULER_STATUS. Completion is polled in memory (fence +
  status), so 4d does not depend on interrupts. MES fence interrupts plus TIME_STAMP_INT on
  the MES pipes should give CP EOP IVs (client 0x14, source 181), which would be the IH write
  path's first proof. MES is halted again at the end; mode 1 reset as always.
- **2026-11-12 20:10, stage 4d on the box (6073c0fa): MES up, IH write path proven, idle refused.**
  Both MES pipes run firmware 0x1029097. SET_HW_RESOURCES, SET_HW_RESOURCES_1, ADD_QUEUE
  (scheduler, via pipe 1) and QUERY_SCHEDULER_STATUS all returned status 1 within 30–50 µs.
  12 CP EOP IVs arrived (client 0x14, src 181, rings 12/13, ctx 0xb1), the IH wptr went to
  0x180 with the writeback in step, and 1 MSI fired, so the IH write path is PASS. The MP1
  SW_INT miss is on the MP1 side. After stage 4 the card would not idle: 53–60 W, gfx
  3.4 GHz, busy 100 %. MES was halted but both MES HQDs were still active. The driver
  refused, the mode 1 reset left it cold, PCH 28–30 °C, no strays. Fix (next run): proper
  teardown as amdgpu's kiq_hw_fini does. That means REMOVE_QUEUE (unmap_legacy_queue) for
  the scheduler queue, dequeuing both MES HQDs by register, unmapped doorbells off and
  CPC_INT_CNTL restored, then halting. GRBM_STATUS, CP_STAT and RLC_STAT are logged at
  each step.
- **2026-11-12 20:23, stage 4d with teardown (84bd8b3f): teardown clean, still not idle.**
  REMOVE_QUEUE (scheduler) ok in 110 µs, both MES HQDs dequeued to inactive, MES halted.
  There were 14 CP EOP IVs. Power was still 13 → 52 → 60 W with gfx 3.4 GHz and 100 %
  busy, so the active HQDs were not the cause. RLC_STAT went 0x35 → 0x25 → 0 once MES
  started and answered, and stayed 0. GRBM_STATUS was 0xa800382c (GUI_ACTIVE, CP_BUSY,
  ANY_ACTIVE) after MES answered and after the halt. The driver refused and the reset
  left the card cold. Next run: RLC and clock-gating registers logged around MES, and the
  GC idle sequence (SRM + clock gating) re-run after the teardown.
- **2026-11-12 20:28, 7c536595: re-running the idle sequence after MES changed nothing.**
  Clock-gating state (MGCG_OVERRIDE 0x40, CGCG_CGLS(_3D) 0x363f, SRM 0x3, CP_INT_CNTL
  0x3c0000) was identical before MES, while it ran, after the teardown and after the
  re-run. RLC_STAT only bounces (live status). The sticky change is GRBM_STATUS 0x382c ->
  0xa800382c (GUI_ACTIVE, CP_BUSY, ANY_ACTIVE) as soon as MES answers, with CP_STAT 0, and
  it lasts past the halt. Power was 28 -> 60 W, gfx 3.4 GHz, busy 100 %; the driver
  refused and the reset left the card cold. Next: name the busy CP block (CP_CPC_STATUS,
  CPC/CPF busy, GRBM_STATUS2/3, RLC_GPM_STAT) and watch GRBM_STATUS for 500 ms with MES
  running, after the queues are gone, and after the halt.
- **2026-11-12 20:32, 6c90468a: the stuck block is the GFX command fetcher.** When MES
  answers, CP_CPF_STATUS goes 0 -> 0x84000001 (CPF_BUSY, CPF_GFX_BUSY,
  POST_WPTR_GFX_BUSY) and GRBM_STATUS2 shows CPF_BUSY. CPC stays 0. GRBM stays busy for
  500 ms at each point: MES running, queues removed, MES halted. Power 56 -> 60 W;
  refused; reset cold. Reading: given gfx_hqd_mask 0xfe (as amdgpu does), MES posts a GFX
  wptr, but PFP/ME are halted (4c), so it is never consumed. Next: gfx_hqd_mask 0 until
  the GFX ring stage; the CP stays halted.
- **2026-11-12 20:37, f4265f28 (gfx_hqd_mask 0): MES pipe 1 never started, so the
  question went untested.** 500 µs after the start, pipe 1 sat at instr 0x1 with GP3 0,
  while pipe 0 was normal. Its HQD was still activated, and the wptr post wedged CPF
  (0xad000001) and CPC (0xa0010000). SET_HW_RESOURCES got no answer, the dequeue timed
  out, and 32 IVs arrived from client 0xa (SOC21 GFX) src 0. Power was 62 W; the driver
  refused and the reset left the card cold. The mes12.c change between runs was only the
  mask, which is used after the start, so the start is intermittent. Fix (next run): wait
  up to 100 ms for each pipe's firmware version, and refuse before any queue is made
  active.
- **2026-11-12 20:41, 0e5974ad: the start gate worked; MES pipe 1 failed to start again.**
  Pipe 0 was up at once, but pipe 1 had no firmware version after 100 ms (instr 0x1 → 0).
  The driver refused before any queue was made active, nothing wedged, GRBM stayed idle,
  and the card was proven idle at 14 W on the first sample. So MES running with no active
  queues lets the card idle; the 60 W came from queue state. Both failing runs, and none
  of the good ones, logged 32 IVs from client 0xa (SOC21 GFX) src 0 ring 169, data
  0x3f0126..0x3f017f. The source change since the last good run (6c90468a) cannot touch
  the pipe start. Next: re-run 6c90468a unchanged as a control; if pipe 1 fails there too,
  try a cold power cycle.
- **2026-11-12 20:44, control: known-good 6c90468a now fails the same way.** MES pipe 1 does
  not start (instr 0x1, GP3 0), with the same client-0xa IV burst; the HQD wedges CPF/CPC;
  62 W, refused, reset cold. Nothing changed on the box (uptime since the 17:14 boot, no
  kenvs, firmware shas unchanged). Something that survives the mode 1 reset changed between
  20:31 and 20:36, after about 15 resets. Note: at "started", GRBM_STATUS2 reads 0x100000
  in the failing runs against 0x4100000 in the good one. Next: cold power cycle, then
  6c90468a as the first run.
- **2026-11-12 20:53, 6c90468a, first run after a panel reboot: neither MES pipe started**
  (instr 0, GP3 0). 112 client-0xa src-0 IVs arrived in pairs. Read as GFX UTCL2 faults
  (gmc_v12 SRCID 0; addr = data0 << 12 | (data1 & 0xf) << 44), they are at 0x3f01NN000 and
  0xfffff01NN000 for NN 0x10..0x7f. The second range sign-extends into the uni_mes data
  segment (data_start 0xfffffffff0100000). So the MES firmware seems to fault on its own
  data before running. Power was 66 W, refused, reset cold. Next (diagnostic, with the
  start gate): log each pipe's PSP-programmed IC_BASE, MDBASE, bounds and MSCRATCH, and
  both hubs' fault status, before the start and on failure.
- **2026-11-12 20:59, 45267702: both MES pipes started (same boot where 6c90468a failed),
  so the start is intermittent, not stuck card state.** The PSP placed MES sanely: IC_BASE
  0x7f461d000/0x7f469d000 and MDBASE 0x7f5500000/0x7f5580000, inside its 50 MiB
  reservation, with 512 KiB bounds and no hub faults before MES. There were no client-0xa
  IVs this time. With gfx_hqd_mask 0 the CPF still stuck busy (0x84000001, 60 W), so the
  mask was not the cause. Linux's gfx_v12_0_cp_resume (async gfx ring, the default) sets
  the doorbell ranges and un-halts MEC and PFP/ME, idle with no rings, before
  mes kiq_hw_init starts MES; we started MES with them halted. Next (a07d1fdd): do the same
  before MES, halt them again after, and set gfx_hqd_mask back to 0xfe.
- **2026-11-12 21:05, a07d1fdd: pipe 1 failed to start again; the fault is pinned down.**
  When it failed, the GFXHUB held a VMID 0 fault: status 0x40b53, addr page 0x3f017f.
  That ties the client-0xa src-0 IVs (pages 0x3f0110..0x3f017f) to UTCL2 faults from the
  non-starting pipe. 0x3f01NN000 is 0xfffffffff01NN000 truncated to 34 bits, i.e. the
  pipe's own data-segment addresses (MDBASE window 0xfffffffff0100000, bound 512 KiB) going
  through GPUVM, as if its local data aperture were not in effect yet. Before the start,
  IC/MD bases are sane and no hub faults are latched. After a start, LOCAL_BASE0 reads
  0xf0100000 on both pipes. CP running before MES (CP_ME_CNTL 0xa000, MEC 0x3c000000,
  CP_STAT 0) and halting it again afterwards both worked. With MES failed, the card was
  proven idle at 16 W. Next: retry the start up to 3 times (halt + pipe reset + icache
  invalidate between attempts), each attempt logged.
- **2026-11-12 21:11, 4286a4fc: stage 4d PASSED, and the card idles.** Both MES pipes were
  up on attempt 1, so the retry was not exercised. All APIs returned status 1, there were
  14 CP EOP IVs (IH write path PASS) and no GFXHUB faults. GRBM_STATUS stayed 0x382c and
  CP_CPF_STATUS 0 through MES running, queue removal and the halt. The card was proven
  idle after stage 4 on the first sample: 14 W, 241 MHz, 10 % busy. The reset left it
  cold. So the hot idle came from starting MES with MEC and PFP/ME halted; the fix is
  amdgpu's cp_resume order. Stages 4a–4d are box-verified. Still open: the intermittent
  MES pipe start (root cause unknown; the retry hides it), seen in 5 of 9 runs between
  20:36 and 21:05.
- **2026-11-12 21:15–21:17, 4286a4fc, three repeat runs: all PASSED 4d, card idle and cold
  after.** At 21:15:06 pipe 1 did not start on attempt 1 (instr 0x1, GP3 0, GFXHUB fault
  0x40b53 at page 0x3f017f, 32 client-0xa faults); attempt 2 brought both pipes up, so the
  retry path is now proven on silicon. 21:15:51 and 21:16:36 started first time. Tally for
  4286a4fc: 4 runs, 1 first-attempt failure, all recovered. Logs on the box in
  /root/runs/20261112T2115{06,51}Z.log and 20261112T211636Z.log.
- **Stage 4e built (kenv hw.atrium_gpu.dispatch, runner stage `4e`, needs 4d).** While MES
  is up: one kernel compute queue (MEC1 pipe 0 queue 0, VMID 0, doorbell 0x6) built as
  gfx_v12_0_compute_mqd_init and mapped by MES ADD_QUEUE (map_legacy_kq, compute, sent to
  pipe 0; pipe 0's MES compute mask becomes 0xe so MES leaves that queue alone). The ring
  holds INDIRECT_BUFFER to the oracle's write_42 IB (generated at GART 0x43000 / out
  0x45000 by atrium-rdna4-oracle/examples/kmod_write42.rs into write42_ib.h), then
  RELEASE_MEM with an interrupt. Pass = fence seen and all 64 words 42 (the golden),
  then REMOVE_QUEUE and a register dequeue; the block is leaked, not freed, if the queue
  will not go inactive.
- **2026-11-12 21:26, 2ce97c75: stage 4e PASSED, the first compute dispatch on silicon.**
  MES started both pipes on attempt 1. ADD_QUEUE (compute) on pipe 0 ok in 50 us; the
  queue went active with doorbell 0x60000018; the fence 0x4e000001 came back 30 us after
  the doorbell (rptr = wptr = 0xc); all 64 output words were 0x2a, matching the oracle
  golden write_42.r9700.txt. No hub faults. The IH saw 19 EOP IVs, one on ring 4 (me 1
  pipe 0 queue 0, the dispatch's RELEASE_MEM interrupt). REMOVE_QUEUE (compute) ok in
  140 us, HQD inactive. The card was then proven idle (13 W) and the reset left it cold.
  Log /root/runs/20261112T212618Z.log. Stages 3 and 4a–4e are box-verified; next is stage
  5 (atrium_gpu_test, test_gpu_isolation).
- **Stage 5a built (kenv hw.atrium_gpu.attach=1 with psp_bootload; runner stage `5a`).**
  Attach runs stages 3–4e as one bring-up (smu.c `amd_si_attach`, using the new up/down
  halves `amd_ih7_up/down` and `amd_mes12_up/selftest/down`), proves idle with MES up and
  GfxOff allowed, and stays attached (no gpu/display children yet). A telemetry watcher
  every 5 s takes the GPU down and resets it if the hotspot reaches 95 °C or the card is
  busy with nothing queued three samples running. Detach (and shutdown): GfxOff
  disallowed, the write_42 dispatch again (proves MES survived GfxOff cycles), MES/IH/GART
  down, idle proven, mode 1 reset. PrepareMp1ForUnload is not sent: the mode 1 reset after
  it returns the card to post-POST, which is stronger. Also in this build, from the
  public MES sources: each MES pipe gets a zeroed 256 KiB MSCRATCH area programmed before
  every start (mainline mes_v12_0_enable clears it because stale contents made the
  firmware fault at activation; kenv mes_mscratch=0 turns it off), GART page 0 is left
  unmapped (the IH ring moved to 0x1000), and the log says which start attempt worked.
- **2026-11-12 21:38, 44649060: stage 5a refused a healthy card.** SMU up, GC idle proven
  (7 W), then "attach failed (5)" before the GART: the new end-of-start check read
  GB_ADDR_CONFIG, which reads 0 once GfxOff has powered GC down. Reset left the card cold.
  Fix: that check removed (only a failed SMU link fails the start); every attach failure
  now names its step. Log /root/runs/20261112T213813Z.log.
- **2026-11-12 21:44, 24879d6c: stage 5a PASSED: the driver stayed attached on silicon.**
  attach rc 0; MES started on attempt 1 (MSCRATCH zeroed and programmed; the "before
  start" dump reads 0 because it precedes mes_enable); write_42 PASS at attach; the first
  idle sample after MES was 52 W / 83 % busy, the second 13 W / 0 %, so the gate waited.
  Attached 60 s: every 5 s sample 13 W, gfx 0 MHz, mem 193 MHz, busy 0 %, hotspot 35 to
  32 °C. Detach: write_42 PASS again after 55 s attached (MES survived GfxOff), queues
  removed, MES and CP halted, idle proven (13 W), mode 1 reset left it cold; give-back and
  kldunload rc 0, no hub faults, stray 0. Open: ~6 W more than unattached (7 W) because
  memory clock stays at 193 MHz while MES is resident (look at later). Log
  /root/runs/20261112T214441Z.log. Next: 5b.
- 2026-11-12 22:00, 5b built. gmc12.c now sets up contexts 1–15 on both hubs as
  gfxhub_v12_0_setup_vmid_config does (48-bit VA, depth 3, block size 0, faults to the
  default page, no retry, no base until bound) and builds per-client page tables in the
  gfx12 format: PDB2/PDB1/PDB0/PTB in a 256 KiB VRAM pool at 92 MiB, PDEs VALID with BFS 9
  in PDB1 entries, system-page PTEs VALID|SYSTEM|SNOOPED|R|W|X, MTYPE NC, invalid PTEs
  EXEC|IS_PTE; per-VMID bind and flush with ack; GFXHUB fault latch read and clear.
  Self-test at attach (kenv vm_test, runner stage 5b): VMID 1 with its own table runs the
  oracle's write_42 through the kernel compute queue (IB packet VMID 1), then (B) the same
  shader aimed at a VA only VMID 0 maps (a poisoned victim page; must stay poisoned and
  latch a VMID 1 fault there), then (C) the output page unmapped and flushed, same IB
  again (page must stay poisoned, fault latched). Interrupts for VM faults stay off for
  now (5c wires fault IVs to the owning queue). Faulting writes land in the shared dummy
  page; the test reports and re-zeroes it (5c: a per-client dummy or discard).
- 2026-11-12 22:00, 5b run c7dac886 FAILED (one run, card reset cold, no panic, stray 0;
  log /root/runs/20261112T215929Z.log). Everything through the VMID 0 dispatch passed; then
  the VMID 1 IB fetch at 0x1_0000_1000 hung with no fence in 1 s and GFXHUB latched a
  VMID 1 read fault there (status 0x1009ba: WALKER_ERROR 5, MAPPING_ERROR, CID 4; IV
  client 0xa vmid 1). Cause: I applied amdgpu's translate_further settings, but
  gmc_v12_0 sets translate_further only for GC 12.1. GC 12.0.x runs without it: no BFS
  in PDB1 entries, L2 bank select 9 / BigK fragment 6. The walker rejected the BFS 9
  PDE. Fix: translate_further is now a per-GC-version row field (false for 12.0), used
  for both the L2_CNTL3 values and the PDE format.
- 2026-11-12 22:10, 5b run a937a7a2 FAILED the same way (log /root/runs/20261112T220611Z.log):
  the same status 0x1009ba at the IB page, so translate_further was not the cause (the
  change stays: it is what amdgpu does on GC 12.0). Next kmod is diagnostic. It reads back
  the CONTEXT1 registers and L2_CNTL*, logs every level of the walk, decodes the fault by
  gc_12 field names, and tries four layouts in one run, each with a fresh queue: the
  amdgpu 4-level table in VRAM, a flat depth-0 table (the VMID 0 GART path, on VMID 1),
  the 4-level table in system memory, and the 4-level table with IS_PTE on the leaves.
  5b passes only on the amdgpu layout; the others show where the walker disagrees.
- 2026-11-12 22:15, 5b diagnostic run cb679b38 (log /root/runs/20261112T221309Z.log) found
  it. Same CONTEXT1 setup (read back: CNTL 0x3aaa807, depth 3, no retry, range 0..2^36-1),
  same PDEs; only bit 63 on the leaf PTE differs. Without IS_PTE on the leaf the walk
  faults (WALKER_ERROR 5 with tables in VRAM, 4 in system memory); with it, write_42 ran
  through VMID 1's own four-level table (64 x 42, fence in 20 µs, no fault), and the flat
  depth-0 table (GART-style PTEs, which carry IS_PTE) also passed. Valid leaves now always
  carry IS_PTE. Isolation held: the write to the VA only VMID 0 maps latched a VMID 1 TCP
  write fault at the victim page (status 0x141152) and the victim stayed poisoned. But
  the faulting wave then stays stuck (rptr = wptr, no fence, GRBM busy) instead of
  finishing on the dummy page. Next kmod: B and C pass on "fault latched for VMID 1 at
  the page, target untouched, queue recovers", where recovery kills VMID 1's waves with
  SQ_CMD (gfx_v11 soft recovery; gfx12 amdgpu has none) and waits for the fence. Only the
  four-level VRAM layout runs.
- 2026-11-13 01:20, **stage 5b PASSED** (e0bed369; log /root/runs/20261113T011936Z.log). VMID 1
  with its own four-level VRAM table (IS_PTE leaves): (A) write_42 64 x 42, fence 20 µs, no
  fault; (B) a write to a VA only VMID 0 maps latched a VMID 1 TCP write fault at the victim
  page (0x141156), the victim stayed poisoned, the stuck wave was killed with SQ_CMD and
  the queue fenced and went idle; (C) after unmap + VMID 1 flush, the revoked page latched
  a fault (0x141050) and stayed poisoned, recovered the same way. Then attached 30 s at
  13 W (gfx 0 MHz, mem 193 MHz, 0 % busy), VMID 0 dispatch PASS at detach, idle proven,
  mode 1 reset cold; give-back and kldunload rc 0, stray 0. Notes for 5c: no-retry faulting
  writes did not appear in the dummy page (dropped, so no cross-client channel there); two
  IVs from client 0xa with src 0xef (data 0x580000, 0x80 / 0x82) came with the B fault and
  are not yet decoded; the fault IV (client 0xa src 0, data = page, vmid) is the hook for
  "only the faulting queue's fd sees device-lost". Next: 5c.
- 2026-11-13 01:40, 5c split. Step 1 (5c-1) proves MES user queues on silicon from the
  kernel before the fd/kqueue interface (step 2) goes on top. Built: two processes, each
  with its own four-level VM and one compute user queue added with a plain ADD_QUEUE
  (mes_userq_map: process and gang context pages in the GART, quanta 10 ms / 1 ms, NORMAL,
  page-table base, VA 0..2^36-1 pages, vm_context_cntl from CONTEXT8_CNTL); the MQD is the
  user-queue form of compute_mqd_init (no PRIV/KMD, ring/EOP/rptr/wptr at VAs in the
  process's VM), wptr page aliased in the GART for MES; doorbells 0x20/0x22 (userqueue
  range). Both map write_42 at the same VAs onto their own pages. Pass: both fence, each
  out page all 42, different MES VMIDs (8-15, read from the HQDs), no HQD active after
  REMOVE_QUEUE. Runner stage 5c1 (5b's test runs first).
- 2026-11-13 01:35, 5c-1 run bdddc2dc (log /root/runs/20261113T013353Z.log): 5b PASS again;
  both MES user queues added, both fenced, both out pages all 42 from the same VAs onto
  their own (differently poisoned) pages, no faults, both removed cleanly. Failed only
  the VMID check: right after ADD_QUEUE only process 0 was resident (me1 pipe 1 queue 0,
  VMID 8); MES maps a user queue onto an HQD when its doorbell brings work. Fix: read the
  VMIDs again after both ran (queues stay mapped until REMOVE_QUEUE).
- 2026-11-13 01:38, **stage 5c-1 PASSED** (be750894; log /root/runs/20261113T013722Z.log).
  5b PASS again; two MES user queues in their own VMs at the same VAs both fenced with
  64 x 42 on their own pages; after both ran MES had them on two HQDs (me1 pipe 1 queue 0
  VMID 8, me1 pipe 0 queue 1 VMID 9); no faults in the user-queue phase; both removed,
  no HQD left. Attached 30 s at 13 W, dispatch PASS at detach, mode 1 reset cold, stray 0.
  Next: 5c-2, the queue fd / kqueue / doorbell-mapping interface on top.
- 2026-11-13 02:20, 5c-2 split. **5c-2a** puts the client node on silicon: the gpu module
  (atrium_gpu_amd.ko) attaches as the base's child when kenv `hw.atrium_gpu.client=1`, and the
  unchanged Rust front-end runs over a silicon backend (si_client.c): VM fds on kernel VMIDs 1–7
  with their own four-level tables (5b), System BOs (VRAM BOs wait for 5d), compute SUBMIT, and
  the IH ring's IVs routed to the front-end (ih7.c consumer in the interrupt thread). As an
  interim path SUBMIT runs on the kernel compute queue (4e/5b: VMID 0 ring, IB under the VM's
  VMID, fence in VMID 0, so a client cannot forge its completion); the front-end retires a
  signalled submit only when that fence passes. UTCL2 fault IVs fail the faulting submit (EIO)
  and the watchdog stops it: wave kill for the VMID, then the fence (5b), else the queue is
  mapped afresh. GfxOff stays off while any VM exists. Userspace test tests/si_client_test.c
  (static cross-build, scripts/r9700/build-si-client-test.sh; IB from the oracle,
  examples/si_client_w42.rs, patched for kernel-chosen VAs): dispatch with kqueue completion,
  two VMs at the same VAs, a cross-VM write that must fault with EIO and leave the other VM's
  page alone, recovery, refusals. Runner stage 5c2. **5c-2b** then moves SUBMIT onto MES user
  queues per the 2026-11-12 decision (queue fd = MES user queue, doorbell mapping, device-lost
  on the queue's fd), after which client work no longer runs on the kernel queue.
- 2026-11-13 02:00, 5c-2a run 4f9c4f21 (log /root/runs/20261113T020027Z.log): the client node
  came up on silicon (/dev/atrium-gpu0, amd-silicon backend, kernel queue at GART 0x70000).
  si-client-test 9 of 11: dispatch with kqueue completion in 0 ms; two VMs at the same VAs each
  64 x 42 on their own page; the cross-VM write faulted (TCP, VMID 4, write, VA 0x10002000),
  the fault IV failed the submit with EIO, the watchdog stopped the queue (wave kill, fence
  after 100 us) and the other VM's page kept its poison; the other VMs then ran clean.
  Failures were the test and an errno, not the hardware path: (1) the faulted VM's next job
  reused the syncobj that had failed, whose EIO is sticky by design (ABI §5.6), so its wait
  returned at once; the test now gives it a fresh syncobj. (2) a VRAM BO was refused with
  EDQUOT (0 % of no VRAM) instead of EOPNOTSUPP; the front-end now refuses VRAM BOs with
  EOPNOTSUPP when the device has no VRAM heap. Idle with the client node attached: 7 W
  (GfxOff allowed while no VM exists). Detach clean, mode 1 reset cold, stray 0.
- 2026-11-13 02:07, **stage 5c-2a PASSED** (c540be71; log /root/runs/20261113T020625Z.log).
  si-client-test 11 of 11: dispatch with kqueue completion; two VMs at the same VAs on their
  own pages; the cross-VM write faulted (TCP, VMID 4) and failed only that submit (EIO), the
  other VM's page untouched; the faulted VM (fresh syncobj) and the others then ran clean;
  graphics, VRAM BOs and user-mode queues EOPNOTSUPP. vm_faults counts fault IVs (2 for the one
  event). Idle with the client node attached 7 W; detach clean, dispatch PASS at detach, mode 1
  reset cold, stray 0. Next: 5c-2b, client work on MES user queues behind queue fds.
- **2026-11-13, 5c-2b step 1 first run: FAIL (13/21), box left busy.** `6e5aafbf`, log
  `20261113T022617Z`. Every user queue faulted as the CPF fetched its ring (client CPF,
  PERMISSION_FAULTS 0x8, RW 0): the ring PTE was read-only without EXECUTABLE, which a CP
  fetch needs (5c-1 mapped every page RWX). No user-queue job ran, so EOP routing by doorbell
  is untested (the EOPs seen, doorbells 0x2/0x3/0xb, were other queues'). The fault was
  misattributed (kernel-queue VMID logic), so jobs timed out. At close, wave kill did not
  help a CPF fault; MES refused REMOVE_QUEUE (status 0x8100030100000000) for the three
  mapped queues, the driver leaked them as designed, detach/unload got EBUSY and the card
  sat at 62 W, 3.4 GHz busy until a reboot. Fixes: ring/queue pages executable; a fault on
  a MES VMID fails that queue (EIO, later submits refused); close resets a wedged HQD
  (CP_HQD_DEQUEUE_REQUEST 2) before REMOVE_QUEUE and retries once after a refusal. Last rung
  (Shankar, 2026-11-13: reset first): if MES still keeps a queue, the card gets a mode 1
  reset and bus mastering goes off for good (`amd_si_lose`), every client queue fails with
  EIO, and the queue's memory is freed, so detach proceeds and the card never sits hot.
- **2026-11-13, stage 5c-2b step 1 PASSED (21/21).** `483b4baf`, log `20261113T023850Z`.
  Queue fds on MES user queues end to end: write_42 on a queue of its own (twice, out_seqno
  from 1), two VMs with a queue each at the same VAs, two queues in one VM, a queue fd passed
  over SCM_RIGHTS to a child that submits, a close in flight (cancelled; the VM then runs on a
  new queue), and all refusals. Every ADD_QUEUE and REMOVE_QUEUE returned ok (a close with
  work in flight: wave kill, fence, REMOVE ok); 0 VM faults; 12 BOs deferred behind user
  queues and all 12 reaped; attached idle 13 W; detach proven idle, card cold after reset.
  EOP routing by doorbell did not match: every user-queue EOP's src_data[0] named no queue
  ("routed 0"), and completions were retired by the check-all fallback (IV-driven, poll 0).
  Next: decode what a MES user-queue EOP carries (ring_id / me-pipe-queue) and key on it,
  then step 2.
- **2026-11-13, stage 5c-2b step 2 PASSED (29/29).** `22191091`, log `20261113T024533Z`.
  EOP routing by INT_CTXID works: each user queue's EOP carries src_data[0] =
  0x1000000 | doorbell (0x1000020..0x1000026) and retires only that queue. A faulting queue's
  work fails with EIO at once, the queue refuses more, is evicted (wave kill, fence, REMOVE
  ok) and other VMs run on; the VM runs clean on a new queue. A queue hung in WAIT_REG_MEM
  (65 W, 100 % busy while hung) fails with EIO after 5049 ms (hang_ms 5000); its HQD reset
  (DEQUEUE_REQUEST 2) went inactive in 10 us and REMOVE_QUEUE was ok; then 7 W idle. No
  REMOVE refused, no last-rung reset; vm_faults 1, hangs 1, 16 deferred / 16 reaped; detach
  clean, card cold. The remaining unrouted EOPs (src_data 0x20000002/3/b) are MES's own
  pipes; they should not count as misses. Next: step 3 (user mode: mmap of ring, wptr and
  doorbell from the queue fd, revocation), then step 4 (the free rule).
- **2026-11-13, stage 5c-2b step 3 built (box run pending).** User-mode queues
  (`USER_MODE`, root only for now): `QUEUE_MAP2` ('A',82, v2 layout plus a proposed
  `out_fence_gpu_va`) and `fo_mmap` on the queue fd map the ring, the writeback page and
  the queue's own doorbell page (BAR page 2 + slot; page 1 is MES's aggregated doorbells,
  registered fictitious at MES init) through one managed pager object. Ring and writeback
  are kpi `RamPages` (patch 0012: wired, managed, object-less pages a mgtdev pager may
  insert); the last close revokes every mapping first, then removes the queue. Writeback
  page: rptr report, wptr, fence, emitted; the CP's rptr is a fetch pointer (a faulted IB
  showed it past its RELEASE_MEM), so the user-mode hang clock is fence short of emitted,
  or rptr short of wptr, with neither moving for hang_ms. Lost queues: QUEUE_MAP2 EIO, no
  new pages mapped; mappings already made stay until close (the queue is off the GPU, its
  doorbell page belongs to no one else). Cleanups: MES's own EOPs (0x20xxxxxx) counted
  apart, not as unrouted; `vm_faults` counted once per fault (it was counted in both
  si_client.c and amd_fault_handler); the dummy-page line says whether a write landed.
  Test 9 in si-client-test (the step-1 user-mode refusal is gone).
- **2026-11-13, stage 5c-2b step 3 PASSED (42/42).** `bd51958f`, log `20261113T031144Z`.
  write_42 rung from user space twice on one ring (fence 1 then 2 in 1 ms, rptr/wptr 12 then
  24), so a doorbell on its own BAR page outside CP_MEC_DOORBELL_RANGE works under MES.
  All seven refusals held; after close the ring and the doorbell SIGBUS (pager read error).
  A user ring stuck in WAIT_REG_MEM (66 W, gfx busy 100 %) was lost after 5251 ms: waves
  killed, HQD reset inactive in 90 us, REMOVE ok; other VMs ran on and the VM ran clean on
  a kernel-mode queue. Steps 1/2 checks passed again (hang EIO after 5039 ms). vm_faults 1
  (counted once), hangs 2, grants 2 / live 0; unrouted EOPs 1 (src_data 0x18000000 once at
  attach, before any queue), MES own 54. Idle 7 W, detach clean, card cold. Next: step 4,
  the free rule (MES suspend/resume before freeing a BO of a VM with user queues).
- **2026-11-13, stage 5c-2b step 4 built (box run pending): the free rule.** A BO's last
  close clears its PTEs and flushes the VM's kernel VMID and every MES VMID (as before);
  then each user queue of those VMs is preempted once: MES SUSPEND of its gang with a fence
  in the kernel's MES block (never client memory), wait for it (2.1 s), RESUME. Proven VMs
  free the BO at once; VMs whose queues could not be suspended (lost, leaked, or SUSPEND
  without a fence) keep the old rule (the BO waits until they have no queues). Test 10.
  Also: `out_fence_gpu_va` adopted into ABI v2 as 2.2 (advisory, client-written; never a
  free or signal point; §5.9). RamPages follow-ups: a non-contiguous path for larger rings,
  a review of the managed-flag change against vm_page locking and invariants, a leak
  counter.
- **2026-11-13, stage 5c-2b step 4 PASSED (47/47); 5c-2b done.** `58c1881e`, log
  `20261113T032103Z`. BOs of VMs with live user queues are freed at their close: 21
  preemptions (SUSPEND 70-200 us, RESUME 100-140 us, suspend fence landed at once), 0
  failed, isolation `deferred 0 / reaped 0` (step 3: 19 / 19). A BO closed under a queue
  stuck in WAIT_REG_MEM was freed at once too (MES suspends a CP wait); the queue was still
  lost after hang_ms. Steps 1-3 passed again; SIGBUS children no longer dump core. Idle 7 W,
  detach clean, card cold. Open: the unrouted EOP count rises with the preemptions (1 to
  22) while only two are logged (rate limit); the SUSPEND/RESUME EOPs need identifying and
  counting as MES's own. Next: 5d (VRAM).
- **2026-11-13, stage 5d built (box run pending): VRAM.** The base starts the VRAM
  allocator (vram.c) on silicon after attach, over `[128 MiB, lowest PSP reservation)`
  rounded to 2 MiB: below is the driver's own fixed area (PSP 64, IP fw 68, SMU 84, GART
  88, page-table pool 92 MiB), above it the PSP's reservations (FB_FW_RESERV_ADDR[_EXT],
  now kept in the softc) and the top-of-VRAM areas amdgpu also reserves; with no
  reservation reported, the top 1 GiB stays out. Silicon need not wipe VRAM at a reset, so
  the whole range starts dirty (1 GiB pieces) and is handed out only as the scrub clears
  it. The scrub is CP DMA (DMA_DATA fill, SRC_SEL DATA, DST through the L2, CP_SYNC) on the
  kernel compute queue's own ring, so under VMID 0 at MC `fb_start + offset` (system
  aperture), 256 MiB per fence, only when the queue is idle between client submits, and
  checked against the queue's reset generation so a reset never passes a range unscrubbed.
  The engine registers only after a self-test (a BAR0 pattern on the range's first two
  pages: the fill must zero the first and leave the second). VRAM BOs map with VRAM PTEs
  (FB physical, no SYSTEM/SNOOPED), only inside the allocator's range; caps report the
  range as the device heap. Test 11 (VRAM BO reads zero, a fill reads back through a CP
  DMA copy, CPU read refused, freed BO scrubbed, the next BO reads zero); test 6's "VRAM
  refused" check is gone. Also: the first four distinct unrouted EOP `src_data[0]` values
  are counted and logged at queue close, to identify the SUSPEND/RESUME EOPs.
- **2026-11-13, stage 5d PASSED (53/53).** `a22a43de`, log `20261113T033700Z`. PSP
  reservations 50 MiB at 0x7f3e00000 and 4 MiB at 0x7d6c00000 (ext); gmc VRAM 32640 MiB at
  MC 0x8000000000, FB offset 0. Allocator range 0x8000000-0x7d6c00000 (31980 MiB); the
  ~400 MiB between the two reservations stays unused. Self-test at 0x8000000 (MC
  0x8008000000) PASS; the initial scrub of 31980 MiB took 410 ms (~78 GB/s, 128 fills, no
  idle waits, 0 failed). Test 11: a new VRAM BO reads zero, a fill reads back, CPU read
  refused, the freed BO scrubbed in 20 ms, the next BO reads zero. Steps 1-4 passed again.
  Unrouted EOPs identified: apart from 0x18000000 once at attach, they carry a bare user
  queue doorbell (0x20/0x22/0x26, no 0x1000000 tag) and track the free-rule preemptions:
  MES's SUSPEND/RESUME EOPs, to be counted as preemption EOPs. Idle 7 W, detach clean, card
  cold. Next: 5e (real-shader test variants).
- **2026-11-13, stage 5e step 1 built (box run pending): gpusim's compute ops as real
  shaders.** `atrium-rdna4-oracle/examples/si_ops.rs` builds COPY, INC and FILL (64
  threads, dst[i] = src[i], src[i] + 1, value; params in user SGPRs s[0:4]) and one shared
  IB with a derived patch table (`tests/si_ops.h`). si-client-test 12 runs each from a System
  and a VRAM source. The acceptance suites (`atrium_gpu_test`, `test_gpu_isolation`) still
  use the old ring SUBMIT, SIM kernels, draws, display and reset, none of which exist on
  silicon; how they run there is an open choice (dual-mode recommended). Also: bare
  user-queue doorbells in an EOP's `src_data[0]` are counted as MES preemption EOPs.
- **2026-11-13, stage 5e step 1 PASSED (60/60).** `1b62fe61`, log `20261113T035017Z`. COPY,
  INC and FILL as real shaders, each from a System and a VRAM source: 0/64 wrong every
  time. Preemption EOPs now counted as such (26 = the 26 free-rule preemptions); unrouted
  stays at 1 (0x18000000 at attach). Tests 1-11 passed again; idle 7 W, detach clean, card
  cold. Decided (Shankar): gpusim gets queue fds too, both acceptance suites move to queue
  fds and the old ring SUBMIT retires.
- **2026-11-13, stage 5f built (box run pending): queue fds everywhere, ring SUBMIT
  retired.** gpusim gets queue fds: a kernel-mode queue fd runs its blob on the shared
  engine queue in its VM (`rs/uqueue.rs`, `SIM_SLOT`), with a gfx family
  (`ATRIUM_GPU_QUEUE_FAMILY_GFX`, gpusim only; silicon EINVAL); USER_MODE stays silicon
  only. The v1 ring SUBMIT ('A',4) is gone from the ABI and the client node (ENOTTY;
  si-client-test 6 checks it). `atrium-gpu-rs` `Vm::submit`/`submit_deadline` keep their
  signatures and run the ring BO as a blob on a per-VM, per-engine queue fd (gfx falls
  back to compute where there is no gfx family), so frescod, atrium-gpu-app and the bins
  need no change. `atrium_gpu_test` and `test_gpu_isolation` use queue fds only and detect
  silicon (no GRAPHICS feature): there gfx work runs on compute, DMA_DATA uses the copy
  control word, the SIM INC kernel is the real INC shader, and the gpusim-only parts
  (draws, QUEUE_MAP, display, GPU_RESET, shared-queue deferral, watchdog and fault
  counters) print SKIP with their si-client-test counterpart. On silicon the isolation
  suite runs part A, breaks 1 (B is never blocked by A's parked work) and 5, F (VRAM
  scrub) and G (quotas). Also written into the ABI header: USER_MODE queues are only for a
  trusted renderer's own path, never an app or a per-client renderer (Shankar agreed).
- **2026-11-13, stage 5f on gpusim: suite ALL OK (28), isolation 82/82** (cloud amd64
  VM, TCG, queue-fd kmods). First run failed 9 + 11 checks: the suites' synchronous
  helper waited on a syncobj, but on gpusim the blob is the ring and a syncobj signals
  only from an interrupting EOP the ring itself carries, so plain rings never signalled
  (`EAGAIN` from SYNCOBJ_WAIT after the work had run). The helpers now submit without a
  syncobj on gpusim, as the old SUBMIT did; on silicon the kernel ends every blob with
  its own EOP, so they wait on one there. Runner: `gpu-stage-run.sh
  <pci>,<gpu>,<si test>,<suite>,<iso> 5f` runs si-client-test, then
  `/root/atrium-gpu-test` and `/root/test-gpu-isolation`.
- **2026-11-13, stage 5f on the box (a0597b2d): si-client-test 60/60; both suites
  FAILED.** `atrium_gpu_test`'s gfx test wrote gpusim's 6-dword RELEASE_MEM; gfx12's
  has 7 body dwords (the last is INT_CTXID). The short packet at the end of the IB
  wedged the CP (rptr 0, GPU busy), MES refused REMOVE_QUEUE, and the driver fell back
  to a mode 1 reset. Every later call failed ENXIO, so both suites cascaded into
  failures. Findings: (1) a client's malformed packet can force a whole-GPU reset
  (hostile-PM4 gap); (2) after the reset the device stays lost until reattach, and the
  reset path leaks bus resources at detach; (3) an unrouted EOP with context 0x4000020
  was logged.
- **2026-11-13, fixes for the next 5f run (gpusim: suite ALL OK, isolation ALL OK).**
  The suites emit the real gfx12 RELEASE_MEM on silicon (`emit_eop`); other
  silicon-path packets (WRITE_DATA, WAIT_REG_MEM, DMA_DATA, the si_ops shaders) were
  checked. Both suites stop at the first device loss (a fresh VM_CREATE fails ENXIO)
  instead of cascading. Also settled from the GPU security review: every ctl-node ioctl
  checks PRIV_DRIVER; a per-uid cap on live queues (`isolation.queue_limit`, 32,
  EDQUOT); GPU W^X: `BIND_EXEC` (needs `READONLY`) and `isolation.gpu_wx`. With it set,
  only EXEC-bound pages are executable on silicon, and a user queue's writeback and EOP
  pages lose X. The default stays 0 until the box shows which fetches need X.
  si-client-test 13 sets it, runs code and IB bound EXEC, checks that a jump into a
  read-write data BO faults (EIO), and probes an IB without X (informational).
- **2026-11-13 05:02, stage 5f on the box (05fdaebf): si-client-test 61/63; suites not
  run (device lost).** Tests 1-11 passed again. Test 13 set `gpu_wx`, which also took
  X off the user queue's writeback and EOP pages. The queue's first job was lost to a
  fault within 8 ms (counter only, no fault line logged; rptr report 0xc, no fence).
  Its HQD stayed active through the wave kill and HQD reset, MES refused REMOVE_QUEUE
  (status 0x8100030100000000), and the driver did a mode 1 reset. Both suites then
  stopped at their first VM_CREATE (ENXIO), as now intended. Detach was clean and the
  card cold, but the reset path again leaked 1 MSI vector and memory resources. The
  unrouted EOP tag was 0x4000026 with this queue's doorbell 0x26; the a0597b2d run
  had 0x4000020 with doorbell 0x20. So it is the failing queue's own EOP, tagged
  0x04000000 | doorbell. Conclusion: gfx12 needs X on a user queue's writeback/EOP
  pages, and a CP-side fault leaves a queue MES cannot remove (open, with the
  hostile-PM4 reset). Fix: `gpu_wx` is now bits: 1 = client BOs X only where bound EXEC;
  2 = no X on the queue's pages (diagnostic only). Test 13 uses bit 1 and no longer
  probes an IB without X.
- **2026-11-13 05:08, stage 5f on the box (955528b9): si-client-test 64/64; suite: all
  silicon tests pass but va_reuse; isolation stopped after Part A step 6 (GPU reset).**
  Test 13 (W^X, `gpu_wx` = 1) passed: code and IB bound EXEC run with data pages
  without X, and a jump into a read-write data BO faults (EIO). That queue came off
  cleanly (wave kill, fence landed, REMOVE_QUEUE ok) and the VM ran on. `va_reuse` ran
  out at bind 6656. Client VMs took their tables from the 5b test pool: 4 slices of 16
  tables, so 13 leaf tables, 6656 pages (26 MiB) per VM and only 4 client VMs at once.
  Isolation steps 4/5 have VM B DMA-copy from a VA it does not map. The CP-side fault
  lost B's queue, and at its close the HQD stayed active through the wave kill and HQD
  reset. MES refused REMOVE_QUEUE (status 0x8100030100000000) and the driver reset the
  GPU. This is the third wedge with this signature. All three were faults raised by the
  CP itself: a truncated packet, an EOP write without X, a DMA from an unmapped VA.
  Shader-side faults (stores, instruction fetch) recover cleanly. The fault lines were
  missing: the client-fault decode shared a 5/s log budget.
- **2026-11-13, fixes for the next run.**
  - Client VMs take each page table as a CPU-visible page from the VRAM allocator
    (`AMD_VM12_4L_HEAP`, up to 192 tables: the whole 256 MiB BO window plus queue
    pages, under a per-VM sx lock). The allocator's part inside BAR0 (128-256 MiB) is
    now marked visible for them; ordinary allocations go above it first.
  - Every VM fault batch logs a line (VMIDs, address) and the decoded fault status
    (client CPF/CPC/SQC...), on its own 10/s budget.
  - The HQD reset also writes SPI_COMPUTE_QUEUE_RESET, as amdgpu's
    `mes_v12_0_reset_queue_mmio` does. If REMOVE_QUEUE is still refused, the driver
    sends MES's own queue reset (MES_SCH_API_RESET, reset_queue_only, by doorbell) and
    tries REMOVE once more; only then does it reset the GPU. Isolation steps 4/5 are the
    reproducer.
- **2026-11-13 05:18, stage 5f on the box (a4073d89): si-client-test 64/64; suite ALL OK
  (va_reuse fixed); isolation again stopped after Part A step 6.** The new fault decode
  shows the difference:
  - test 7's store is client TCP and test 13's jump is client SQC (inst); both recover
    with a wave kill and REMOVE_QUEUE.
  - Isolation steps 4/5 (B's DMA_DATA from a VA its VM does not map) fault in client
    **CPG** (RW 0, PERMISSION_FAULTS 0x3) on VMID 8. That HQD stays active through
    the dequeue reset (now with SPI_COMPUTE_QUEUE_RESET), REMOVE_QUEUE (status
    0x8100030100000000) and MES's RESET (0x8100080000000000), so the driver reset the
    GPU.

  All four 5f wedges were CP-side faults from client packets, not a pipe problem.
  CP_DEBUG.CPG_UTCL1_ERROR_HALT_DISABLE was already set at gfx init. After the
  reset, 2 scrubs failed (40 KiB dirty). The allocator's visible part was 256 MiB.
  Fix for the next run: one more rung before the GPU reset, amdgpu's
  `mes_v12_0_reset_compute_pipe_mmio`. It holds the queue's MEC pipe in reset, forces
  every HQD on it inactive, releases the pipe, then retries REMOVE_QUEUE. Every queue on
  that pipe is lost (EIO), not the whole card.
- **2026-11-13 05:28, stage 5f on the box (a652163f): si-client-test 64/64; suite ALL OK;
  isolation again stopped after Part A step 6.** The CPG fault ladder ran in this order:
  dequeue reset (still active), REMOVE_QUEUE refused (…0301…), MES RESET refused
  (…0800…), REMOVE refused (…0303…), MEC pipe reset. The pipe reset **made the HQD
  inactive** ("not mapped"), but REMOVE_QUEUE was still refused (…0303…), so the driver
  did a mode 1 reset. After it, 4 scrubs failed with ENXIO and those pages were kept
  back. Next: reset the pipe first, as soon as the dequeue reset fails and before MES
  is asked anything, so MES never sees the refused REMOVE and RESET; MES RESET becomes
  the last rung.
- **2026-11-13 05:35, stage 5f on the box (647da1b1): si-client-test 64/64; suite ALL OK;
  isolation ran to the end with 5 FAILs (Break 1 and four Part F checks); no mode 1
  reset during the run.** With the pipe reset first, REMOVE_QUEUE succeeded for the
  CPG-faulted queue, but MEC pipe 1 stayed dead afterwards: CP_CPC_BUSY_STAT 0x840,
  and MES answered ADD_QUEUE ok for later queues on that pipe without ever mapping
  them, so their work never ran. GC sat busy at about 61 W until detach, whose
  designed mode 1 reset left the card cold.

  **Interrupts were not in use.** The card has one MSI message and no MSI-X. A detach
  through the reset path (04:35) never released the vector, so every later attach
  fell back to polling the IH ring every 10 ms, quietly; the suite's irq test skipped
  itself because it only knew MSI-X. Fixes for the next run:
  - every detach path, including the lost-GPU one, releases the IH vector;
  - a lost vector prints "ih: WARNING: ... polling the ring every 10 ms ... (reboot
    to recover)", and on silicon GET_IRQS reports the IH ring's vector and count;
  - atrium_gpu_test's irq test accepts MSI and fails on silicon in poll mode.
  The box was rebooted at 05:44 (user's go) to get the vector back.

  Pipe and CP-error fixes for the same run:
  - after a MEC pipe reset the driver enters RLC safe mode, re-arms the pipe's start
    PC (CP_MEC_ME1_PIPEn_PRGRM_START) and waits up to 100 ms for the PC to move,
    logging PC and CP_CPC_BUSY_STAT before and after;
  - CP error interrupts (bad opcode 0xb7, privileged register 0xb8, privileged
    instruction 0xb9) are enabled per user queue and lose that queue;
  - a stuck HQD logs its rptr/wptr and the CPC/CPF status registers;
  - unrouted IVs log client, source, ring, VMID and src_data[0] (still unexplained:
    EOP tags 0x04000000|doorbell from the faulting queue, and 0x18000000).
- **2026-11-13 05:52, stage 5f on the box (fdadff06): si-client-test 64/64; suite ALL OK;
  isolation 12 FAILs. Interrupts are back:** "ih: MSI, 1 vector", 300 interrupts,
  completions "after 0 ms", and detach released the vector. The MEC pipe reset made
  things worse:
  - step 7's CPG fault (VMID 8, pipe 1) got its pipe reset. The HQD went inactive, but
    CP_CPC_BUSY_STAT stayed 0x4 and the pipe's PC came back at 0x4198, not the start
    0xc00;
  - the next test's queue on pipe 0 (VMID 9, no fault of its own) never fetched, so
    pipe 0 was reset too. That killed the kernel queue (MEC1 pipe 0 queue 0), so every
    later VRAM scrub failed (scrub_failed 43, 33.5 GB dirty), and Parts F and G hit
    ENOMEM;
  - at detach, the kernel queue's self-test got no fence, GC was busy at about 69 W,
    and the mode 1 reset left the card cold.
  The CP error interrupts never fired. Only one unrouted EOP was seen, 0x18000000.
  Conclusion: across two runs a pipe reset never recovers the MEC. Proposed instead:
  drop the pipe reset; when a CP-faulted queue won't stop, do a mode 1 reset and
  re-initialise in the kernel (decision pending). Log: branch
  claude/project-thread-33aauq, docs/r9700-runs/20261113T055218Z-5f-fdadff06.log.
- **Fix for the next run: reset and re-initialise the GPU for a queue that will not
  stop** (Shankar's decision, 2026-11-13). The MEC pipe reset is gone. When a user
  queue's HQD stays active after its dequeue reset (a CP-side fault), MES is not asked
  to remove it; the driver resets and re-initialises the card instead:
  - the client side fails every queue's work (EIO) and lets go of its kernel queue,
    the IH consumer and the VRAM scrub engine;
  - `amd_si_reinit` does the mode 1 reset with bus mastering off, then frees the old
    MES, IH and kernel-queue memory without touching the hardware;
  - it turns bus mastering back on and runs the attach sequence again: PSP firmware
    load, SMU start and idle proof, GART, GFX, IH (a fresh MSI vector), MES, the
    dispatch self-test and the idle proof;
  - the client side opens a new kernel queue and re-registers the scrub engine (the
    failed scrubs are retried).
  Queues and VMs that existed at the reset stay lost until their fds close: their slots
  and VMIDs stay taken, so nothing new shares them, and their calls fail with EIO.
  New VMs and queues run on the new card. The doorbell BAR, with its fictitious user
  doorbell pages, carries over to the new MES, because old fds may still map them. At
  most 3 re-inits in 10 minutes; after that the GPU stays lost until the driver is
  reloaded. A failed re-init also leaves it lost, as before. gpusim does not run this
  path (both suites ALL OK there as a regression check).
- **2026-11-13 06:31, stage 5f on the box (ce098609): the box hung about 3 minutes into
  the run**, near isolation's step-7 CPG fault, where the first GPU re-init would run.
  ssh stopped answering and the box did not reboot. Before the run:
  - the 06:04 reboot into Ubuntu was Atrium's own fallback (/etc/rc.d/atrium_fallback),
    which re-arms on every boot; atrium-stay was run again at 06:31;
  - the box was booted once into Atrium (BootNext 0005) at 06:30.
  The log on Atrium's disk (read from Ubuntu, read-only) shows what happened. The re-init
  ran: mode 1 reset, sOS, TOC, 19 IP firmware loads and AUTOLOAD_RLC all went fine.
  It then gave up on "RLC autoload NOT complete" (ETIMEDOUT). That message also
  appears at every healthy attach, where it is ignored: the autoload completes once
  the SMU enables its features. So the re-init failed on a healthy card, did a second
  mode 1 reset and left the GPU lost. The last line on disk is 06:32:28; ssh died about
  06:34. Most likely cause of the hang: the detach after the suites. Client detach
  called `amd_ih7_set_consumer` on the IH pointer that the failed re-init had set to
  NULL, which panics, and with no dump device the box just stops. Fixes:
  - 54939f76: the NULL IH at client detach, and the scrub engine stopped before the
    kernel queue is taken;
  - next commit: the re-init treats the PSP load's result as attach does, and lets the
    SMU start and si_up decide.
  No crash dump: dumpdev="AUTO" has no swap partition to write to.
- **2026-11-13 07:19, stage 5f on the box (26fffb0b): the GPU re-init works on silicon.**
  si-client-test 64/64; suite ALL OK; isolation 2 FAILs, both in the test's accounting.
  - Step 7's CPG fault left a stuck HQD. The driver did a mode 1 reset and brought the
    whole card back in about 4 s: PSP, SMU (which completes the RLC autoload), GART,
    GFX, IH (MSI again), MES, dispatch PASS and the idle proof. Then "GPU re-init 1 of
    3: PASS" and a new kernel queue. Every later test ran on the new card; Break 1 and
    Part G pass.
  - Scrubs never failed. The card returned to idle (GRBM 0x382c) and was proven idle
    at detach (7 W). The vector was released.
  - Only 0x18000000 is still unrouted (one per IH instance).
  The 2 FAILs: Part F expected "VRAM in use" to grow by exactly the 64 KiB BO. On
  silicon a VM's page tables come from VRAM too, so A's grew by 88 KiB. Its last check
  also ran while B's VM was still open. The test now allows up to 1 MiB of page tables
  and closes B's VM before the final check (gpusim ALL OK).
- **2026-11-13 07:30, stage 5f on the box (26fffb0b kmods, 71259704 isolation test):
  STAGE 5f PASSED.** si-client-test 64/64; atrium_gpu_test ALL OK; test_gpu_isolation
  ALL OK, on queue fds only, with interrupts on MSI.
  - Step 7's CPG fault was handled by a full GPU re-init mid-run ("re-init 1 of 3:
    PASS"); the later parts ran on the new card.
  - Part F: +88 KiB for the 64 KiB BO with its page tables, scrubbed, B reads zero, a
    full-device alloc and free with no ENOMEM, and in-use back to its start.
  - Sysctls: scrub_failed 0, enomem 0, ih_overflows 0.
  - Power: 7 W idle while attached and at detach, where the card was proven idle; the
    vector was released.
  Logs: branch claude/project-thread-33aauq, commit 16d37a66.
  Open: the unrouted 0x18000000 EOP (one per IH instance); hostile-PM4 policy;
  reset-path leaks; crash dumps need a dump device on Atrium's disk.
- **2026-11-13 08:00, security gate step 1: the hostile-PM4 test is ready (4efb65df).**
  si-client-test 14 (silicon only) aims client packets at registers only the kernel
  may touch. It takes them from `dev.atrium_gpu_amd_pci.0.si_protected_regs`, which
  the kernel builds from its per-IP tables:
  - VM context 0's page-table base on the GC hub and on the MMHUB;
  - IH ring 0's base;
  - CP_MES_CNTL;
  - the SMU argument mailbox, read only (a write could race a live SMU message).
  Writes (WRITE_DATA and COPY_DATA immediate) use the value already in the register,
  so a write that lands changes nothing. Contexts 8-15 are left out because MES can
  point one at a new VM between the read and the write. Each packet runs on a fresh VM
  and passes only if it is refused: its work fails, the marker after it never lands
  and the register is unchanged. A read passes if it is refused or does not return
  the real value. A bystander VM must run clean after each. New counter:
  `isolation.cp_errors`. Risk on the box: if a refused packet leaves a stuck HQD, each
  refusal costs a whole-GPU re-init. The test stops after 2 (the cap is 3 per 10 min),
  and its last check reports whether any were needed.
  On gpusim with these kmods, atrium_gpu_test and test_gpu_isolation are ALL OK, and
  test 14 skips (no silicon). Box artifacts: atrium_gpu_amd.ko 43c9d76a…,
  atrium_gpu_amd_pci.ko 5849b6c7…, si-client-test 7e0c21dc….
- **2026-11-13 08:22, hostile-PM4 run on the box (4efb65df kmods, stage 5c2): isolation
  holds, recovery does not.** si-client-test 69/70. Tests 1-13 pass.
  - Test 14: the WRITE_DATA writes to VM context 0's page-table base on the GC hub and
    on the MMHUB were both refused. Each raised a CP error IV (privileged register), the
    register stayed unchanged, the marker never landed, and a bystander VM ran clean.
  - The cost: the CP halts mid-packet on the error (rptr 0x4 of 0xc, CPC_STATUS
    0xa0000051, CPF_STATUS 0xb8008001). That HQD will not dequeue even after its
    reset, so each refusal became a mode 1 reset plus re-init. Both re-inits passed.
  - The test stopped at 2 re-inits. ih_rb_base, cp_mes_cntl and the SMU read were not
    run. The final check ("costs its queue, not a whole-GPU re-init") FAILs.
  - Finding: any client can reset the whole GPU with one packet, and with 3 packets in
    10 minutes it leaves the GPU lost until the driver is reloaded. That is a
    denial-of-service hole.
  - The card was proven idle at detach (9 W) and the vector was released.
  Logs: branch claude/project-thread-33aauq, commit 03bb7450.
- **2026-11-13 08:45, reset blame (9e337339), chosen by Shankar on the card.** The GPU
  reset is blamed on the uid that created the queue which forced it. That uid's
  QUEUE_CREATE returns EPERM for `isolation.reset_lockout_s` (600 s). Root is exempt,
  and 0 turns the rule off. New counters: `resets_blamed` and `lockout_refused`. The
  global cap of 3 re-inits per 10 min stays. si-client-test 14 now runs each hostile
  job in a child under its own uid and checks that uid's next queue. The targets the
  last run missed go first. On gpusim, atrium_gpu_test and test_gpu_isolation are ALL
  OK. Box artifacts: pci 80cd52f5…, gpu f6aae609…, si-client-test 5801b269…, staged
  in xfer b40b243d. Still owed: recovering a halted CP queue on its own, without a
  full reset.
