/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * Aura: the interface between the Wi-Fi kernel core (aura_wlan.ko) and the
 * hardware drivers (aura_mt79, aura_intel, aura_sim). docs/spec/wifi.md §3.
 *
 * Control requests reach the driver as validated aura-proto messages (the
 * same bytes aura-mlme and aura-keys write, header included), so the C ABI
 * between modules is that one versioned format. The core holds the radio
 * lock around ops->request: a driver must not sleep there and must not call
 * back into the core from it (queue the work instead).
 */
#ifndef _AURA_WLAN_H_
#define _AURA_WLAN_H_

#include <sys/types.h>

#define	AURA_WLAN_ABI		1
#define	AURA_HEADER_LEN		16
#define	AURA_MAX_BODY		8192
#define	AURA_MSG_MAX		(AURA_HEADER_LEN + AURA_MAX_BODY)
#define	AURA_MAX_CHANNELS	128
/* Largest 802.11 data frame a driver hands up (an 11454-byte VHT MPDU). */
#define	AURA_MAX_RX_FRAME	11454

struct mbuf;
struct aura_wlan;

struct aura_chan {
	uint16_t	freq;		/* MHz */
	uint16_t	flags;		/* aura_proto::chan_flags */
	int16_t		max_power_mbm;
};

struct aura_caps {
	uint8_t		mac[6];
	uint8_t		bands;		/* aura_proto::bands */
	uint8_t		max_scan_ssids;
	uint32_t	ciphers;	/* bit per 00-0F-AC suite type */
	uint32_t	flags;		/* aura_proto::caps_flags */
	uint16_t	nchan;
	struct aura_chan chan[AURA_MAX_CHANNELS];
};

/* What the hardware says about one received data frame. */
struct aura_rx_info {
	uint8_t		decrypted;	/* protected, decrypted, MIC checked, IV/MIC removed */
	uint8_t		key_idx;
	uint64_t	pn;
};

struct aura_driver_ops {
	int	abi;			/* AURA_WLAN_ABI */
	/* A validated control request. 0 or an errno. Lock held; no sleep. */
	int	(*request)(void *drv, const uint8_t *msg, size_t len);
	/*
	 * Send one 802.11 data frame (header and LLC/SNAP prepended by the
	 * core). Consumes m. encrypt: with the installed pairwise key (or the
	 * group key for group-addressed frames). Called without the lock.
	 */
	void	(*tx)(void *drv, struct mbuf *m, uint8_t tid, bool encrypt);
};

int	aura_wlan_attach(void *drv, const struct aura_driver_ops *ops,
	    const struct aura_caps *caps, struct aura_wlan **out);
void	aura_wlan_detach(struct aura_wlan *w);

/* Received management frame (header + body, no FCS): to aura-mlme. */
void	aura_wlan_rx_mgmt(struct aura_wlan *w, uint16_t freq, int8_t rssi,
	    uint8_t flags, uint64_t tsf_us, const uint8_t *frame, size_t len);
/* Received data frame (no FCS; crypto header/MIC removed if decrypted). */
void	aura_wlan_rx_data(struct aura_wlan *w, const uint8_t *frame,
	    size_t len, const struct aura_rx_info *info);
/*
 * An event for the daemons, as an aura-proto message (ScanDone,
 * Disassociated, BeaconLoss, RfKill, FirmwareError, Stats). The core
 * updates its state and forwards it to the channels that take it.
 */
void	aura_wlan_event(struct aura_wlan *w, const uint8_t *msg, size_t len);

#endif /* !_AURA_WLAN_H_ */
