# Wi-Fi: Atrium's own drivers, MAC layer and supplicant

**Status:** proposal 2026-11-13, decisions in §10 open.
**Scope:** Wi-Fi station support for Atrium: kernel drivers per chip family,
the 802.11 MAC layer, the supplicant (WPA2/WPA3), regulatory, firmware, and
how atrium-net, Arca, dispensatord and jaild fit around them.
**Background:** Shankar 2026-11-13: Atrium writes its own Wi-Fi drivers, as it
does for the GPU; FreeBSD's linuxkpi `iwlwifi`, `rtw88` and `rtw89` are not
used ([device-assignment.md](device-assignment.md) §7).

## 1. Short answer

- **Drivers:** one Rust kernel module per chip family, with thin C newbus
  glue, built like the GPU driver. Intel first (AX200/AX201/AX210/AX211,
  firmware "MVM" interface), Intel BE200 (Wi-Fi 7) second, then one of
  MediaTek MT7921/MT7922 or Realtek RTL8852 depending on the hardware we have.
- **MAC layer:** do not build on net80211. Split the MAC the way modern
  offload firmware already splits it: the kernel keeps only the data path
  (rings, 802.11↔Ethernet data frames, keys, reorder), and every management
  frame (scan results, auth, association, action frames, information
  elements) goes to a **jailed user-space Rust daemon**. That puts the
  over-the-air parsers out of the kernel, which is the D9 rule ("new
  hostile-input parsers go to jailed user-space daemons first").
- **Supplicant:** our own, in Rust, inside that same daemon: WPA2-Personal,
  WPA3-Personal (SAE with H2E), PMF (802.11w) in v1; Enterprise (EAP) later.
  No `wpa_supplicant`. Passwords live in Arca.
- **Firmware:** vendor redistributable binaries only, shipped in signed
  Opifex packages under `/boot/firmware/<vendor>/`, never disassembled
  (same rule as amdgpu).
- **Testing:** a simulated radio (`aura_sim`) bridged to Linux
  `mac80211_hwsim` + `hostapd`, so the MAC and supplicant are tested against
  the reference AP before any silicon; then real cards.
- **Proposed name:** **Aura** (Latin: breeze, the moving air). `aurad` is the
  daemon; kernel modules are `aura_intel`, `aura_mt79`, `aura_rtw89`,
  `aura_sim`.

The size is honest: this is the GPU driver's order of effort, not the
Bluetooth one. The first family plus the MAC and supplicant is the bulk; each
later family is mostly transport and firmware-command work.

## 2. Why not FreeBSD's stack

| Piece | FreeBSD today | Problem for Atrium |
|---|---|---|
| `iwlwifi`, `rtw88`, `rtw89`, `mt76` | Linux drivers under linuxkpi + LinuxKPI 802.11 shim over net80211 | Shankar's decision; also a large C translation layer in the kernel, regressions on the wireless list through 2026 |
| `iwx(4)` | Native port of OpenBSD's driver, in 15.0 (March 2025) | No 802.11ax, no hardware crypto offload ([iwx(4)](https://man.freebsd.org/iwx/4)); useful as a **reference** (ISC licence) |
| `iwm(4)` | Native, older Intel (7260–9560) | Reference only; parts we won't target first |
| net80211 | Mature C MAC: scan, state machine, IE parsing, software crypto, ifconfig integration | No MFP/802.11w (groundwork posted June 2026), no WPA3-SAE path, HE/11ax only definitions ([status report 2025Q2](https://www.freebsd.org/status/report-2025-04-2025-06/lkpi-wireless), [wireless list 2026-06](https://lists.freebsd.org/archives/freebsd-wireless/2026-June/)). Built for soft-MAC chips; modern Intel/MediaTek firmware does scan, rate control and much of the MLME itself, so the fit is poor. All IE parsing in kernel C. |
| `wpa_supplicant` | Ports/base, C, root, talks to net80211 via ioctl | C parser of hostile frames running as root; WPA3 needs driver support net80211 lacks |

Keeping net80211 would mean writing new drivers against a MAC layer that
cannot yet do what WPA3 networks require (PMF is mandatory for WPA3), and
keeping a large hostile-input parser in kernel C. Porting net80211 to Rust
is the worst of both: all its size, none of the split.

## 3. Architecture

```
  Forum Wi-Fi picker / settings
          │ aqueduct (JoinWifi, ListWifiNetworks; network.md §9)
  atrium-net  (root, narrow; DHCP, routes, resolv.conf)
          │ aqueduct, one socket
  aurad  (jailed, uid _aura, no network, no filesystem but its state dir)
    ├─ MLME: scan policy, BSS selection, auth/assoc, roaming, PS policy
    ├─ IE / management-frame parsers (Rust, fuzzed)
    ├─ supplicant: 4-way + group handshake, SAE, PMF keys, later EAP
    ├─ regulatory: wireless-regdb, country selection
    └─ credentials: Arca client (passphrase/SAE password, per network)
          │ /dev/aura/ctlN (granted by dispensatord, jaild devfs_grant)
  ─────────────────────────────── kernel ─────────────────────────────────
  aura_wlan  (Rust, shared by all families)
    ├─ control channel: typed, versioned message protocol on the cdev
    ├─ ifnet wlanN as an Ethernet interface (pf, DHCP, IPv6 unchanged)
    ├─ controlled port: only EAPOL until aurad says "authorized"
    ├─ data path: 802.11 data ↔ 802.3, A-MSDU checks, reorder, TX queues
    └─ key table → hardware crypto (CCMP-128, GCMP-256, BIP for PMF)
  aura_intel / aura_mt79 / aura_rtw89 / aura_sim   (Rust + C newbus glue)
    └─ PCIe/USB transport, firmware load, command/notification rings, IRQs
```

### 3.1 The kernel/user split

What stays in the kernel is what must be fast or is fixed-format:

- DMA rings, interrupts, firmware load and the firmware command channel.
- Data frames: header conversion, A-MPDU reorder (where firmware doesn't do
  it), A-MSDU deaggregation with the FragAttacks mitigations (drop non-SPP
  A-MSDU whose first subframe looks like an LLC/SNAP header, never mix
  fragments across keys, flush fragment cache on rekey).
- Key install and the controlled port.
- Power-save and rfkill mechanics the firmware needs answered in time.

What moves to `aurad`: every management frame body, every information
element, scan result handling, BSS selection, roaming decisions, the
handshakes, regulatory policy. The driver hands management frames (and
firmware scan reports, which carry raw beacons/probe responses) up the
control channel as opaque bytes plus a fixed RX header; it never parses an
IE. Firmware-handled MLME steps (Intel's firmware sends auth/assoc frames
itself in MVM) are driven by commands that `aurad` issues with parameters
it already validated.

This is the Fuchsia WLAN shape (user-space MLME in Rust over thin drivers),
and close to Linux's split for full-MAC chips; it is not new territory.

### 3.2 Control channel

A character device per radio, `/dev/aura/ctlN`, carrying length-prefixed
messages of a versioned binary protocol defined once in a `no_std` crate
(`aura-proto`) used by both sides. Requests: `Scan`, `Join`, `Auth`,
`Assoc`, `SetKey`, `DelKey`, `PortAuthorized`, `SetPowerSave`,
`SetRegulatory`, `SetMac`, `TxMgmt`, `TxEapol`. Events: `MgmtRx`,
`EapolRx`, `ScanDone`, `Disassociated`, `BeaconLoss`, `RfKill`,
`FirmwareError`, `Stats`. One opener at a time (EBUSY otherwise); closing it
disassociates and clears keys.

Why a cdev and not netlink: it fits device assignment as built (dispensatord
grants one node to one jail, default deny), needs no routing-socket rights in
the jail, and the jail's sysfil profile is just `read`/`write`/`kevent` on
one fd. `ifconfig wlan0 ssid …` does not work; atrium-net is the only
configuration path. `ifconfig` still shows the interface, addresses and
link state.

### 3.3 Kernel module structure

- `aura_wlan`: the shared Rust core above (no_std, in-kernel), exported to
  family modules through a Rust trait (`Radio`) plus a small C seam for the
  ifnet/mbuf calls where the kpi bindings don't reach yet.
- Per family: Rust for firmware-file parsing (TLV sections: new code, D2),
  command/notification structs, ring management and op logic; C only for
  `DRIVER_MODULE`/newbus attach, MSI-X setup, `bus_dma` tag creation, as in
  `atrium-gpu-amd`'s `glue.c`.
- New kpi bindings needed (patches after 0012 in `kmod-rust-spike/kpi-patches`):
  `bus_dma` maps/syncs, MSI-X, `firmware(9)`, `mbuf` (chains, `m_pullup`,
  `m_defrag`), `ifnet` (alloc/attach/if_input/if_transmit), `taskqueue`.
  The GPU work deferred the PCI/MSI-X/firmware bindings; Wi-Fi needs them, so
  they get built and then serve the GPU too.
- IOMMU: on amd64 enable DMAR for the Wi-Fi device when the platform has it.
  The card is a bus master running vendor firmware that parses radio frames;
  treat firmware as semi-trusted and every length in a notification as
  hostile (Rust bounds checks, no unchecked `from_raw_parts`).

### 3.4 aurad

- One process per machine (not per radio), jailed through jaild: uid
  `_aura` (new reserved uid next to Vinculum's), devfs ruleset with only
  `/dev/aura/ctl*` grants, no network, sysfil profile `device`.
- Starts before login (machine networks must join at boot), so credentials
  for machine-wide networks need the Arca **machine store** (arca.md §6.4),
  with a new kind `WifiCredential`; per-user networks stay in the user's
  store and join after login. Decision 7.
- Crypto: RustCrypto (aes, aes-gcm, cmac, hmac, sha1/sha2, p256/p384 for
  SAE), constant-time where secrets are involved. Fuchsia's `wlan-rsn` and
  `wlan-sae` crates (BSD-3) are a reference and possibly a starting point;
  checked against IEEE 802.11-2020 Annex J test vectors and hostapd interop.
- Policy defaults: refuse WEP and TKIP outright; WPA3 preferred, WPA2 and
  WPA2/WPA3 transition allowed; PMF "capable" by default, "required" for
  WPA3; open networks allowed with a Forum warning; OWE later. Per-network
  randomized MAC (stable per SSID, like Android/iOS "private address") and a
  random MAC for every scan; hidden-network directed probes only for networks
  the user marked hidden. Decision 8.

### 3.5 atrium-net

network.md §9 already gives Wi-Fi to `atrium-net`, the request-driven
daemon (not `atrium-netd`, which reconciles pf for jails). Change there:
`atrium-net` talks to `aurad` instead of writing `wpa_supplicant.conf`, and
runs DHCP on `wlanN` when `aurad` reports the port authorized. The
`network_wifi` capability and `JoinWifi`/`ListWifiNetworks` RPCs stay.

### 3.6 Regulatory

- Database: wireless-regdb (`db.txt`, ISC licence) compiled into a signed
  table shipped with `aurad`.
- Country: user setting, else the firmware's location-aware result (Intel
  LAR/MCC updates), else world-safe ("00"). An AP's Country IE may only
  tighten rules, never loosen them.
- The kernel enforces the final channel/power list it was given; `aurad`
  cannot ask for a channel outside it, and firmware limits apply on top.

### 3.7 Firmware

As device-assignment.md §4.4: raw files under `/boot/firmware/<vendor>/`,
loaded with `firmware_get_flags(…, FIRMWARE_GET_NOWARN)`; Intel
`iwlwifi-*.ucode` plus `.pnvm` for AX210 and later; MediaTek and Realtek
files from linux-firmware. Each package carries its licence text. All three
vendors' Wi-Fi firmware licences are redistributable-binary,
no-reverse-engineering; to be confirmed file by file before each package
ships (as for amdgpu-firmware-license).

## 4. Where the knowledge comes from

Intel, MediaTek and Realtek publish no host-interface documentation; the
drivers are the documentation. Proposed rule (decision 4):

- **Allowed references:** code under permissive licences: OpenBSD `iwx`/
  `iwm` (ISC), FreeBSD `iwx`/`iwm`, Linux `iwlwifi` under its BSD-3-Clause
  option (it is dual GPL-2.0/BSD-3), `mt76` (ISC), `rtw89` under its
  BSD-3-Clause option; Fuchsia WLAN (BSD-3); public standards (IEEE
  802.11-2020/2024, WPA3 spec, RFCs).
- **Not allowed:** GPL-only code, firmware disassembly, leaked documents.
- Files that carry structures or sequences taken from a reference keep the
  reference's copyright notice and licence in the header (BSD/ISC require
  it), and the commit names the reference file and version.

This is looser than gpusim's clean-room rule, deliberately: for the GPU there
are public ISA and register documents; for Wi-Fi there is nothing else.

## 5. Chip families

| Order | Family | Parts | Interface | Notes |
|---|---|---|---|---|
| 1 | Intel 22000 / AX210 gen | AX200, AX201 (CNVi), AX210, AX211 (CNVi), AX411 | PCIe, gen2 transport, MVM firmware API | most common in recent laptops and M.2 cards; OpenBSD/FreeBSD `iwx` is a clean reference for the transport |
| 2 | Intel BE20x | BE200, BE201, BE202 | PCIe, newer firmware API (Linux moved to the separate MLD op mode) | Wi-Fi 7 without MLO first; MLO is a large MAC feature, later |
| 3a | MediaTek MT7921/MT7922/MT7925 | AMD laptops (RZ608/RZ616), USB MT7921AU dongles | PCIe and USB | ISC reference (`mt76`); the USB variant can be passed into the dev VM (§6) |
| 3b | Realtek RTL8852BE/CE, RTL8922AE | budget laptops, mini PCs | PCIe | `rtw89` BSD-3 option |

### 5.1 Hardware available (Shankar, 2026-11-13)

Shankar can get a USB Wi-Fi module and plans to buy a Minisforum AI X1 Pro.
Reviews list its Wi-Fi card as a **MediaTek MT7925** (Wi-Fi 7, PCIe, M.2,
connac3 firmware); to be confirmed by PCI ID when it arrives. That reverses
the order above: MediaTek goes first.

- **USB dongle: MT7921AU** (Wi-Fi 6E, connac2), e.g. Alfa AWUS036AXML or
  Netgear A8000. Passes into the arm64 QEMU VM on the Mac by USB
  passthrough, so the driver can be developed before any bare-metal box.
- **MT7921 and MT7925 share most of the host interface** (`mt76`'s common
  `mt792x` code, ISC): MCU message framing, firmware patch + RAM load, WFDMA
  rings on PCIe, the same USB transport style. One `aura_mt79` module covers
  both, with the bus (USB/PCIe) and the connac2/connac3 differences as layers.
- **AI X1 Pro:** x86_64, MT7925 on PCIe; runs Atrium bare metal for the
  throughput and soak gates. Its M.2 card can later be swapped for an Intel
  BE200 to bring up `aura_intel` on the same box.

Older Intel (7260–9560) stays out unless someone needs it: `iwm` covers them
in FreeBSD and that hardware is going away.

## 6. Testing

1. **Unit and fuzz:** `aura-proto`, every IE/frame parser, the handshakes
   (Annex J vectors), firmware TLV parsing; cargo-fuzz targets, as in
   Vinculum.
2. **Simulated radio `aura_sim`:** a kmod implementing the `Radio` trait with
   no hardware, exchanging frames over a UDP socket with a bridge on a Linux
   host running `mac80211_hwsim` + `hostapd` (hwsim forwards frames to user
   space over netlink, the same path wmediumd uses). That tests MLME,
   supplicant, PMF, SAE/H2E, roaming and later EAP against the reference AP,
   in the existing arm64 dev VM, with an Ubuntu VM beside it on the Mac (or
   in the cloud container's VM setup). Gate: WPA2, WPA3, transition mode, PMF
   required, rekey, roam between two BSSs, deauth flood ignored with PMF.
3. **Real hardware:** an Intel card in an x86_64 machine running Atrium bare
   metal, an AP we control (ideally one that can do WPA2, WPA3 and 6 GHz),
   iperf3 against a wired host. Target: within 15% of Linux `iwlwifi` on the
   same machine and AP for TCP throughput, and no loss of association across
   a 24 h soak.
4. **VM path for hardware:** PCIe passthrough needs VFIO on a Linux/KVM host
   (not the Mac). A USB MT7921AU dongle can go into a QEMU VM on the Mac by
   USB passthrough, which makes MediaTek the only family testable without a
   spare bare-metal box. That is the argument for 3a early if no Intel test
   machine is available.

## 7. Build order

| Step | What | Done when |
|---|---|---|
| W0 | `aura-proto`, `aura_wlan` skeleton, kpi bindings (bus_dma, MSI-X, firmware, mbuf, ifnet), `aura_sim` + hwsim bridge | `wlan0` appears in the VM and passes Ethernet frames to the sim |
| W1 | `aurad`: scan, open + WPA2-PSK + PMF, Arca credentials, jail, atrium-net glue, DHCP | VM joins hwsim `hostapd` WPA2 and gets an address |
| W2 | `aurad`: WPA3-SAE (H2E + hunting-and-pecking), transition mode, roaming, MAC randomization, regdb | sim gate (§6.2) green |
| W3 | `aura_mt79` USB transport: MCU messages, patch + RAM firmware load, hardware scan | MT7921AU in the Mac VM scans and reports BSSs to `aurad` |
| W4 | `aura_mt79` station: BSS/STA records, TX/RX, HW crypto, firmware rate control, BA sessions, PS, country/regulatory; then the PCIe (WFDMA) bus layer and MT7925 on the AI X1 Pro | real AP joined at HE/EHT rates, iperf target met on the AI X1 Pro |
| W5 | suspend/resume, rfkill, firmware crash recovery (reload + rejoin), 24 h soak | soak green |
| W6 | second family: `aura_intel` (BE200 swapped into the AI X1 Pro, or AX210) | same gates |
| W7 | Enterprise: EAP-TLS, PEAP-MSCHAPv2, EAP-TTLS; certificates through Arca | interop with hostapd's EAP server |

W0–W2 need no Wi-Fi hardware at all.

## 8. Security notes

- Attack surface over the air ends in `aurad` (jailed, no network, Rust,
  fuzzed) for management frames, and in `aura_wlan` (Rust) for data-frame
  headers only. Nothing in kernel C parses radio input.
- The firmware is a DMA master: IOMMU where available; ring indices and
  lengths from the device are bounds-checked; a firmware error notification
  resets the device, never trusts its contents beyond logging.
- `aurad` compromise gives: the ability to join/leave networks and see
  management frames and EAPOL; not the data path, not other jails' traffic,
  not Arca secrets of networks it isn't joining (Arca releases one
  credential per join request from atrium-net).
- Downgrade: never fall back from SAE to PSK on a network remembered as
  WPA3 (WPA3 transition-disable indication honoured).

## 9. Not in scope

AP/hotspot mode, monitor mode, mesh, Wi-Fi Direct/P2P, NAN, MLO, Passpoint,
WEP/TKIP, older Intel (iwm parts), Broadcom (no permissively-licensed modern
reference and poor firmware terms). AP mode for tethering is the likeliest
later addition; the split in §3.1 allows it (hostapd's role would move into
`aurad`).

## 10. Decisions

1. **Own split MAC, not net80211.** Kernel keeps the data path; all
   management frames and IEs go to jailed `aurad`. *Recommend yes* (§2, §3.1).
2. **Control interface is a cdev with a versioned message protocol**, granted
   by dispensatord; no `ifconfig` Wi-Fi configuration. *Recommend yes* (§3.2).
3. **Rust kmods with thin C newbus glue, and build the missing kpi bindings**
   (bus_dma, MSI-X, firmware, mbuf, ifnet). *Recommend yes* (D2).
4. **Reference rule:** permissive-licensed driver code and public standards
   allowed, notices kept; GPL-only code and firmware disassembly never.
   *Recommend yes* (§4).
5. **First family MediaTek** (revised 2026-11-13 after Shankar's hardware
   answer, §5.1): MT7921AU over USB in the Mac VM, then MT7925 over PCIe in
   the Minisforum AI X1 Pro; Intel AX2xx/BE200 second. *Recommend yes.*
6. **Own Rust supplicant inside `aurad`**, WPA2/WPA3-Personal + PMF in v1,
   Enterprise in W7. *Recommend yes.*
7. **Machine-wide networks in Arca's machine store** (new kind
   `WifiCredential`) so the machine can join before login; per-user networks
   in the user store. *Recommend yes.*
8. **Security defaults:** no WEP/TKIP ever; PMF capable by default; per-network
   random MAC on by default; random scan MAC. *Recommend yes* (§3.4).
9. **Regulatory from wireless-regdb**, firmware LAR, user country; AP may
   only tighten. *Recommend yes* (§3.6).
10. **Simulation-first testing** (`aura_sim` + Linux hwsim/hostapd) before
    silicon. *Recommend yes* (§6). **Accepted 2026-11-13** (Shankar), with no
    emulated chip model: QEMU has no Wi-Fi device, so the driver is tested on
    the real MT7921AU via USB passthrough and the MT7925 bare metal.
11. **Station only in v1**; AP/hotspot later. *Recommend yes* (§9).
12. **Usable on stock FreeBSD as a port** (kmods + `aurad` with seams for
    Arca and launch), as Vinculum is. *Recommend yes*, it costs little: the
    kmods are ordinary kmods and `aurad`'s Atrium dependencies are two
    traits.
13. **Name Aura** (`aurad`, `aura_*` kmods). *Recommend yes*; alternative
    *Aether*.

### Open question for Shankar

What Wi-Fi hardware is available for testing: which machines (and their
Wi-Fi chips) can run Atrium bare metal, is a spare M.2/PCIe Intel AX210 or
BE200 card an option, and which access point (WPA3 and 6 GHz capable?) can we
reconfigure freely? The R9700 box is a hosted server with no radio, and the
Mac's Apple Wi-Fi is not a target.
