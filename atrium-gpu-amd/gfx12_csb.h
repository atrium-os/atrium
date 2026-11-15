/*
 * gfx12_csb.h: the GFX12 clear-state buffer (RLC_CSIB), as
 * gfx_v12_0_get_csb_buffer builds it from Linux clearstate_gfx12.h:
 * cluster count, then per extent (register count, first context register,
 * values). Generated from that header, whose notice follows.
 */
/*
 * Copyright 2023 Advanced Micro Devices, Inc.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
 * THE COPYRIGHT HOLDER(S) OR AUTHOR(S) BE LIABLE FOR ANY CLAIM, DAMAGES OR
 * OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
 * ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
 * OTHER DEALINGS IN THE SOFTWARE.
 *
 */
#ifndef ATRIUM_GFX12_CSB_H
#define ATRIUM_GFX12_CSB_H

static const uint32_t gfx12_csb[] = {
	6,	/* clusters */
	34, 0x0000a03e,
	0x00000000,	/* mmSC_MEM_TEMPORAL */
	0x00000000,	/* mmSC_MEM_SPEC_READ */
	0x00000000,	/* mmPA_SC_VPORT_0_TL */
	0x00000000,	/* mmPA_SC_VPORT_0_BR */
	0x00000000,	/* mmPA_SC_VPORT_1_TL */
	0x00000000,	/* mmPA_SC_VPORT_1_BR */
	0x00000000,	/* mmPA_SC_VPORT_2_TL */
	0x00000000,	/* mmPA_SC_VPORT_2_BR */
	0x00000000,	/* mmPA_SC_VPORT_3_TL */
	0x00000000,	/* mmPA_SC_VPORT_3_BR */
	0x00000000,	/* mmPA_SC_VPORT_4_TL */
	0x00000000,	/* mmPA_SC_VPORT_4_BR */
	0x00000000,	/* mmPA_SC_VPORT_5_TL */
	0x00000000,	/* mmPA_SC_VPORT_5_BR */
	0x00000000,	/* mmPA_SC_VPORT_6_TL */
	0x00000000,	/* mmPA_SC_VPORT_6_BR */
	0x00000000,	/* mmPA_SC_VPORT_7_TL */
	0x00000000,	/* mmPA_SC_VPORT_7_BR */
	0x00000000,	/* mmPA_SC_VPORT_8_TL */
	0x00000000,	/* mmPA_SC_VPORT_8_BR */
	0x00000000,	/* mmPA_SC_VPORT_9_TL */
	0x00000000,	/* mmPA_SC_VPORT_9_BR */
	0x00000000,	/* mmPA_SC_VPORT_10_TL */
	0x00000000,	/* mmPA_SC_VPORT_10_BR */
	0x00000000,	/* mmPA_SC_VPORT_11_TL */
	0x00000000,	/* mmPA_SC_VPORT_11_BR */
	0x00000000,	/* mmPA_SC_VPORT_12_TL */
	0x00000000,	/* mmPA_SC_VPORT_12_BR */
	0x00000000,	/* mmPA_SC_VPORT_13_TL */
	0x00000000,	/* mmPA_SC_VPORT_13_BR */
	0x00000000,	/* mmPA_SC_VPORT_14_TL */
	0x00000000,	/* mmPA_SC_VPORT_14_BR */
	0x00000000,	/* mmPA_SC_VPORT_15_TL */
	0x00000000,	/* mmPA_SC_VPORT_15_BR */
	2, 0x0000a0cc,
	0x00000000,	/* mmPA_CL_PROG_NEAR_CLIP_Z */
	0x00000000,	/* mmPA_RATE_CNTL */
	1, 0x0000a0d8,
	0x00000000,	/* mmCP_PERFMON_CNTX_CNTL */
	6, 0x0000a0db,
	0x00000000,	/* mmCONTEXT_RESERVED_REG0 */
	0x00000000,	/* mmCONTEXT_RESERVED_REG1 */
	0x00000000,	/* mmPA_SC_CLIPRECT_0_EXT */
	0x00000000,	/* mmPA_SC_CLIPRECT_1_EXT */
	0x00000000,	/* mmPA_SC_CLIPRECT_2_EXT */
	0x00000000,	/* mmPA_SC_CLIPRECT_3_EXT */
	11, 0x0000a2e5,
	0x00000000,	/* mmPA_SC_HIZ_INFO */
	0x00000000,	/* mmPA_SC_HIS_INFO */
	0x00000000,	/* mmPA_SC_HIZ_BASE */
	0x00000000,	/* mmPA_SC_HIZ_BASE_EXT */
	0x00000000,	/* mmPA_SC_HIZ_SIZE_XY */
	0x00000000,	/* mmPA_SC_HIS_BASE */
	0x00000000,	/* mmPA_SC_HIS_BASE_EXT */
	0x00000000,	/* mmPA_SC_HIS_SIZE_XY */
	0x00000000,	/* mmPA_SC_BINNER_OUTPUT_TIMEOUT_CNTL */
	0x00000000,	/* mmPA_SC_BINNER_DYNAMIC_BATCH_LIMIT */
	0x00000000,	/* mmPA_SC_HISZ_CONTROL */
	8, 0x0000a3c0,
	0x00000000,	/* mmCB_MEM0_INFO */
	0x00000000,	/* mmCB_MEM1_INFO */
	0x00000000,	/* mmCB_MEM2_INFO */
	0x00000000,	/* mmCB_MEM3_INFO */
	0x00000000,	/* mmCB_MEM4_INFO */
	0x00000000,	/* mmCB_MEM5_INFO */
	0x00000000,	/* mmCB_MEM6_INFO */
	0x00000000,	/* mmCB_MEM7_INFO */
};

#endif
