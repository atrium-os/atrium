/*
 * atrium-gpu-amd — internal driver header (shared across the per-block .c
 * files; design §4.2). Holds the softc, the GFX12/SIM register map, the leaf
 * MMIO/PM4 mechanics (inline — §3.3), and the cross-file prototypes.
 *
 * Layout of the kmod (one module, several files; the §4.1 three-kmod split is
 * deferred until display lands):
 *   module.c    newbus probe/attach/detach + /dev/atrium-gpu0 cdev + teardown
 *   firmware.c  PSP/MES bring-up: reset, CP firmware load, MES init
 *   gmc.c       Graphics Memory Controller: GPUVM page tables, GMC/IH init
 *   bo.c        DMA-page allocator + BO backing store (System pages, VRAM)
 *   vm.c        GPUVM: VMIDs, page tables, PTEs
 *   cp.c        Command Processor: queue map + doorbell submit, compute state
 *   glue.c      cdevsw tables + softc accessors for the Rust front-end
 *   rs/         the front-end, in Rust (rs/README.md): the ATRIUM_GPU_IOC_*
 *               switch, and the VM, BO and syncobj fd objects
 */
#ifndef _ATRIUM_GPU_AMD_H_
#define _ATRIUM_GPU_AMD_H_

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/bus.h>
#include <sys/capsicum.h>
#include <sys/conf.h>
#include <sys/event.h>
#include <sys/fcntl.h>
#include <sys/file.h>
#include <sys/filedesc.h>
#include <sys/kernel.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/mutex.h>
#include <sys/proc.h>
#include <sys/rman.h>
#include <sys/selinfo.h>
#include <sys/stat.h>
#include <sys/user.h>

#include <machine/bus.h>
#include <machine/resource.h>

#include <sys/sx.h>
#include <sys/vmem.h>
#include <sys/sysctl.h>
#include <sys/taskqueue.h>
#include <sys/queue.h>

#include <vm/vm.h>
#include <vm/pmap.h>

#include <dev/pci/pcivar.h>
#include <dev/pci/pcireg.h>

#define ATRIUM_AMD_VENDOR	0x1002
/*
 * No device ids: the base probes any AMD display-class function and decides
 * at attach, from what answers behind BAR5, whether it can drive it
 * (identify.c): gpusim answers regSIM_ID; silicon is described by its IP
 * discovery table, and per-IP code is chosen by (hw id, version), never by
 * PCI device id (Shankar 2026-11-05).
 */

/* BAR5 = 256 KiB MMIO register file; BAR2 = the doorbell page (dev-ref §2,§5). */
#define ATRIUM_AMD_REGS_BAR	PCIR_BAR(5)
#define ATRIUM_AMD_DOORBELL_BAR	PCIR_BAR(2)
#define ATRIUM_AMD_VRAM_BAR	PCIR_BAR(0)	/* CPU window onto VRAM (sized, not mapped) */

/*
 * Register offsets within BAR5. A real GFX12 register's absolute BAR5 offset
 * = APER_<block> + reg_dword*4 (device-reference §3): APER_GC=0x0_0000,
 * APER_OSS=0x1_0000, APER_SIM=0x2_0000. The SIM aperture has no single real-HW
 * analog — it models the PSP/MES/reset handshake + the stubbed compute state.
 * Offsets are spelled out absolutely so a reader can match them to the model
 * (engine/src/device.rs `regs`) without arithmetic.
 */
#define regSIM_ID		0x00	/* identity probe: reads GPUSIM_MAGIC */
#define SIM_ID_MAGIC		0x47505553u /* 'G','P','U','S' */
#define regGRBM_STATUS		0x3690	/* GC: gpusim models as 0 (functional) */
#define regGPU_RESET		0x36a0	/* GC: GRBM_SOFT_RESET — per-block GFX soft
					 * reset (gpu-scoped: recovers a hung engine,
					 * leaves VRAM + the display block intact).
					 * Distinct from the device-wide FLR below. */

#define regFW_CP_VERSION	0x20050	/* SIM: staged CP ucode version (w) */
#define regFW_CP_LOAD		0x20054	/* SIM: w 1 = validate + activate CP */
#define regRESET_REQ		0x20058	/* SIM: w 1 = begin full GPU reset */
#define regRESET_STATUS		0x2005c	/* SIM: r 1 = reset latched, awaiting ack */
#define regRESET_ACK		0x20060	/* SIM: w 1 = ack / close reset window */
#define regMES_INIT		0x20068	/* SIM: w 1 = init MES (needs CP fw) */

#define regGMC_ENABLE		0x5890	/* GC: GCVM_CONTEXT0_CNTL (enable paging) */
#define regPT_BASE_LO		0x5a3c	/* GC: GCVM_CONTEXT0_PAGE_TABLE_BASE_LO32 */
#define regPT_BASE_HI		0x5a40	/* GC: ..._HI32 */
#define regTLB_INVALIDATE	0x591c	/* GC: GCVM_INVALIDATE_ENG0_REQ */
#define regVM_CNTL		0x20088	/* SIM: bit 0 = enforce PTE_WRITE */
#define regCONFIG_MEMSIZE	0x2008c	/* SIM: r = device VRAM in MiB (the NBIO
					 * RCC_CONFIG_MEMSIZE role); 0 on older gpusim */
#define VM_CNTL_WRITE_PROTECT	0x1
#define regIH_SIZE		0x10200	/* OSS: IH_RB_CNTL (ring size in entries) */
#define regIH_WPTR		0x10208	/* OSS: IH_RB_WPTR (device's write pointer) */
#define regIH_BASE_LO		0x1020c	/* OSS: IH_RB_BASE */
#define regIH_BASE_HI		0x10210	/* OSS: IH_RB_BASE_HI */

/* Interrupt-handler ring: each entry is a 16-byte cookie [cause:u32, ring:u32]. */
#define ATRIUM_AMD_IH_ENTRIES	256
#define ATRIUM_AMD_IH_COOKIE	16

#define regCP_RB0_BASE		0x7780	/* GC: gfx ring base (holds base>>8) */
#define regCP_RB0_CNTL		0x7784	/* GC: gfx ring size (bytes) */
#define regCP_RB_DOORBELL_CONTROL 0x7a34 /* GC: gfx ring doorbell offset (BAR2) */
#define regCP_ME_CNTL		0x200c	/* GC: w 0 = clear ME halt -> run */

#define regHQD_SELECT		0x20080	/* SIM: which HQD the CP_HQD regs program */
#define regCP_SUBMIT_DEADLINE_NS 0x20084 /* SIM: frame deadline (ns) the next doorbell stamps on its queue */
#define regCP_HQD_PQ_BASE	0x7ec4	/* GC: HQD ring base (holds base>>8) */
#define regCP_HQD_PQ_CONTROL	0x7ee8	/* GC: HQD ring size */
#define regCP_HQD_PQ_DOORBELL_CONTROL 0x7ee0 /* GC: HQD doorbell offset (BAR2) */
#define regCP_HQD_ACTIVE	0x7eac	/* GC: w 1 = activate HQD (gated on MES) */
/*
 * The COMPUTE state (SIM 0x200..0x210) and DRAW state (SIM 0x214..0x240)
 * registers are deliberately NOT named here: the kernel never writes them.
 * Userspace carries that state in the submitted ring as SET_SH_REG packets the
 * CP applies (the opaque-blob submit of ABI-v2); the register layout is the
 * userspace<->firmware contract, not the kernel's concern.
 */

/*
 * Per-context GPUVM (SIM aperture). A user address space (VMID 1..15) gets its
 * own page-directory base: select the VMID, then program its base. The
 * per-queue QF_VMID field (abstract Q_BASE block, q(qid)=Q_BASE+qid*STRIDE)
 * tells the CP which context a queue's submissions translate under — this is
 * what makes the same GPU-VA resolve to different memory per process.
 */
#define regVM_CTX_SELECT	0x2006c	/* SIM: which VMID the next regs program */
#define regVM_CTX_PT_BASE_LO	0x20070	/* SIM: selected VMID's page-dir base (lo/hi) */
#define regVM_CTX_PT_BASE_HI	0x20074
#define ATRIUM_AMD_Q_BASE	0x20100	/* SIM: per-queue config block base */
#define ATRIUM_AMD_Q_STRIDE	0x20	/* bytes per queue */
#define ATRIUM_AMD_QF_VMID	0x14	/* offset of the VMID field within a queue */
#define ATRIUM_AMD_QF_RPTR	0x1c	/* r: the queue's read pointer, in the same
					 * dword units as the doorbell wptr (gpusim
					 * 1ea93e5). rptr < wptr = work in flight. */

/*
 * Display block (APER_DISP = 0x3_0000; model: engine/src/display.rs `regs`).
 * Architecturally independent of GFX/compute — its own register aperture.
 */
#define regDISP_CONNECTOR_STATUS 0x30000 /* r: bit0 = connected (HPD) */
#define regDISP_DDC_OFFSET	0x30004	/* w: EDID byte offset to read next */
#define regDISP_DDC_DATA	0x30008	/* r: EDID byte at DDC_OFFSET (0xff = no DDC) */
#define regDISP_FB_BASE_LO	0x3000c	/* w: scanout FB VRAM offset (lo/hi) */
#define regDISP_FB_BASE_HI	0x30010
#define regDISP_FB_SIZE		0x30014	/* w: FB size in bytes */
#define regDISP_SET_MODE	0x30018	/* w 1: program the connector's EDID mode */
#define regDISP_FLIP		0x3001c	/* w: bit0 = vsync; the write triggers a flip */
#define regDISP_CONNECTOR_TYPE	0x30034	/* r: §8 interface type code */
#define regDISP_CFG_CONNECTOR_TYPE 0x30038 /* w: set connector type (test/bring-up) */
#define regDISP_CFG_PLUG_MODE	0x3003c	/* w: re-plug advertising a built-in mode */
#define regDISP_CFG_USBC	0x30040	/* w: USB-C alt-mode (0=USB, 2|4=enter N lanes) */
#define regDISP_USBC_LANES	0x30044	/* r: negotiated USB-C lane count */
#define regDISP_MST_ENABLE	0x30048	/* w1: (re)build a DP MST hub */
#define regDISP_MST_ADD_SINK	0x3004c	/* w: hot-plug an MST sink (mode code) */
#define regDISP_MST_SELECT	0x30050	/* w: select sink index for the MST queries */
#define regDISP_MST_SINK_COUNT	0x30054	/* r: number of MST sinks */
#define regDISP_MST_SINK_STARVED 0x30058 /* r: selected sink bandwidth-starved? */
#define regDISP_DPTRAIN_CABLE_RATE 0x3005c /* w: cable max rate (0=RBR..3=HBR3) */
#define regDISP_DPTRAIN_CABLE_LANES 0x30060 /* w: cable wired lanes */
#define regDISP_DPTRAIN_RUN	0x30064	/* w1: run link training */
#define regDISP_DPTRAIN_BW_MBPS	0x30068	/* r: trained bandwidth (MB/s) */
#define regDISP_DPTRAIN_TRAINED	0x3006c	/* r: 1 = a link trained */
#define regDISP_VBLANK_IRQ_EN	0x30070	/* w: 1 = raise an IH interrupt each vblank (DCN-like) */
#define regDISP_POWER_DEMAND_MW	0x30074	/* r: modeled display power demand, mW (energy federation) */
#define regDISP_POWER_BUDGET_MW	0x30078	/* w/r: granted power cap, mW (0 = uncapped) */
#define regDISP_POSTURE		0x3007c	/* w/r: power posture 0..10 (0 powersave..10 perf) */
#define regDISP_VBLANK_COUNT	0x30020	/* r: vblanks elapsed */
#define regDISP_DROPPED_FLIPS	0x30024	/* r: flips dropped by the depth-1 queue */
#define regDISP_FAULT		0x30028	/* r: last DisplayFault code (0 = none) */
#define regDISP_TEAR_LINE	0x3002c	/* r: first tear scanline (0xffffffff = none) */
#define regDISP_FB_PITCH	0x30080	/* w/r: FB line pitch, bytes (0 = packed) */
#define regDISP_FB_FORMAT	0x30084	/* w/r: FB pixel format code (0 = XRGB8888) */
#define regDISP_FLIP_PENDING	0x30088	/* r: 1 = a vsync'd flip is queued, not on screen */
#define regDISP_FLIP_IRQ_EN	0x3008c	/* w/r: 1 = IH interrupt (cause FLIP) when a flip lands */
#define regDISP_FLIP_DONE_COUNT	0x30090	/* r: flips that reached the screen */
#define regDISP_SCANOUT_BASE_LO	0x30094	/* r: VRAM base of the FB on screen now (lo/hi) */
#define regDISP_SCANOUT_BASE_HI	0x30098
#define regDISP_CFG_UNPLUG	0x3009c	/* w1: unplug the simulated monitor (test) */
#define regDISP_PIXEL_CLOCK_KHZ	0x300a0	/* w/r: pixel clock for the next mode set (DCCG not modelled) */
#define regDISP_FRAME_PERIOD_NS	0x300a4	/* r: the lit mode's frame period, ns */
#define regDISP_SINK_EOTF	0x300ac	/* r: test: the monitor's EOTF from the infoframes it gets */
#define regDISP_SINK_COLORIMETRY 0x300b0 /* r: test: 1 = BT.2020 */
#define regDISP_SINK_CONTENT_LIGHT 0x300b4 /* r: test: MaxCLL 15:0, MaxFALL 31:16 */
#define regDISP_SINK_BPC	0x300b8	/* r: test: bits per colour of the surface on screen */
#define regDISP_BW_BUDGET_KHZ	0x300bc	/* w/r: pixel rate the lit pipes share, kHz (device-wide;
					 * silicon: the DISPCLK/DCFCLK clock tables) */
#define regDISP_BW_USED_KHZ	0x300c0	/* r: pixel rate the lit pipes draw now */
/*
 * Span groups (display 3.1): SIM registers modelling DCN's OTG timing
 * synchronisation and flip lock. Per pipe. Silicon: the OTG trigger / GSL
 * programming, with D5.
 */
#define regDISP_SYNC_GROUP	0x300c4	/* w/r: timing group 1..4 this lit pipe joins (0 = none) */
#define regDISP_FLIP_LOCK	0x300c8	/* w/r: 1 = vsync'd flips held until the group's locked members all have one */
#define regDISP_SYNC_MASTER	0x300cc	/* r: 1 = this pipe's timing drives its group */
/*
 * Several pipes (display 3.0): pipe n's SIM registers are its bank at
 * + n x 0x100 (pipe 0 above); BW_* read the same in every bank.
 */
#define DISP_SIM_PIPE_STRIDE	0x100u
#define regDISP_PIPE(reg, n)	((reg) + (n) * DISP_SIM_PIPE_STRIDE)

/*
 * The real DCN 4 (DCN 4.0.1, RDNA4) display registers the scanout path uses,
 * pipe 0 (Fresco D4). Offsets are hardware dword offsets in DCN segment 2 and
 * fields are from AMD's MIT-licensed dcn_4_1_0_offset.h / _sh_mask.h. A
 * register's MMIO byte address is (segment base + offset) * 4. gpusim places
 * segment 2 at byte 0x60000 of its register BAR, as it places GC and OSS; on
 * the R9700 IP discovery gives DMU 4.1.0 (the block Linux's DC calls DCN 4.0.1 and its
 * firmware dcn_4_0_1; amdgpu renames 4.1.0 to 4.0.1 after discovery) segment bases 0x12, 0xc0, 0x34c0,
 * 0x9000, 0x2403c00 (dwords), so on silicon (D5) DCN_SEG2_BYTES comes from IP
 * discovery: 0x34c0 * 4 (identify.c sets sc->dcn_seg2_bytes). What is left on the SIM registers above: the mode
 * set (OTG timing, OPP, DIG and PHY are not modelled), EDID/DDC, link
 * training, MST and power, the referee's fault code, and the test hooks.
 */
#define DCN_SEG2_BYTES_GPUSIM	0x60000u	/* silicon: sc->dcn_seg2_bytes from IP discovery */
/* Byte offset inside DCN segment 2; the display macros add sc->dcn_seg2_bytes. */
#define regDCN2(d)		((d) * 4u)
#define regHUBP0_DCSURF_SURFACE_CONFIG		regDCN2(0x05e5)
#define regHUBP0_DCSURF_TILING_CONFIG		regDCN2(0x05e7)
#define regHUBPREQ0_DCSURF_SURFACE_PITCH	regDCN2(0x0607)
#define regHUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS	regDCN2(0x060a)
#define regHUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH	regDCN2(0x060b)
#define regHUBPREQ0_DCSURF_FLIP_CONTROL		regDCN2(0x0613)
#define regHUBPREQ0_DCSURF_SURFACE_FLIP_INTERRUPT	regDCN2(0x0617)
#define regOTG0_OTG_H_TOTAL			regDCN2(0x1b2a)
#define regOTG0_OTG_H_BLANK_START_END		regDCN2(0x1b2b)
#define regOTG0_OTG_H_SYNC_A			regDCN2(0x1b2c)
#define regOTG0_OTG_V_TOTAL			regDCN2(0x1b2f)
#define regOTG0_OTG_V_TOTAL_MIN			regDCN2(0x1b30)
#define regOTG0_OTG_V_TOTAL_MAX			regDCN2(0x1b31)
#define regOTG0_OTG_V_TOTAL_CONTROL		regDCN2(0x1b33)
#define regOTG0_OTG_V_BLANK_START_END		regDCN2(0x1b38)
#define regOTG0_OTG_V_SYNC_A			regDCN2(0x1b39)
#define regOTG0_OTG_STATUS_FRAME_COUNT		regDCN2(0x1b4d)
#define regOTG0_OTG_CONTROL			regDCN2(0x1b43)
#define regHPD0_DC_HPD_INT_STATUS		regDCN2(0x175a)
#define regHPD0_DC_HPD_INT_CONTROL		regDCN2(0x175b)
#define regHPD0_DC_HPD_CONTROL			regDCN2(0x175c)
#define regOTG0_OTG_VERTICAL_INTERRUPT0_POSITION	regDCN2(0x1b5f)
#define regOTG0_OTG_VERTICAL_INTERRUPT0_CONTROL	regDCN2(0x1b60)
#define regVPG0_VPG_GENERIC_PACKET_ACCESS_CTRL	regDCN2(0x2068)
#define regVPG0_VPG_GENERIC_PACKET_DATA		regDCN2(0x2069)
#define regVPG0_VPG_GSP_FRAME_UPDATE_CTRL	regDCN2(0x206a)
#define regDIG0_HDMI_GENERIC_PACKET_CONTROL0	regDCN2(0x20a5)

/*
 * DCN 4 has four pipes. The registers above are pipe 0's; pipe n's are the
 * same block's instance n, `stride` dwords on (dcn_4_1_0_offset.h): HUBP /
 * HUBPREQ 0xdc, OTG 0x80, HPD 0x8, VPG / DIG 0x124. Pipe n drives
 * connector n (the OTG-to-DIG crossbar is not modelled). Interrupt sources
 * (irqsrcs_dcn_1_0.h): OTGn vertical interrupt 0 is source 0x1e + n, HUBPn
 * flip 0x4f + n, HPDn+1 source 9 with context n.
 */
#define DCN_PIPES			4u
#define DCN_STRIDE_HUBP			0xdcu
#define DCN_STRIDE_OTG			0x80u
#define DCN_STRIDE_HPD			0x8u
#define DCN_STRIDE_DIG			0x124u
#define DCN_PIPE_REG(reg, stride, n)	((reg) + (n) * (stride) * 4u)
#define DCN_IH_SRC_VINT0		0x1eu
#define DCN_IH_SRC_FLIP			0x4fu
#define DCN_IH_SRC_HPD			0x9u
#define DCN_OTG_MASTER_EN		(1u << 0)	/* OTG_CONTROL */

#define DCN_PIXEL_FORMAT_ARGB8888	8u	/* SURFACE_CONFIG.SURFACE_PIXEL_FORMAT */
#define DCN_PIXEL_FORMAT_ARGB2101010	10u
/* VPG generic packets (infoframes): GENERIC_DATA_INDEX = 9 x n, then 9
 * dwords to GENERIC_PACKET_DATA (header, PB0..PB31), then FRAME_UPDATE bit
 * n; DIG0 HDMI_GENERICn_SEND (bit 4n) + _CONT (4n+1) send it every frame.
 * Packet layout as the Linux display code writes it (gpl_fact, medium). */
#define DCN_VPG_PACKET_DWORDS		9u
#define DCN_VPG_GENERICS		8u
#define DCN_HDMI_GENERIC_SEND_CONT(n)	(3u << (4 * (n)))
#define DCN_SW_MODE_LINEAR		0u	/* TILING_CONFIG.SW_MODE */
#define DCN_PITCH_MASK			0xffffu	/* SURFACE_PITCH.PITCH: pixels - 1 */
#define DCN_ADDRESS_HIGH_MASK		0xffffu
#define DCN_SURFACE_UPDATE_LOCK		(1u << 0)	/* FLIP_CONTROL */
#define DCN_SURFACE_FLIP_TYPE		(1u << 1)	/* 0 vsync, 1 immediate */
#define DCN_SURFACE_FLIP_PENDING	(1u << 8)
#define DCN_SURFACE_FLIP_INT_MASK	(1u << 0)	/* SURFACE_FLIP_INTERRUPT: enable */
#define DCN_SURFACE_FLIP_CLEAR		(1u << 8)
#define DCN_OTG_FRAME_COUNT_MASK	0x00ffffffu
#define DCN_VINT0_INT_ENABLE		(1u << 8)	/* VERTICAL_INTERRUPT0_CONTROL */
#define DCN_VINT0_CLEAR			(1u << 20)
#define DCN_OTG_END_SHIFT		16	/* *_START_END / *_SYNC_A: end in 30:16 */
#define DCN_OTG_TOTAL_MASK		0x7fffu	/* *_TOTAL, *_TOTAL_MIN/MAX: lines - 1 */
#define DCN_V_TOTAL_MIN_SEL		(1u << 0)	/* OTG_V_TOTAL_CONTROL */
#define DCN_V_TOTAL_MAX_SEL		(1u << 1)
#define DCN_HPD_SENSE			(1u << 1)	/* DC_HPD_INT_STATUS */
#define DCN_HPD_INT_ACK			(1u << 0)	/* DC_HPD_INT_CONTROL */
#define DCN_HPD_INT_POLARITY		(1u << 8)
#define DCN_HPD_INT_EN			(1u << 16)
#define DCN_HPD_EN			(1u << 28)	/* DC_HPD_CONTROL */
#define ATRIUM_AMD_EDID_LEN	128

/*
 * Firmware energy-fair scheduler block (APER_SCHED = 0x4_0000; model:
 * engine/src/sched_regs.rs). The kernel programs weights, the device enforces.
 */
#define regSCHED_WEIGHT		0x40000	/* w: staged queue weight */
#define regSCHED_KERNEL_OPS	0x40004	/* w: staged per-dispatch ops */
#define regSCHED_KERNEL_BYTES	0x40008	/* w: staged per-dispatch bytes */
#define regSCHED_KERNEL_LEVEL	0x4000c	/* w: staged memory level (0..4) */
#define regSCHED_ADD_QUEUE	0x40010	/* w1: append a queue with the staged config */
#define regSCHED_RUN_ROUNDS	0x40014	/* w: run N energy-fair rounds */
#define regSCHED_SELECT		0x40018	/* w: select a queue for readback */
#define regSCHED_ENERGY_UJ	0x4001c	/* r: selected queue energy (uJ, telemetry) */
#define regSCHED_RUNS		0x40020	/* r: selected queue run count */
#define regSCHED_QUEUE_COUNT	0x40024	/* r: number of queues */
#define regSCHED_BUSY_US	0x40028	/* r: selected queue engine time (us, fairness) */
#define regSCHED_POWER_BUDGET_MW 0x4002c /* w: federation budget (0 = uncapped) */
#define regSCHED_POWER_DEMAND_MW 0x40030 /* r: average power demand (mW) */
#define regSCHED_ROUNDS_EXEC	0x40034	/* r: cumulative rounds executed */
#define regSCHED_DEADLINE	0x40038	/* w: selected queue's deadline, ns from now (0=clear) */
#define regSCHED_DEADLINE_WINDOW 0x4003c /* w: deadline window, ns (0=deadline-blind) */
#define regSCHED_NOW_NS		0x40040	/* r: scheduler virtual clock, ns */
#define regSCHED_POWER_POSTURE	0x40044	/* w/r: power posture 0..10 (0 powersave..10 perf) */

/*
 * Power-gating block (APER_PGATE = 0x5_0000; model: engine/src/pgate_regs.rs).
 * The driver reads which IP blocks are idle, power-gates them, and reads back the
 * power/energy. Exposes POWER gating (the driver-controlled lever); clock gating
 * is hardware-automatic, no register.
 */
#define regPGATE_NUM_BLOCKS	0x50000	/* r: number of gateable IP blocks */
#define regPGATE_BLOCK_BUSY	0x50004	/* r: bitmask of blocks with work in flight */
#define regPGATE_BLOCK_GATE	0x50008	/* rw: bitmask of blocks to power-gate */
#define regPGATE_SET_BUSY	0x5000c	/* w: set the busy bitmask (engine stand-in) */
#define regPGATE_POWER_MW	0x50010	/* r: current draw, milliwatts */
#define regPGATE_ENERGY_UJ	0x50014	/* r: accumulated energy, microjoules */
#define regPGATE_TICK_US	0x50018	/* w: advance N us, accruing energy */
#define regPGATE_SELECT		0x5001c	/* w: select a block for readbacks */
#define regPGATE_SEL_ACTIVE_MW	0x50020	/* r: selected block powered leakage (mW) */
#define regPGATE_SEL_GATED_MW	0x50024	/* r: selected block gated leakage (mW) */
#define regPGATE_SEL_EXIT_US	0x50028	/* r: selected block wake latency (us) */
#define regPGATE_NEXT_BUSY	0x5002c	/* w: foreknowledge - blocks the next job needs */
#define regPGATE_WAKE_STALL_US	0x50030	/* r: stall if NEXT_BUSY blocks aren't pre-woken */

/* CP firmware: minimum ucode version the model accepts (CP_FW_MIN_VERSION). */
#define ATRIUM_AMD_CP_FW_VERSION 0x40
/* Reset poll: model latches synchronously; poll-with-timeout is the HW shape. */
#define ATRIUM_AMD_RESET_POLLS	1000	/* ×10us = 10ms budget */
#define ATRIUM_AMD_RESET_DELAY	10	/* us between polls */

/*
 * GPUVM 2-level page table (residency.rs): PDE at pdb[va>>21], PTE at
 * pt[va>>12], each 8 bytes = (page-aligned phys) | PTE_VALID(bit 0).
 */
#define ATRIUM_AMD_PTE_VALID	0x1ULL
#define ATRIUM_AMD_PTE_VRAM	0x2ULL	/* page is device VRAM (else System/GTT) */
#define ATRIUM_AMD_PTE_WRITE	0x4ULL	/* GPU may write (enforced once VM_CNTL.WP) */
#define ATRIUM_AMD_VRAM_BYTES	(256ULL << 20)	/* VRAM size if CONFIG_MEMSIZE reads 0 */
#define ATRIUM_AMD_PD_SHIFT	21
#define ATRIUM_AMD_PT_SHIFT	12
#define ATRIUM_AMD_PT_MASK	0x1ff

/* PM4 type-3 header layout (public PM4; opcodes verified vs kfd_pm4_opcodes.h). */
#define PM4_TYPE3		3u
#define IT_NOP			0x10u
#define IT_RELEASE_MEM		0x49u
#define IT_DMA_DATA		0x50u	/* CP DMA: copy, or fill with SRC_SEL = DATA */
/* DMA_DATA fields (public layout, Mesa sid.h R_411/R_415; GFX9+). */
#define DMA_DATA_SRC_SEL_DATA	(2u << 29)	/* DW1: source is the DW2 value */
#define DMA_DATA_CP_SYNC	(1u << 31)	/* DW1: finish before the next packet */
#define DMA_DATA_MAX_BYTES	((1u << 26) - 1)	/* DW6 BYTE_COUNT width */
#define IT_DISPATCH_DIRECT	0x15u
#define IT_DRAW_INDEX_AUTO	0x2du

/*
 * Per-engine fixed queue/doorbell assignment (no queue manager yet). Each
 * queue's doorbell sits at the start of its OWN BAR2 page so the page can be
 * mmap'd / SCM_RIGHTS-granted to one client without exposing other queues'
 * doorbells (real-HW per-queue doorbell granularity).
 */
#define ATRIUM_AMD_RING_BYTES	256	/* CP ring-size register value */
#define ATRIUM_AMD_GFX_QID	0
#define ATRIUM_AMD_GFX_DOORBELL	0x0000	/* doorbell page 0 */
#define ATRIUM_AMD_COMPUTE_QID	1
#define ATRIUM_AMD_COMPUTE_DOORBELL 0x1000 /* doorbell page 1 */

/*
 * Per-VM GPU-VA bump allocator: each address space places BOs at BO_VA_BASE,
 * BO_VA_BASE+page, ... across a contiguous run of NUM_PT page-directory
 * entries (NUM_PT * 512 pages, NUM_PT pre-allocated page-table pages). Two VMs
 * both start at BO_VA_BASE, so the same VA in different VMs resolves to
 * different memory — isolation. NUM_PT is sized for the display server's
 * scanout set (a System staging BO + two VRAM scanout BOs, each
 * framebuffer-sized) twice over, since a mode switch lights fresh buffers
 * before it frees the old ones: 128 PT pages = 256 MiB of VA covers a 4K
 * set (~100 MiB) next to another. Cost: 512 KiB of page tables per VM.
 * (16 PT pages, 32 MiB, fit only one 1080p set: a switch to 1440p ran the
 * VM out of VA.) PD indices AMD_VM_PD_INDEX .. +NUM_PT-1 must stay < 512.
 */
#define ATRIUM_AMD_BO_VA_BASE	0x10000000ULL
#define ATRIUM_AMD_VM_NUM_PT	128	/* page-table pages per VM */
#define ATRIUM_AMD_VM_MAX_BO	(512 * ATRIUM_AMD_VM_NUM_PT) /* VA pages per VM */
#define ATRIUM_AMD_MAX_VMID	16	/* hardware contexts; 1..15 for user VMs */

/*
 * A page of DMA-able guest memory: kernel VA (CPU side) + guest-physical
 * address (what the device DMA-walks). In a VM with no IOMMU on this device,
 * gpa == vtophys(kva).
 */
struct atrium_amd_dma_page {
	void		*kva;
	vm_paddr_t	 gpa;		/* device-visible bus address */
	bus_dma_tag_t	 tag;
	bus_dmamap_t	 map;
};

/*
 * A buffer object: a DMA page mapped into GPUVM and exposed to userspace as a
 * file descriptor (fd-as-handle — ABI-v2 principle 2). Lifetime is the fd
 * refcount; the BO owns its own page (distinct from the internal dma[] pages
 * that back page tables / the IH ring). It carries a back-reference to the
 * device so fo_close can unmap + free without a handle table.
 */
struct atrium_amd_softc;
struct atrium_amd_vm;
struct atrium_amd_bo;
struct atrium_amd_client;
struct atrium_gpu_sched;	/* 'A' ABI structs (defined in atrium_gpu_amd_abi.h) */
struct atrium_gpu_powergate;

/*
 * Backend-supplied device capability values; the front-end assembles these into
 * the QUERY_CAPS TLV (the ABI version is the front-end's, the rest is backend).
 */
struct atrium_gpu_backend_caps {
	const char *vendor;	/* CAP_VENDOR */
	uint32_t    features;	/* CAP_FEATURES_V1 bitmap */
	uint64_t    va_base;	/* CAP_ADDRESS_SPACE */
	uint64_t    va_size;
	uint64_t    va_align;
	uint64_t    vram_bytes;	/* CAP_HEAPS device heap size */
	uint64_t    bind_features; /* CAP_BIND_FEATURES */
	uint32_t    isa_family;	/* CAP_ISA */
	uint32_t    isa_revision;
	const char *isa_name;
};

/*
 * Backend ops — the per-transport hardware seam under the shared 'A' front-end
 * (docs/spec/atrium-gpu-driver-architecture.md §6). The cdev + ioctl dispatch,
 * the BO/VM/syncobj fd-objects, the bindings list, the syncobj timeline and the
 * caps TLV are transport-neutral and call ONLY through this vtable; a backend
 * (amd = GPUVM + PM4 over gpusim/real silicon; virtio = context + SUBMIT_3D)
 * supplies the hardware. The submit blob is opaque and the GPU-VA is backend-
 * assigned, which is what keeps the front-end neutral.
 */
struct atrium_amd_syncobj;
struct atrium_gpu_backend_ops {
	const char *name;
	/* Stand up / tear down a VM's hardware address space (amd: VMID + GPUVM
	 * page tables + per-context PT base; virtio: a 3D context). The front-end
	 * owns the fd object + struct; the backend owns the hardware. vm_setup
	 * self-cleans on failure; vm_teardown undoes a successful setup. */
	int	(*vm_setup)(struct atrium_amd_softc *sc, struct atrium_amd_vm *vm);
	void	(*vm_teardown)(struct atrium_amd_vm *vm);
	/* Allocate / free a BO's backing store (amd: bus_dma System pages or a VRAM
	 * bump; virtio: a blob resource). Fills the BO's page list + size; the
	 * front-end owns the struct + fd + the cross-VM bindings. bo_alloc
	 * self-cleans on failure. */
	int	(*bo_alloc)(struct atrium_amd_softc *sc, struct atrium_amd_bo *bo,
		    uint64_t size, uint32_t flags);
	void	(*bo_free)(struct atrium_amd_bo *bo);
	/* Map / unmap one page of a BO into a VM at a GPU-VA (amd: a GPUVM PTE). */
	int	(*map_page)(struct atrium_amd_vm *vm, uint64_t va,
		    vm_paddr_t phys, int vram, int writable);
	void	(*unmap_page)(struct atrium_amd_vm *vm, uint64_t va);
	/* Submit a prepared ring on an engine in a VM (amd: PM4 ring + doorbell,
	 * under the queue's lock; EBUSY if another client owns the queue or
	 * another VM's work on it does not drain). The front-end has checked
	 * the arguments: the ring is bound in `vm` at `ring_va`, holds at least
	 * `n_dwords` (> 0), and the caller holds the ring's and VM's files. The
	 * backend checks only what is its own: `engine`. */
	int	(*submit)(struct atrium_amd_softc *sc,
		    struct atrium_amd_client *client, uint64_t ring_va,
		    uint32_t n_dwords, uint32_t engine, struct atrium_amd_vm *vm,
		    uint32_t deadline_ns, struct atrium_amd_syncobj *so,
		    uint64_t signal_value);
	/* Export a BO as a scanout handle (amd: absolute VRAM offset + size; only
	 * VRAM is scannable). EINVAL if the BO can't be scanned out. */
	int	(*export_scanout)(struct atrium_amd_bo *bo, uint64_t *vram_offset,
		    uint64_t *size);
	/*
	 * Front-end exports for the display module, which has no BO or syncobj
	 * code of its own and links against nothing in this module: it resolves
	 * a commit's bo_fd and fence fds through these. They live in the gpu
	 * module, so they are NULL (with the rest of the vtable) while it isn't
	 * loaded. syncobj_signal may be called with sc->lock held (the order is
	 * sc->lock, then the syncobj's own lock, as in the EOP handler).
	 */
	int	(*bo_fget)(struct thread *td, int fd, struct file **out_fp,
		    struct atrium_amd_bo **out_bo);
	int	(*syncobj_fget)(struct thread *td, int fd, struct file **out_fp,
		    struct atrium_amd_syncobj **out_so);
	void	(*syncobj_signal)(struct atrium_amd_syncobj *so, uint64_t value);
	void	(*syncobj_fail)(struct atrium_amd_syncobj *so, int error);
	/* Install (fn != NULL) or clear the syncobj's one watcher. */
	void	(*syncobj_watch)(struct atrium_amd_syncobj *so,
		    void (*fn)(void *), void *arg);
	/* Has the timeline reached `value`? 1 yes, 0 not yet, -errno if the
	 * work that would reach it was lost. */
	int	(*syncobj_ready)(struct atrium_amd_syncobj *so, uint64_t value);
	/* mmap a device aperture into userspace (amd: the doorbell page of a queue
	 * this client owns, as a revocable managed-pager object). */
	int	(*mmap_single)(struct atrium_amd_softc *sc,
		    struct atrium_amd_client *client, vm_ooffset_t *offset,
		    vm_size_t size, vm_object_t *object, int nprot);
	/* Fill in the backend's device capability values (vendor, heaps, VA window). */
	void	(*get_caps)(struct atrium_amd_softc *sc,
		    struct atrium_gpu_backend_caps *caps);

	/*
	 * Optional ops — a backend that doesn't implement one leaves it NULL and
	 * the front-end returns ENOTSUP (capability-gated per v2). These are the
	 * advanced/vendor-specific surfaces beyond the common path.
	 */
	/* Map a queue's doorbell for user-mode submission (amd: program the MEC/CP
	 * queue at ring_va, return the doorbell page offset). */
	int	(*queue_map)(struct atrium_amd_softc *sc,
		    struct atrium_amd_client *client, struct atrium_amd_vm *vm,
		    uint64_t ring_va, uint32_t engine, uint32_t *doorbell_off);
	/* Recover a wedged engine (amd: reset + reload firmware + re-init MES). */
	int	(*gpu_reset)(struct atrium_amd_softc *sc);
	/* Energy-aware scheduler hint (amd: SCHED registers). */
	void	(*sched)(struct atrium_amd_softc *sc, struct atrium_gpu_sched *s);
	/* Power-gating policy (amd: GFXOFF / per-IP PG). */
	int	(*powergate)(struct atrium_amd_softc *sc,
		    struct atrium_gpu_powergate *p);
};

/*
 * A per-process GPU address space (ABI-v2 §5.2): its own VMID and 2-level page
 * table, programmed into the device's per-context page-directory registers. A
 * VM is a struct file; BOs created in it hold a reference, so it outlives them.
 * Page-table pages are pre-allocated (one PT page = 512 BOs in a 2 MiB span) so
 * mapping a BO never allocates under a lock.
 */
struct atrium_amd_vm {
	struct atrium_amd_softc *sc;
	uint16_t	 vmid;		/* 1..15 */
	struct atrium_amd_dma_page pdb;	/* page-directory page */
	struct atrium_amd_dma_page pt[ATRIUM_AMD_VM_NUM_PT]; /* page-table pages */
};
/*
 * Front-end/back-end split (rs/README.md). The struct above is the backend's
 * part of a VM: the hardware address space. The fd object, its GPU-VA arena
 * and its lifetime are the Rust front-end's (rs/vm.rs), which embeds this
 * struct as its first field, so a `struct atrium_amd_vm *` is also a pointer
 * to the whole object. The same holds for BOs below; syncobjs and clients are
 * the front-end's alone and opaque to C.
 */

/*
 * A buffer object: a DMA page mapped into a VM's GPUVM and exposed to userspace
 * as a file descriptor (fd-as-handle — ABI-v2 principle 2). Lifetime is the fd
 * refcount; the BO owns its own pages and is independent of any VM (ABI-v2
 * principle 4), so the SAME object can be bound into several VMs at once — the
 * cross-address-space sharing path (a compositor importing a client's buffer).
 * Each binding holds a reference (vm_fp) on its VM, so every VM the BO is mapped
 * into outlives the BO and fo_close can unmap from all of them.
 */
/*
 * Upper bound on ONE BO, as a sanity check on a userspace-supplied size — not a
 * design limit. The page list is allocated to fit the BO (see bo->pages), so a
 * scanout is bounded by memory, not by a compiled-in array.
 *
 * It used to be 512 pages (2 MiB) because pages[] was an inline fixed array
 * sized around "a 640x480x4 scanout FB is 300 pages". That silently capped the
 * display at VGA: moving the modeled monitor to 1080p made frescod die at
 * startup with EINVAL, because a 1920x1080x4 scanout is 2025 pages. 4K is 8100
 * and 8K is 32400 — none of which can be an inline array in every BO.
 *
 * 64 Ki pages = 256 MiB: comfortably above an 8K RGBA scanout (132 MiB) while
 * still refusing a nonsense request outright rather than trying to honour it.
 */
#define ATRIUM_AMD_BO_MAX_PAGES	(64 * 1024)
#define ATRIUM_AMD_BO_MAX_BIND	8	/* VMs a BO can be shared into at once */

/*
 * The backend's part of a BO: its backing store. The bindings into VMs (and
 * their lock, kmod audit 2026-11-07 G1) are the front-end's (rs/bo.rs).
 */
struct atrium_amd_bo {
	struct atrium_amd_softc *sc;
	void		*kva;		/* CPU mapping (bus_dmamem, page-contiguous) */
	bus_dma_tag_t	 dmat;		/* per-BO DMA tag */
	bus_dmamap_t	 dmamap;	/* the BO's DMA mapping */
	int		 npages;	/* pages backing this BO */
	int		 vram;		/* 1 = VRAM-resident (pages[] are VRAM offsets,
					 * no kva/dmat); 0 = System/GTT (bus_dma) */
	bus_addr_t	*pages;		/* per-page bus addrs / VRAM offsets;
					 * npages entries, allocated with the BO */
	uint64_t	 size;
};

/*
 * A timeline syncobj: a monotonic 64-bit counter exposed as an fd
 * (ABI-v2 §5.6). A submission signals it on completion; userspace waits for a
 * value either blocking (SYNCOBJ_WAIT) or via kqueue — the fd is EVFILT_READ-
 * able, with the threshold passed in the kevent `data` field, so a compositor
 * folds GPU completion into one kevent() alongside input and timers. (v2's
 * separate per-threshold event_fd is deferred; the syncobj fd is the kqueue
 * source for now.) The struct-file refcount is its lifetime.
 */
struct atrium_amd_syncobj;	/* rs/syncobj.rs; opaque here */

/*
 * A completion the ISR owes a syncobj. A submission that signals a syncobj
 * pushes one of these *before* ringing the doorbell; the ISR pops it on the
 * end-of-pipe interrupt and signals the syncobj — so a submission whose ring
 * parks on a cross-queue WAIT is signalled asynchronously, when a *later*
 * doorbell unblocks it, not inline. The syncobj's fo_close scrubs its entries
 * under sc->lock, which serializes against the ISR (no refcount / no ISR free).
 */
#define ATRIUM_AMD_MAX_PENDING	16	/* signalled submits in flight per queue */
/* The completions each queue owes, and its hang clock, are rs/pending.rs
 * (guarded by sc->lock, which the ISR holds). */

#define ATRIUM_AMD_IH_CAUSE_EOP	1	/* IH cookie cause: end-of-pipe (device.rs) */
#define ATRIUM_AMD_IH_CAUSE_VBLANK 2	/* IH cookie cause: DCN vertical blank */
#define ATRIUM_AMD_IH_CAUSE_FLIP 3	/* IH cookie cause: DCN flip landed on screen */
#define ATRIUM_AMD_IH_CAUSE_VM_FAULT 4	/* GPUVM fault: queue, VMID, page (gpusim 013b62b) */
#define ATRIUM_AMD_IH_CAUSE_HPD 5	/* DCN hot plug detect (HPD0 level interrupt) */
#define ATRIUM_AMD_IH_NCAUSE	6	/* dispatch-table size (causes 0..N-1) */

/*
 * IH cause handler: the device-global interrupt ring is owned by the BASE
 * module, which drains it and demuxes by cookie cause (exactly as real silicon's
 * one IH ring carries GFX end-of-pipe AND DCN vblank to a single ISR). Each IP
 * module registers a handler for its own cause(s) — gpu for EOP, display for
 * VBLANK — so neither needs the other: the base routes the cause to whoever
 * registered. The handler is called with sc->lock HELD and `count` = how many of
 * that cause this interrupt drained (coalesced events report > 1).
 */
typedef void (*atrium_amd_ih_handler)(struct atrium_amd_softc *sc, int count);

/*
 * Device-reset coordination. A full FLR (mode-1 reset) is a DEVICE-global event:
 * on real silicon it resets every IP block — GFX *and* DCN (display) — so it
 * cannot be a thing one IP module does behind the others' backs. The base owns
 * the FLR (amd_flr) and the coordinator (amd_device_reset); each IP module
 * registers a prepare/restore pair so a device-lost recovery quiesces every
 * block before the FLR and re-initialises it after (amdgpu's pre_reset/post_reset
 * across IP blocks). prepare/restore run under sc->lock is NOT assumed — they may
 * sleep (firmware reload), so the coordinator calls them unlocked.
 *
 * Cold bring-up takes a different path: the base FLRs ONCE at device attach,
 * before any child exists, so no hooks are needed there — the children then
 * attach onto an already-clean device.
 */
enum {
	ATRIUM_AMD_IP_GPU = 0,		/* GFX/compute (gpu module) */
	ATRIUM_AMD_IP_DISPLAY,		/* DCN (display module) */
	ATRIUM_AMD_IP_COUNT
};
struct atrium_amd_reset_hooks {
	void (*prepare)(struct atrium_amd_softc *sc);	/* quiesce before FLR */
	void (*restore)(struct atrium_amd_softc *sc);	/* re-init after FLR */
};

/*
 * GPU isolation (docs/spec/gpu-isolation.md; review 2026-11-03 §A3).
 *
 * A client is one open of /dev/atrium-gpuN (cdevpriv). A hardware queue is one
 * of the device's two shared rings (gfx qid 0, compute qid 1). Each has its own
 * sleepable lock around program + doorbell, so two clients can never interleave
 * those register writes (break 1). A queue is never switched to another VMID
 * while the queue's read pointer trails what was rung (work in flight); the
 * submitter waits, then gets EBUSY. A client that maps a queue for user-mode
 * submission owns it exclusively until it closes the device or the VM goes away
 * (break 3); only the owner can mmap that queue's doorbell page, through a
 * per-grant managed pager object the kernel revokes on release.
 *
 * The real fix for the shared queues is per-VM MES queues (review §A2.3); this
 * keeps the two shared queues safe until that lands.
 */
#define ATRIUM_AMD_NHWQ		2
#define ATRIUM_AMD_QUEUE_WAIT_MS 2000	/* wait for another VM's work to drain */
#define ATRIUM_AMD_HANG_MS	5000	/* default watchdog limit (sysctl hang_ms) */
#define ATRIUM_AMD_VRAM_LIMIT_PCT 75	/* default per-uid VRAM cap (kmod audit G2) */
#define ATRIUM_AMD_SYS_LIMIT_PCT 50	/* default per-uid System BO cap */

struct atrium_amd_client;	/* one open of the client node (rs/ioctl.rs) */


struct atrium_amd_isolation_stats {
	uint64_t	 queue_waits;	/* submits that waited for another VM */
	uint64_t	 queue_refused;	/* EBUSY: owned, or did not drain */
	uint64_t	 dequeues;	/* queues stopped with work dropped */
	uint64_t	 grants;	/* doorbell grants created */
	uint64_t	 revokes;	/* doorbell grants revoked */
	uint64_t	 deferred;	/* BO frees deferred */
	uint64_t	 reaped;	/* deferred BO frees completed */
	uint64_t	 hangs;		/* queues the watchdog stopped */
	uint64_t	 fences_failed;	/* completions failed with EIO */
	uint64_t	 vm_faults;	/* VM fault interrupts */
	uint64_t	 quota_refused;	/* BO_ALLOCs refused with EDQUOT */
	int		 deferred_now;	/* deferred BO frees outstanding */
	int		 grants_live;	/* grant pager objects alive */
};

/*
 * The device VRAM allocator (vram.c, base module). Arch-neutral: it knows
 * offsets and sizes, and nothing of how a GPU clears memory. Freed VRAM is
 * DIRTY (it may hold another client's pixels) until the registered clear
 * engine has scrubbed it; only then does it go back to the free arena. The
 * clear engine is the per-architecture part (amd: clear.c, a CP DMA fill on a
 * kernel-owned queue), registered by the gpu module with amd_vram_set_clear.
 *
 * Small BAR: VRAM can be far bigger than the CPU's BAR0 window (8 GiB behind
 * 256 MiB on gpusim, like a non-ReBAR discrete card). `total` comes from the
 * device (CONFIG_MEMSIZE), `visible` from BAR0. Nothing the kernel does with
 * VRAM needs the CPU (the scrub is a GPU fill), so ordinary allocations go
 * above the window and keep it free for what must be CPU-visible
 * (AMD_VRAM_CPU_VISIBLE).
 */
struct amd_vram_dirty {
	TAILQ_ENTRY(amd_vram_dirty) link;
	uint64_t	 off;		/* one whole freed allocation */
	uint64_t	 size;
};

/* Clear [off, off+len) of VRAM to zero and wait for it. Sleeps; never called
 * with a lock held. 0, or an errno (the range then stays dirty). */
typedef int amd_vram_clear_fn(struct atrium_amd_softc *sc, uint64_t off,
    uint64_t len);

struct amd_vram_stats {
	uint64_t	 allocs;
	uint64_t	 frees;
	uint64_t	 scrubbed;	/* bytes cleared by the clear engine */
	uint64_t	 scrub_jobs;	/* clear-engine calls */
	uint64_t	 scrub_failed;	/* clear-engine calls that failed */
	uint64_t	 alloc_waits;	/* allocations that waited for a scrub */
	uint64_t	 enomem;	/* allocations refused */
	uint64_t	 peak_used;
};

struct atrium_amd_vram {
	struct mtx	 lock;		/* everything below */
	int		 inited;
	vmem_t		*arena;		/* clean free VRAM (offsets + ARENA_BASE) */
	uint64_t	 total;		/* device VRAM bytes */
	uint64_t	 visible;	/* of which the CPU can reach via BAR0 */
	uint64_t	 used;		/* allocated now */
	uint64_t	 dirty_bytes;	/* freed, not yet scrubbed (incl. in flight) */
	TAILQ_HEAD(, amd_vram_dirty) dirty;
	amd_vram_clear_fn *clear;	/* the clear engine; NULL = none loaded */
	int		 scrubbing;	/* a scrub pass is running */
	uint64_t	 passes;	/* scrub passes finished (waiters sleep on it) */
	struct taskqueue *tq;		/* our own thread: a scrub sleeps */
	struct timeout_task scrub_task;
	struct amd_vram_stats st;
};
#define AMD_VRAM_ARENA_BASE	(1ULL << 40)	/* vmem never hands out 0 */
#define AMD_VRAM_CPU_VISIBLE	0x1	/* amd_vram_alloc: inside the BAR0 window */

/*
 * The amd clear engine (clear.c, gpu module): a compute HQD and a GPUVM
 * context that belong to the kernel alone. VMID 0 is the kernel context on
 * AMD (user VMs get 1..15) and the queue is never handed to a client. Its page
 * directory maps one System page (ring + fence) and a window of VRAM that is
 * re-pointed at each range before the fill. No CPU access to VRAM at all.
 */
#define AMD_CLEAR_QID		2	/* kernel-only compute HQD */
#define AMD_CLEAR_DOORBELL	0x2000	/* its own doorbell page */
#define AMD_CLEAR_VMID		0	/* the kernel GPUVM context */
#define AMD_CLEAR_WIN_PT	32	/* PT pages in the window: 64 MiB */
struct amd_clear_engine {
	struct sx	 lock;		/* one clear at a time; held across reset */
	int		 ready;
	struct atrium_amd_dma_page pdb;
	struct atrium_amd_dma_page pt_misc;	/* maps the ring page */
	struct atrium_amd_dma_page ring;	/* PM4 ring + fence word */
	struct atrium_amd_dma_page pt_win[AMD_CLEAR_WIN_PT];
	uint64_t	 seq;		/* last fence value */
};

/*
 * What identify.c found behind BAR5. ip_* hold the discovered (or, for gpusim,
 * the modelled) version and register segment bases of the blocks the driver
 * uses; present = 0 when the table lists no such block.
 */
#define AMD_HW_GPUSIM		1
#define AMD_HW_SILICON		2
#define AMD_IP_NBASE		8
struct amd_ip_ver {
	uint8_t		present;
	uint8_t		major, minor, rev;
	uint8_t		nbase;
	uint32_t	base[AMD_IP_NBASE];	/* dword segment bases */
};
/* Order matters: rs (base/base_rs.rs) fills the same layout. */
struct amd_ip_info {
	struct amd_ip_ver gc, mp0, mp1, sdma0, mmhub, osssys, hdp, nbif, dmu, vcn;
	uint32_t	nips;			/* IP instances in the table */
};
int	atrium_amd_discovery_parse(const uint8_t *bin, size_t len,
	    struct amd_ip_info *out);		/* Rust, base/base_rs.rs */
int	amd_identify(struct atrium_amd_softc *sc);	/* identify.c */
int	amd_psp_bootload(struct atrium_amd_softc *sc);	/* psp.c, silicon test */
int	amd_smu_init(struct atrium_amd_softc *sc);	/* smu.c, silicon test */

struct atrium_amd_softc {
	device_t	 dev;
	int		 hw_kind;	/* AMD_HW_* (identify.c, before any write) */
	struct amd_ip_info ip;
	uint32_t	 isa_family;	/* CAP_ISA, from GC's version */
	uint32_t	 isa_revision;
	char		 isa_name[16];
	uint32_t	 dcn_seg2_bytes; /* DMU segment 2 byte offset in BAR5 */
	const struct atrium_gpu_backend_ops *backend; /* hardware seam (gpu module) */
	struct resource	*regs;		/* BAR5 MMIO register file */
	int		 regs_rid;
	struct resource	*doorbell;	/* BAR2 doorbell page */
	int		 doorbell_rid;
	struct cdev	*cdev;		/* /dev/atrium-gpu0 (gpu module) */
	struct cdev	*display_cdev;	/* /dev/atrium-display0 (display module) */
	int		energy_member;	/* gpu energy-federation id, -1 = none */
	int		display_energy_member; /* display energy-federation id, -1 = none */

	/*
	 * Vblank knote list: EVFILT_READ knotes registered on /dev/atrium-display0
	 * (display module) hang here, but the GPU module's IH ISR is what fires them
	 * (it sees the vblank interrupt). It lives in the SHARED softc, inited by the
	 * base (pci) module under sc->lock, so the ISR's KNOTE_LOCKED is valid whether
	 * or not the display module is loaded (empty list = harmless no-op walk).
	 */
	struct selinfo	 display_sel;

	struct resource	*msix_table;	/* BAR holding the MSI-X table (BAR4) */
	int		 msix_table_rid;
	struct resource	*irq;		/* MSI-X vector 0 */
	int		 irq_rid;
	void		*intr_cookie;
	int		 msix_enabled;	/* 1 = interrupt mode, 0 = poll mode */

	/*
	 * Device-reset coordination (owned by the base): each IP module registers a
	 * prepare/restore pair so a device-wide FLR quiesces + re-inits every block.
	 */
	uint64_t	 reset_gen;	/* device resets (FLRs) since attach; base reset.c */
	struct atrium_amd_reset_hooks reset_hooks[ATRIUM_AMD_IP_COUNT];

	/*
	 * The display module's state (display_dev.c), per output: what is on
	 * screen and committed, the monitor and the mode. Guarded by sc->lock.
	 */
	struct atrium_amd_display_state *display_state;
	int		 display_opened;	/* exclusive open held */
	/*
	 * The display lease (ISOLATION.md "Display lease"): who may open
	 * /dev/atrium-display0. Unset, only host (prison0) processes may; set
	 * (by jaild, through the root-only sysctl dev.atrium_gpu_amd_display.N.
	 * lease), only processes in the named jail running as the named uid.
	 */
	int		 display_lease_set;
	uid_t		 display_lease_uid;
	char		 display_lease_jail[MAXHOSTNAMELEN];

	/*
	 * Device-global IH (interrupt-handler) ring + dispatch, owned by the BASE
	 * module. The ring is one coherent DMA page the device write-walks; the base
	 * ISR drains it and routes each cookie by cause to ih_handler[cause] (set by
	 * the gpu/display modules under sc->lock). ih_kva/tag/map back the page.
	 */
	void		*ih_kva;	/* interrupt-handler ring (CPU side) */
	bus_dma_tag_t	 ih_tag;	/* bus_dma tag for the ring page */
	bus_dmamap_t	 ih_map;	/* bus_dma map for the ring page */
	uint32_t	 ih_rptr;	/* our read pointer into the IH ring */
	uint64_t	 ih_overflows;	/* passes where the device lapped the ring */
	bus_addr_t	 ih_gpa;	/* the ring's bus address (re-armed on reset) */
	atrium_amd_ih_handler ih_handler[ATRIUM_AMD_IH_NCAUSE]; /* cause -> handler */
	/* This ISR pass's display interrupts per pipe (VBLANK, FLIP, HPD), from
	 * the cookie's IH source and context (DCN_IH_SRC_*). */
	uint32_t	 ih_disp_count[ATRIUM_AMD_IH_NCAUSE][DCN_PIPES];
	u_int		 irq_count;	/* interrupts serviced (atomic vs reader) */
	struct mtx	 lock;		/* guards fence-wait sleep/wakeup + pending */
	int		 lock_inited;
	uint32_t	 ih_eop_ring[ATRIUM_AMD_NHWQ]; /* this ISR pass's EOPs */
	uint32_t	 ih_fault_ring[ATRIUM_AMD_NHWQ]; /* ... and VM faults */
	uint32_t	 ih_fault_vmid[ATRIUM_AMD_NHWQ]; /* last fault's VMID */
	uint32_t	 ih_fault_page[ATRIUM_AMD_NHWQ]; /* last fault's VA >> 12 */

	uint32_t	 vmid_bitmap;	/* allocated VMIDs (bit N = VMID N in use) */
	int		 vm_count;	/* live vm_fds */

	int		 bo_count;	/* live bo_fds; detach refuses if > 0 */
	u_int		 syncobj_count;	/* live syncobj fds; ditto (atomic) */

	/* GPU isolation state (queue.c). */
	struct cdev	*ctl_cdev;	/* /dev/atrium-gpuNctl: reset/sched/powergate */
	int		 hwq_inited;
	struct atrium_amd_dma_page null_pd; /* all-invalid page directory */
	int		 db_fict_registered;
	void		*rs_state;	/* the Rust front-end's (rs/defer.rs) */
	struct timeout_task defer_task;
	struct atrium_amd_isolation_stats iso;
	struct timeout_task watchdog_task;
	int		 hang_ms;	/* no progress this long = hung */
	int		 vram_limit_pct; /* per-uid VRAM cap, % of VRAM (rs/quota.rs) */
	int		 sys_limit_pct;	/* per-uid System BO cap, % of RAM */
	int		 watchdog_armed; /* watchdog_task scheduled (sc->lock) */
	struct atrium_amd_vram vram;	/* device VRAM allocator (base, vram.c) */
	struct amd_clear_engine clear;	/* VRAM clear engine (gpu, clear.c) */
};

/*
 * Leaf MMIO/PM4 mechanics (§3.3 / §7.1 — pure, no driver state beyond the BAR
 * handle, no control flow). Inline in the header: they are one-liners used
 * across every block, so a ring_helpers.c translation unit would be all
 * boilerplate and no substance. The single place raw bus_space access lives.
 */
static inline uint32_t
amd_mmio_read32(struct atrium_amd_softc *sc, bus_size_t reg)
{
	return (bus_read_4(sc->regs, reg));
}

static inline void
amd_mmio_write32(struct atrium_amd_softc *sc, bus_size_t reg, uint32_t val)
{
	bus_write_4(sc->regs, reg, val);
}

/* type[31:30] | (body_dwords-1)[29:16] | opcode[15:8]. */
static inline uint32_t
amd_pm4_type3_header(uint32_t opcode, uint32_t body_dwords)
{
	return ((PM4_TYPE3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) |
	    (opcode << 8));
}

/* bo.c — DMA pages (the GPUVM page tables) + fd-backed buffer objects */
int	 amd_dma_page_alloc(struct atrium_amd_softc *sc,
	    struct atrium_amd_dma_page *p);
void	 amd_dma_page_free(struct atrium_amd_dma_page *p);

/*
 * IH ring + dispatch — owned by the BASE (pci) module (ih.c). amd_ih_init stands
 * up the device-global interrupt ring; amd_irq_setup hooks the ISR that drains
 * it and routes by cause. The gpu/display modules don't call these — they only
 * register a handler for their cause via amd_ih_set_handler (below).
 */
int	 amd_ih_init(struct atrium_amd_softc *sc);
void	 amd_ih_fini(struct atrium_amd_softc *sc);

/*
 * Register/clear the handler for an IH cause. Stored in the shared softc under
 * sc->lock (the ISR reads + calls it under that same lock), so a module sets its
 * handler on attach and clears it (NULL) on detach with no races against a
 * firing interrupt. Inline in the header → no cross-module symbol, each module
 * compiles its own copy (the function it points at is its own).
 */
static inline void
amd_ih_set_handler(struct atrium_amd_softc *sc, int cause,
    atrium_amd_ih_handler fn)
{
	mtx_lock(&sc->lock);
	if (cause >= 0 && cause < ATRIUM_AMD_IH_NCAUSE)
		sc->ih_handler[cause] = fn;
	mtx_unlock(&sc->lock);
}
/* Front-end (Rust, rs/bo.rs). amd_bo_fget holds a file reference the caller
 * drops with fdrop(). */
int	 amd_bo_fget(struct thread *td, int fd, struct file **out_fp,
	    struct atrium_amd_bo **out_bo);
/* amd backend: allocate/free a BO's backing (bus_dma System pages or a VRAM
 * bump); the front-end owns the struct/fd/bindings. */
int	 amd_bo_backing_alloc(struct atrium_amd_softc *sc,
	    struct atrium_amd_bo *bo, uint64_t size, uint32_t flags);
void	 amd_bo_backing_free(struct atrium_amd_bo *bo);
/* amd backend: export a VRAM BO as a {vram_offset, size} scanout handle. */
int	 amd_export_scanout(struct atrium_amd_bo *bo, uint64_t *vram_offset,
	    uint64_t *size);

/* vm.c — the backend's GPUVM: VMIDs, page tables, PTEs. The VM fd object is
 * the front-end's (rs/vm.rs). */
int	 amd_vm_map(struct atrium_amd_vm *vm, uint64_t va, vm_paddr_t phys,
	    int vram, int writable);
void	 amd_vm_unmap(struct atrium_amd_vm *vm, uint64_t va);
/* amd backend hardware setup/teardown of a VM's GPUVM (the front-end owns the
 * struct + fd; these own the VMID + page tables + PT base). */
int	 amd_vm_setup(struct atrium_amd_softc *sc, struct atrium_amd_vm *vm);
void	 amd_vm_teardown(struct atrium_amd_vm *vm);
extern const struct fileops atrium_amd_vm_fileops;

/* gmc.c — GMC bring-up (GPUVM paging enable) */
int	 amd_gmc_init(struct atrium_amd_softc *sc);

/* firmware.c (GPU module) — GFX-block bring-up + the gpu-scoped soft reset */
void	 amd_grbm_soft_reset(struct atrium_amd_softc *sc);
void	 amd_firmware_load(struct atrium_amd_softc *sc);
void	 amd_mes_init(struct atrium_amd_softc *sc);

/*
 * reset.c (BASE module) — the device-wide FLR + its coordinator. amd_flr drives
 * the raw REQ/poll/ACK handshake (used directly for the cold reset at device
 * attach). amd_device_reset is the recovery path: it runs every registered IP's
 * prepare hook, FLRs, then runs every restore hook — so a device-lost reset
 * doesn't corrupt a block (e.g. the display) that didn't ask for it.
 */
int	 amd_flr(struct atrium_amd_softc *sc);
int	 amd_device_reset(struct atrium_amd_softc *sc);

/*
 * vram.c (BASE module) — the device VRAM allocator (struct atrium_amd_vram).
 * VRAM is a *device* resource (the gpu's BOs today, a display cursor/overlay
 * plane tomorrow), so the base owns it. Offsets are bytes into device VRAM.
 *   alloc: clean VRAM only; waits (bounded) for a scrub in progress; ENOMEM.
 *   free:  the range becomes dirty and is scrubbed before anyone gets it again.
 *   set_clear: the gpu module registers its clear engine (NULL on detach,
 *          which waits out a running scrub).
 */
int	 amd_vram_init(struct atrium_amd_softc *sc);
void	 amd_vram_fini(struct atrium_amd_softc *sc);
int	 amd_vram_alloc(struct atrium_amd_softc *sc, uint64_t size, int flags,
	    uint64_t *out_off);
void	 amd_vram_free(struct atrium_amd_softc *sc, uint64_t off, uint64_t size);
void	 amd_vram_set_clear(struct atrium_amd_softc *sc, amd_vram_clear_fn *fn);

/* clear.c (gpu module) — the amd VRAM clear engine (struct amd_clear_engine) */
int	 amd_clear_init(struct atrium_amd_softc *sc);
void	 amd_clear_fini(struct atrium_amd_softc *sc);
void	 amd_clear_reset_prepare(struct atrium_amd_softc *sc);
void	 amd_clear_reset_restore(struct atrium_amd_softc *sc);

/*
 * Register an IP module's reset prepare/restore hooks (NULL to clear on detach).
 * Stored in the shared softc under sc->lock; the coordinator snapshots them, so
 * a module clears its hooks on detach with no race against a concurrent reset.
 * Inline → no cross-module symbol (the hooks point at the registering module).
 */
static inline void
amd_reset_register(struct atrium_amd_softc *sc, int ip,
    void (*prepare)(struct atrium_amd_softc *),
    void (*restore)(struct atrium_amd_softc *))
{
	mtx_lock(&sc->lock);
	if (ip >= 0 && ip < ATRIUM_AMD_IP_COUNT) {
		sc->reset_hooks[ip].prepare = prepare;
		sc->reset_hooks[ip].restore = restore;
	}
	mtx_unlock(&sc->lock);
}

/* cp.c — submission (under a VM's VMID) */
int	 amd_queue_program(struct atrium_amd_softc *sc, uint64_t ring_va,
	    uint32_t engine, uint16_t vmid, uint32_t *doorbell_off);
void	 amd_queue_stop(struct atrium_amd_softc *sc, uint32_t qid);

/* queue.c — the C half of queue management: timers, sysctls, the helpers
 * rs/queue.rs calls (rs/glue.h). */
int	 amd_hwq_init(struct atrium_amd_softc *sc, device_t dev);
void	 amd_hwq_fini(struct atrium_amd_softc *sc);
int	 amd_hwq_busy_refs(struct atrium_amd_softc *sc);
void	 amd_hwq_defer_kick(struct atrium_amd_softc *sc);

/* rs/queue.rs — queue ownership, submit, the hang check, reset (GPU
 * isolation, review §A3 breaks 1 and 3, §A2.6): Rust, called from C. */
int	 amd_hwq_submit(struct atrium_amd_softc *sc,
	    struct atrium_amd_client *client, uint64_t ring_va,
	    uint32_t n_dwords, uint32_t engine, struct atrium_amd_vm *vm,
	    uint32_t deadline_ns, struct atrium_amd_syncobj *so,
	    uint64_t signal_value);
int	 amd_hwq_map(struct atrium_amd_softc *sc,
	    struct atrium_amd_client *client, struct atrium_amd_vm *vm,
	    uint64_t ring_va, uint32_t engine, uint32_t *doorbell_off);
int	 amd_hwq_mmap_single(struct atrium_amd_softc *sc,
	    struct atrium_amd_client *client, vm_ooffset_t *offset,
	    vm_size_t size, vm_object_t *object, int nprot);
void	 amd_hwq_client_close(struct atrium_amd_softc *sc,
	    struct atrium_amd_client *client);
void	 amd_hwq_vm_gone(struct atrium_amd_softc *sc, uint16_t vmid);
void	 amd_hwq_reset_prepare(struct atrium_amd_softc *sc);
void	 amd_hwq_reset_restore(struct atrium_amd_softc *sc);

/* cp.c — firmware energy-fair scheduler (the kernel programs weights) */
struct atrium_gpu_sched;
void	 amd_sched(struct atrium_amd_softc *sc, struct atrium_gpu_sched *s);

/* pgate.c — power-gate idle IP blocks (the driver-controlled lever) */
struct atrium_gpu_powergate;
int	 amd_powergate(struct atrium_amd_softc *sc, struct atrium_gpu_powergate *p);

/*
 * The display block (APER_DISP register helpers + the /dev/atrium-display0
 * cdev) lives in a SEPARATE module, atrium_gpu_amd_display.ko (display/), which
 * shares this softc + the regDISP_* defs + the mmio accessors above but has its
 * own newbus child and ABI (atrium_display_abi.h). The GPU module does not
 * reference it.
 */

/*
 * mmap offset (passed to mmap() on the device fd) that maps the BAR2 doorbell
 * page into userspace for user-mode-queue submission. One page; the queue's
 * doorbell lives at its byte offset within it.
 */
#define ATRIUM_AMD_DOORBELL_MMAP_OFF	0

/* Timeline syncobj fd, kqueue-able (Rust, rs/syncobj.rs). signal and fail
 * may be called with sc->lock held (the order is sc->lock, then the
 * syncobj's own lock). */
int	 amd_syncobj_fget(struct thread *td, int fd, struct file **out_fp,
	    struct atrium_amd_syncobj **out_so);
void	 amd_syncobj_signal(struct atrium_amd_syncobj *so, uint64_t value);
void	 amd_syncobj_fail(struct atrium_amd_syncobj *so, int error);
void	 amd_syncobj_watch(struct atrium_amd_syncobj *so, void (*fn)(void *),
	    void *arg);
int	 amd_syncobj_ready(struct atrium_amd_syncobj *so, uint64_t value);

/* ih.c (BASE module) — MSI-X interrupt setup + the device-global IH ring ISR */
int	 amd_irq_setup(struct atrium_amd_softc *sc);
void	 amd_ih_rearm(struct atrium_amd_softc *sc);
void	 amd_irq_teardown(struct atrium_amd_softc *sc);

/*
 * irq.c (GPU module) — the EOP and VM-fault cause handlers. The base ISR
 * routes IH_CAUSE_EOP and IH_CAUSE_VM_FAULT here (amd_ih_set_handler), with
 * sc->ih_eop_ring[] / ih_fault_ring[] holding this pass's counts per queue.
 * Called with sc->lock HELD. The completion lists they retire are Rust
 * (rs/pending.rs, amd_rs_eop).
 */
void	 amd_eop_handler(struct atrium_amd_softc *sc, int count);
void	 amd_fault_handler(struct atrium_amd_softc *sc, int count);
void	 amd_hwq_kick_watchdog(struct atrium_amd_softc *sc);

/* glue.c — the client and control cdevsws (entry points in rs/ioctl.rs) */
extern struct cdevsw atrium_amd_cdevsw;
extern struct cdevsw atrium_amd_ctl_cdevsw;

#endif /* _ATRIUM_GPU_AMD_H_ */
