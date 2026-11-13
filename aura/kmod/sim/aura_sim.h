/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * /dev/aura/sim0: the pipe between aura_sim.ko and the user-space bridge.
 * Each read(2)/write(2) is one message: this header, then `len` bytes.
 * Little-endian, as aura-proto.
 */
#ifndef _AURA_SIM_H_
#define _AURA_SIM_H_

#include <sys/types.h>

struct aura_sim_hdr {
	uint32_t	type;
	uint32_t	len;
};

/* radio -> bridge */
#define	AURA_SIM_REQ		1	/* a validated aura-proto request */
#define	AURA_SIM_TX		2	/* aura_sim_tx + 802.11 data frame */
/* bridge -> radio */
#define	AURA_SIM_RX_MGMT	3	/* aura_sim_rx_mgmt + management frame */
#define	AURA_SIM_RX_DATA	4	/* aura_sim_rx_data + data frame */
#define	AURA_SIM_EVENT		5	/* an aura-proto event (ScanDone, ...) */

struct aura_sim_tx {
	uint8_t		tid;
	uint8_t		encrypt;
	uint8_t		pad[6];
};

struct aura_sim_rx_mgmt {
	uint16_t	freq;
	int8_t		rssi;
	uint8_t		flags;
	uint32_t	pad;
	uint64_t	tsf_us;
};

struct aura_sim_rx_data {
	uint8_t		decrypted;
	uint8_t		key_idx;
	uint8_t		pad[6];
	uint64_t	pn;
};

#define	AURA_SIM_MAX	(sizeof(struct aura_sim_hdr) + 16 + 11454)

#endif
