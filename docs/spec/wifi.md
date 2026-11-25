# Wi-Fi: Atrium's own drivers, MAC layer and supplicant

**Status:** accepted 2026-11-13 (Shankar: all §10 decisions as recommended, 1–17; decision 5 revised to MediaTek first after the hardware answer).
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

Two character devices per radio, `/dev/aura/ctlN` (for `aura-mlme`) and
`/dev/aura/keyN` (for `aura-keys`: `SetKey`, `DelKey`, `PortAuthorized`,
`TxEapol`, and EAPOL in; nothing else), each carrying length-prefixed
messages of a versioned binary protocol defined once in a `no_std` crate
(`aura-proto`, built: `aura/aura-proto`) used by both sides. Requests: `Scan`, `Join`, `Auth`,
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

`aurad` below is one logical service; §8.3 splits it into two jails,
`aura-mlme` (parsing, no secrets) and `aura-keys` (handshakes and secrets),
decision 14.

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
- A wrong passphrase is said as one (2026-11-15), not left to a timeout:
  WPA2, aura-keys tells aura-mlme `handshake what=m2` when it has answered
  the AP's first message, and the AP then giving up (deauthentication or
  disassociation with reason 15, or our handshake timeout) becomes `state
  failed why=wrong-passphrase` (the AP could not check our MIC); WPA3, the
  AP refusing our SAE confirm (hostapd answers status 1 at transaction 2),
  or leaving it unanswered after its retries, is the same, while a refused
  or unanswered commit stays `auth-refused` / `auth-timeout`. atrium-netconf
  hands the reason on, and Curia says "the passphrase is wrong".
- A newly typed passphrase is on trial (2026-11-15): atrium-netconf's join
  sends `wifi-remember ... trial=1`; aura-keys keeps it in memory for the
  next `prepare` of that network only and stores it when that join is
  authorized, and atrium-net saves the network's profile then too. A failed
  join drops it, so a mistyped passphrase never replaces one that worked and
  a new network that never joined is not saved. `atrium-netctl remember`
  still stores at once.

### 3.5 atrium-net

network.md §9 already gives Wi-Fi to `atrium-net`, the request-driven
daemon (not `atrium-netd`, which reconciles pf for jails). Change there:
`atrium-net` talks to `aurad` instead of writing `wpa_supplicant.conf`, and
runs DHCP on `wlanN` when `aurad` reports the port authorized. The
`network_wifi` capability and `JoinWifi`/`ListWifiNetworks` RPCs stay.

### 3.6 Regulatory

- Database: wireless-regdb (`db.txt`, ISC licence) compiled into `aurad`
  at build time (`aura/aura-regdb`, §7.3), so it is signed with the binary.
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
| W0 | `aura-proto`, `aura_wlan` skeleton, kpi bindings (bus_dma, MSI-X, firmware, mbuf, ifnet), `aura_sim` + hwsim bridge | `wlan0` appears in the VM and passes Ethernet frames to the sim. **Done 2026-11-13** (§7.1) |
| W1 | `aurad`: scan, open + WPA2-PSK + PMF, Arca credentials, jail, atrium-net glue, DHCP | VM joins hwsim `hostapd` WPA2 and gets an address. **Done 2026-11-13** (§7.2) |
| W2 | `aurad`: WPA3-SAE (H2E + hunting-and-pecking), transition mode, roaming, MAC randomization, regdb | sim gate (§6.2) green. **Done 2026-11-13** (§7.3) |
| W3 | `aura_mt79` USB transport: MCU messages, patch + RAM firmware load, hardware scan | MT7921AU in the Mac VM scans and reports BSSs to `aurad` |
| W4 | `aura_mt79` station: BSS/STA records, TX/RX, HW crypto, firmware rate control, BA sessions, PS, country/regulatory; then the PCIe (WFDMA) bus layer and MT7925 on the AI X1 Pro | real AP joined at HE/EHT rates, iperf target met on the AI X1 Pro |
| W5 | suspend/resume, rfkill, firmware crash recovery (reload + rejoin), 24 h soak | soak green |
| W6 | second family: `aura_intel` (BE200 swapped into the AI X1 Pro, or AX210) | same gates |
| W7 | Enterprise: EAP-TLS, PEAP-MSCHAPv2, EAP-TTLS; certificates through Arca | interop with hostapd's EAP server. Proposed in §7.4 (2026-11-24) |

W0–W2 need no Wi-Fi hardware at all.

### 7.1 W0 as built (2026-11-13)

- `aura/aura-proto`: the control-channel protocol, `no_std`, no `unsafe`, one
  encoding per message (fuzzed). `aura/aura-wlan-core`: the data path rules
  over a `Radio` trait (controlled port, A-MSDU with the FragAttacks check,
  fragments dropped, KRACK no-reinstall, per-TID replay counters,
  regulatory with world rules until a country is set). Host tests:
  `cd aura && cargo test`.
- `aura/kmod/wlan` (`aura_wlan.ko`): C for cdevs, ifnet (if_t API), mbufs,
  locking and sysctls (`net.aura.wlanN.*`, drop counters by reason); the two
  crates compiled in through kmod.rust.mk with a small Rust seam
  (`aura_wlan_rs.rs`). The driver ABI (`aura/kmod/aura_wlan.h`) passes
  control requests to drivers as validated aura-proto bytes, so there is one
  versioned format and no C struct to keep in step beyond the caps table.
- `aura/kmod/sim` (`aura_sim.ko`): test radio, a pipe to user space through
  `/dev/aura/sim0`; parses nothing.
- Kernel bindings: none were needed yet. The C glue does ifnet and mbufs, as
  §3.3 allowed for; `bus_dma`, MSI-X and `firmware(9)` bindings arrive with
  the first hardware driver (W3), where they can be tested.
- Gate: `scripts/aura-vm-check.sh kernel|test` (RUNBOOK §13.6c), 24 checks
  in the arm64 cloud VM on the fork's ATRIUM-NODEBUG kernel, modules unload
  cleanly. The hwsim/hostapd bridge moves to W1, where the first handshake
  needs it.

### 7.2 W1 as built (2026-11-13)

`aurad` is two daemons, as §8.3 split it:

- `aura/aura-mlme` (uid `_aura` 65033, jail `atrium-aura-mlme`, devfs ruleset 28: only
  `/dev/aura/ctl0`, no network, Capsicum after start-up): scan results and IE parsing
  (WEP, TKIP and WPA1 refused; a remembered SSID offering weaker security than remembered
  is refused as an evil twin), open authentication and association (WMM IE for QoS),
  `SetBss`, PMF (an unprotected deauth or disassoc starts an SA Query instead of leaving;
  the AP's SA Query is answered), timeouts and retries. It never sees a key.
- `aura/aura-keys` (uid `_aura-keys` 65032, jail `atrium-aura-keys`, ruleset 29: only
  `/dev/aura/key0`): credentials in Arca's machine store (`Kind::WifiCredential`, arca.md
  §6.4), PSK derivation, the 4-way and group handshakes (`aura/aura-rsn`), key install
  through `key0`, the controlled port. Its rules (MIC before key data, replay counter only
  forward, no reinstall, msg 3 RSNE equal to the beacon's byte for byte, IGTK required with
  PMF) have host tests including the Annex J vectors and mutation fuzzing.
- `aura/aura-ipc`: the line protocol between them and the control client; peer roles by
  uid (`getpeereid`), lines capped at 4 KiB, values hex. Sockets
  `/atrium/sockets/aura/mlme.sock` (0600) and `/atrium/sockets/aura-keys/keys.sock`.
- `aura/aura-ctl`: the atrium-net stand-in (`remember`, `forget`, `scan`, `join [--open]
  [--wait] [--dhcp IF]`, `leave`, `status`); DHCP is `dhclient` until atrium-net exists.
- `aura/aura-air`: test only, the firmware stand-in on the far side of `/dev/aura/sim0`:
  channel hopping (150 ms dwell, probes only on 2412–2452), CCMP-128 for data and
  unicast robust management frames, BIP-CMAC-128 checks and the management replay check.
  Data frame PNs go up to the kernel core, which checks them. The management replay check
  belongs in `aura-wlan-core` once real drivers deliver protected management frames
  (W3/W4); it lives in aura-air now only because the sim has no such path.
- Manifests: `aura/etc/services.d/43-atrium-aura-keys.toml`, `44-atrium-aura-mlme.toml`;
  rulesets in `etc/atrium.devfs.rules`, pins in `etc/jaild.policy.toml`.
- Gate: `scripts/aura-sim-gate.sh vms|w1` (was `aura-w1-check.sh`; RUNBOOK §13.6c). A second VM (Ubuntu arm64)
  runs unmodified hostapd 2.10 on mac80211_hwsim with WPA2-PSK + PSK-SHA256, PMF
  required, GTK rekey every 60 s; `aura/sim-ap/air-hwsim.py` is the hwsim medium, carrying
  frames over UDP to aura-air. The FreeBSD VM joins in about 3 s, gets an address from
  udhcpd, pings both ways, survives a rekey and a forged deauth, and leaves.


### 7.3 W2 as built (2026-11-13)

- SAE (`aura/aura-rsn/src/sae.rs`), group 19 only, on the `p256` crate: hunting-and-pecking
  (always 40 rounds, constant-time selection) and hash-to-element (SSWU, PT cached per
  SSID and password), anti-clogging tokens (in the H2E container when H2E is used),
  reflection and invalid scalar/element checks. Host tests run IEEE 802.11 Annex J.10
  vectors (the ones hostapd uses), both methods end to end, wrong password, and mutation
  fuzzing of hostile commits. SAE runs in aura-keys; aura-mlme only carries the
  authentication frames and never sees the password or PMK.
- AKM 8 (SAE): key descriptor version 0 (AES-CMAC MIC, SHA-256 KDF), RSNXE in message 2
  and checked in message 3 against the beacon, PMF required (no MFPC means no SAE).
- Transition mode: an AP offering PSK and SAE is joined with SAE. A Transition Disable
  KDE (it is a WFA vendor KDE, OUI 50-6f-9a type 0x20, not an 00-0f-ac one) puts the
  SSID on a WPA3-only list, so a later WPA2 BSS of that name is refused. The list lives
  in aura-mlme memory for now; atrium-net will persist it.
- MAC addresses: per network by default (HMAC of the SSID under a device key that
  aura-keys keeps in Arca as `aura mac-key`), or `random` per join, or `device`. The
  kernel takes the address with `SetMac` and changes the ifnet's lladdr.
- Roaming: beacon RSSI average; below -70 dBm a background scan runs at most every
  10 s, and a BSS of the same network 8 dB better on two scans in a row gets a fresh
  SAE join after a protected deauth; no further roam scan for 30 s after a move
  (hysteresis, added 2026-11-14). No 802.11r (FT) or PMK caching yet.
- Regulatory: `aura/aura-regdb` compiles wireless-regdb `db.txt` (2026.05.30, ISC,
  in `aura-regdb/data/`) into the binary at build time; unknown flags fail the build.
  The country is set with `aura-ctl country CC` (atrium-net later). An AP's Country
  element narrows the rules while associated (intersection only) and they are restored
  on leave.
- Gate: `scripts/aura-sim-gate.sh vms|w1|w2` (RUNBOOK §13.6c). The AP VM runs hostapd
  2.10 with up to two radios bridged to one DHCP network. W2 scenarios: WPA3-only H2E
  with anti-clogging, DHCP, ping and a country change narrowed by the AP; WPA3
  hunting-and-pecking; transition mode with Transition Disable; a WPA2-only AP under a
  WPA3-only name refused; roaming from channel 1 to 6 as `aura/sim-ap/air-hwsim.py`
  lowers the first AP's signal; no secret in the aura-mlme log.

### 7.4 W7 Enterprise (proposed 2026-11-24)

Work, campus and eduroam networks: WPA2/WPA3-Enterprise, where the network checks a
username and password (or a certificate) through a RADIUS server instead of one shared
passphrase. Today Atrium cannot join them at all.

**What joins.** AKM 1 (802.1X, SHA-1) and AKM 5 (802.1X, SHA-256; PMF as the AP asks,
required for WPA3-Enterprise). Not in v1: AKM 3/4 (FT), 12/13 (192-bit Suite B), PMK
caching across roams (each roam runs EAP again: slower, correct).

**Methods.** PEAPv0 with EAP-MSCHAPv2 inside; EAP-TTLS with PAP or MSCHAPv2 inside;
EAP-TLS with a client certificate. These cover eduroam and nearly every company
network. Never: LEAP, EAP-MD5, EAP-FAST, plain MSCHAPv2 outside a tunnel. The server
proposes a method; we answer with a Nak naming the one the network is set for, so a
server cannot talk the machine into a weaker one.

**The server certificate is always checked.** A captured password is the usual way
enterprise Wi-Fi is attacked (an evil twin with the same name and any certificate). So
there is no "don't validate" switch: a network is joined only when the RADIUS server's
chain ends in the trust it was set up with (the system roots, or a CA certificate
imported for that network) and the certificate names the server's domain (a dNSName in
its subjectAltName equal to, or under, the domain set for the network). On a first join
with no CA set, the join stops before any password is sent and Settings shows the
server's certificate (its name, issuer, validity and fingerprint) with "Trust this
server": that pins the chain's root and the server's name for this network only
(the "trust on first use" phones do for eduroam). A later change of root is refused
and shown, never accepted silently.

**Where it runs (§8.3's `aura-eap`).** A new process, `atrium-aura-eap`, one per join,
started by aura-keys (`execve`, so it inherits none of aura-keys' memory) with one
socket as its stdin and nothing else, no network, no files, capability mode before the
first EAP byte. It runs as aura-keys' uid inside aura-keys' jail: aura-keys is not root
and cannot give it another uid, and capability mode already takes away every global
name (no files, no sockets, no signals to other processes), which is more than a
separate uid and a chroot would. It parses EAP, the TLS records and the server's certificate
chain (the large hostile parsers) and holds the password only for that join. aura-keys
keeps EAPOL and the controlled port: it passes EAP packets (EAPOL type 0) both ways,
gives aura-eap the network's settings and credential for that join, and takes back the
MSK at the end; the PMK is its first 32 bytes, and the 4-way handshake runs as for a
passphrase. aura-mlme sees neither the password nor the MSK. A client certificate's key
stays in Arca: aura-eap asks Arca, through aura-keys, to sign the TLS CertificateVerify
(the key never leaves Arca).

**TLS.** rustls 0.23 (Apache-2.0/MIT/ISC, already in the tree), driven without
sockets, no SNI, no resumption. v1 speaks TLS 1.2, which every RADIUS server does and
whose EAP keying is settled: keys from the RFC 5705 exporter ("client EAP encryption"
for PEAP and EAP-TLS, "ttls keying material" and "ttls challenge" for TTLS). TLS 1.3
(RFC 9190, RFC 9427 for PEAP/TTLS) follows once the servers people meet offer it. A
server that speaks only TLS 1.0 or 1.1 is refused with that reason. PEAP runs
Microsoft's crypto-binding when the server asks for it (the server's MAC is checked
and the MSK then comes from the compound keys), and a Result TLV of success counts
only after the server proved the password with MSCHAPv2's authenticator response.
The server certificate's name check is "equal to the domain or under it" on its DNS
names, no wildcards. MSCHAPv2 needs MD4 and DES
(RustCrypto `md4`, `des`: MIT/Apache-2.0).

**Identity privacy.** The outer, unencrypted identity is `anonymous@<realm>` (the realm
from the username) unless the network says otherwise; the real username goes only
inside the tunnel.

**Settings.** A secured network nearby that advertises 802.1X opens an "Enterprise"
sheet: username, password (typed into Arca's field, as for a passphrase), method
(Automatic: PEAP, then TTLS), CA (System or a file), domain, and, under More,
anonymous identity and a client certificate. Saved networks show "Enterprise (802.1X)"
and the pinned server.

**Build steps.**

| Step | What | Done when |
|---|---|---|
| E1 | `aura-eap` crate: EAP state machine, PEAP/TTLS/EAP-TLS, MSCHAPv2 (RFC 2759 vectors), server checks; fuzz targets for EAP and the inner methods | host tests against hostapd's EAP server (`eapol_test`-style harness on the Linux host) for each method, wrong password, wrong CA, wrong domain, TLS 1.0-only server. **Done 2026-11-24**: `aura/aura-eap`, `cargo test` there runs hostapd 2.10 as a RADIUS server (`tests/hostapd.rs`): PEAP (with crypto-binding), TTLS-MSCHAPv2, TTLS-PAP and EAP-TLS each end with the MSK the server gives the AP; wrong passwords, another CA, another domain, an evil twin with the same name (also after pinning), first join with nothing set stopping at the certificate, a TLS 1.0-only server, EAP-TLS without a certificate all refused; fuzz target `aura/fuzz` `eap_peer` (1.3 M runs clean) |
| E2 | aura-keys: AKM 1/5, the `aura-eap` process and jail, MSK to PMK; atrium-net and Arca: enterprise profile and credential | `aura-sim-gate.sh w7`: the VM joins hostapd WPA2- and WPA3-Enterprise on hwsim with PEAP and TTLS, gets an address; an evil-twin AP with another certificate is refused before any password leaves. **Done 2026-11-24**: 25/25 on the cloud VMs. AKM 1 and 5 in aura-rsn and aura-mlme (`security=eap`/`eap-sha256`); aura-keys stores an 802.1X profile in Arca (`remember-eap`, the profile carries the password), runs `atrium-aura-eap` per join and the 4-way handshake on the MSK; `eap-seen`/`eap-trust` pin the server a first join stopped at (only when nothing is set; a changed server needs the network set up again). On a join EAP does not finish aura-keys sends EAPOL-Logoff first: under PMF the AP ignores our unprotected deauth and would hold the half-done conversation and its retry count (hostapd then refuses the next association with reason 23). After an EAP failure the AP ignores the station for its quiet period (hostapd: 60 s). Atrium-net and Settings learn 802.1X profiles in E4; aura-ctl drives the gate. System roots for `Trust::Roots` (no CA file) also come with E4 |
| E3 | EAP-TLS with the client key in Arca | same gate, certificate login. **Done 2026-11-24**: 29/29 on the cloud VMs. arcad lets the Wi-Fi key service (`aura-keys`) `MINT`, `PUBKEY` and `SIGN_DIGEST` a `TlsClientKey` (P-256) of its own, nothing else of crypta's. `eap-key ssid=.. cn=..` mints the key under the account `wifi <ssid hex> tls` and answers a PKCS #10 request signed by Arca (`aura_eap::csr`); `remember-eap method=tls key=.. cert=..` takes the certificate IT issued and refuses one for another key. During the join the helper asks for the one CertificateVerify signature over its pipe (`g` digest, `G` DER signature) and aura-keys has Arca sign it; the helper never holds the key. `forget` removes the key with the network. The certificate goes in one ctl line (up to about 2 KB of DER) |
| E4 | Settings sheet, first-join trust, bar menu | joined from Settings in the VM, shown in frames. **Done 2026-11-24** (cloud VMs; frames in project files `wifi-8021x/`). Settings opens a work-network sheet for a network that advertises 802.1X ("WPA2/WPA3 Enterprise"): username, the password in Arca's field, the method (PEAP, TTLS-MSCHAPv2, TTLS-PAP; chosen, not detected), how the server is checked (ask on first join, or the system's certificates with the server's domain) and the domain. A first join with nothing to check against shows the server's names, issuer, expiry and SHA-256 with "Trust this server"; the press pins it and joins. atrium-netconf ops `WIFI_JOIN_ENTERPRISE`, `WIFI_SERVER_SEEN`, `WIFI_TRUST_SERVER` (grant network-wifi); atrium-net keeps only that a profile is 802.1X (`eap=1`) and holds a network waiting for trust out of auto-join until it is trusted. aura-mlme reports an 802.1X failure before the keys as a failed join. The helper reads `/etc/ssl/cert.pem` (mounted read-only into aura-keys' jail) before capability mode for `roots=system`. The bar lists an unsaved work network as "sign in from Settings" and joins a saved one. Follow-ups, done the same day: the check "CA file" takes the network's CA (PEM or DER) through the system picker; an anonymous identity field for PEAP and TTLS (EAP-TLS sends the username, the name its certificate carries); "Certificate (EAP-TLS)": "Make a certificate request" has Arca mint the key and shows the PKCS #10 request as PEM to copy for IT (`WIFI_CERT_REQUEST`, op 0x28), "Import the signed certificate…" takes it back through the picker, and Join sends the key's id and the certificate. aura-ipc lines may be 16 KiB so a CA and a client certificate fit one line (each at most 3000 bytes of DER). Verified in the cloud VM: request, signing by the test CA (`curia-vm.sh sign`), import and CA through Scrinium's picker (`curia-vm.sh picker`), joined WPA3-Enterprise with EAP-TLS |

**Decisions** (Shankar accepted all six as recommended, 2026-11-24: Q1–Q5 yes, Q6 later)
- Q1. Methods in v1: PEAP-MSCHAPv2, TTLS (PAP, MSCHAPv2), EAP-TLS; nothing weaker. *Recommend yes.*
- Q2. The server certificate always checked (CA plus domain); no "don't validate". *Recommend yes.*
- Q3. First join without a CA: show the server's certificate and "Trust this server",
  pinning its root and name for that network. *Recommend yes.* (Alternative: require a
  CA file or the system roots up front; safer, but most eduroam setups would need one.)
- Q4. rustls, TLS 1.2 in v1 and 1.3 next; TLS 1.0/1.1 servers refused with a clear reason. *Recommend yes.*
- Q5. `aura-eap` as its own sandboxed process per join, password from aura-keys for
  that join only, client keys signing inside Arca. *Recommend yes.*
- Q6. Wired 802.1X (the same `aura-eap` on Ethernet) later, not in W7. *Recommend later.*

### 7.5 Wired 802.1X (built 2026-11-24, Q6 of §7.4)

Office and campus Ethernet ports that ask the machine to sign in before they carry
traffic (802.1X on the switch port, the same EAP methods as §7.4). Today such a port
gives no address, or a guest VLAN.

**What runs where.** The same pieces as Wi-Fi, so the methods, the server checks, "Trust
this server" and EAP-TLS with the key in Arca come unchanged. atrium-net (root) already
holds a bpf descriptor per Ethernet port: it adds EAPOL (ethertype 0x888E) to that
port's filter, joins the PAE group address 01:80:c2:00:00:03, and relays the frames to
aura-keys over a connection kept open for the port. It parses nothing in them: it checks
the ethertype and the length, and forwards. aura-keys runs the supplicant side (EAPOL
start, EAP through `atrium-aura-eap` per authentication, logoff after a failure) and
answers `authorized` or `failed why=..`; atrium-net sends the logoff itself when it
stops the port's session (cable out, profile forgotten). There is no 4-way handshake on a wire: the switch opens
the port when EAP succeeds, and the MSK is dropped (MACsec, 802.1AE, is not in v1).

**Lines on keys.sock** (atrium-net's ctl uid only): `wired-start port=NAME`,
`wired-rx body=HEX` (an EAPOL PDU, at most 1500 bytes), `wired-stop`; back on the same connection
`wired-tx body=HEX` (atrium-net sends it to the PAE group address), `authorized`,
`failed why=..` (`no-credential` when nothing is set up). The profile verbs take
`wired=1` instead of `ssid=..`: `remember-eap`, `eap-seen`, `eap-trust`, `eap-key`,
`forget`. The profile is kept in Arca under the account `wired` (the EAP-TLS key under
`wired tls`). aura-keys runs with `--dev none` on a machine without Wi-Fi.

**Settings and atrium-netconf.** Ops `WIRED_SIGN_IN` (an `Enterprise` with no SSID),
`WIRED_SERVER_SEEN`, `WIRED_TRUST_SERVER`, `WIRED_CERT_REQUEST`, `WIRED_FORGET`, under
network-configure (Settings only); each port's `Iface` carries `dot1x` (needed, running,
authorized, failed) and `dot1x_why`. atrium-net's control lines are `wired-remember-eap`,
`wired-eap-seen`, `wired-eap-trust`, `wired-eap-key`, `wired-forget`; it keeps only
that a wired profile exists (`dot1x` in its settings file). Settings shows a "Wired
network" row for a port whose switch speaks 802.1X ("This port asks you to sign in",
"Signing in…", "Signed in", or why it failed) with "Sign in…" opening the §7.4 sheet,
and shows the server's certificate when a first sign-in stops at it.

**Policy.**
- One wired profile for the machine, used on whichever Ethernet port asks (a laptop
  meets one office network through different docks and adapters). Per-switch profiles
  later, if anyone needs them.
- A port that never sends EAPOL works as today. DHCP starts at link up as before; when
  the port is authorized, atrium-net restarts that port's lease, because the switch
  often moves an authorized port to another VLAN. A switch that asks again
  (re-authentication) gets a fresh EAP run without the link dropping.
- A port asking for a sign-in with nothing set up is reported (`dot1x=needed` in the
  port's status, an event for Settings). After a failure, atrium-net waits 60 s (the
  switch's quiet period) before it starts again, at most three times until the profile
  changes or the cable is plugged in again.
- One port at a time runs 802.1X in v1.

**Testing without a network.** The FreeBSD VM gets one more NIC, a QEMU socket netdev
whose other end is a NIC of the Ubuntu AP VM: a cable between the two VMs only. There
hostapd runs with `driver=wired` and its own EAP server (the certificates and users of
the w7 gate), playing the switch, and udhcpd serves the cable. A link between two VMs
passes the PAE group address that a real bridge drops.

| Step | What | Done when |
|---|---|---|
| D1 | aura-keys wired sessions; atrium-net EAPOL relay, `wired-*` ctl lines, lease restart; the VM cable | `aura-sim-gate.sh w8`: PEAP, TTLS and EAP-TLS over the cable, then an address from the authorized port; a wrong password and another CA refused; first join stops at the certificate; a port without 802.1X unchanged |
| D2 | Settings: the Ethernet row says when the port asks to sign in and opens the §7.4 sheet for it; netconf ops for the wired profile | signed in from Settings in the VM, shown in frames |

**As built (2026-11-24).** D1 and D2 done. The policy is `atrium-net/src/dot1x.rs`
(host-tested, no system calls); the port's bpf filter passes EAPOL (`wrap.rs`), and
the port joins the PAE group with `SIOCADDMULTI`. aura-keys keeps one wired session
(`Wired` in `main.rs`), starting the helper on the switch's first EAP Request; the
account names are `creds::Net`. The rig: `aura-sim-gate.sh vms` boots both VMs with the
cable (FreeBSD vtnet2, QEMU udp 7811/7812), `aura-ap-vm.sh wired` starts hostapd
`driver=wired` with `aura/sim-ap/hostapd-wired.conf`, and `wired-port.sh` (fed
hostapd's port lines) runs udhcpd only while the port is authorized, so an address proves the
switch opened it. `aura-sim-gate.sh w8` 23/23: needed with no address; PEAP stopping at
the certificate, then trusted, an address and a ping; TTLS-PAP with the CA; EAP-TLS with
the key in Arca; cable out and in signs in again with no one asked; a wrong password
and another CA refused (the switch closes the port and the address goes); forget. `aura-sim-gate.sh
bar-wired` then `curia-vm.sh arca`: signed in from Settings with "Trust this server".
A port without 802.1X: `atrium-net-gate.sh n0` 72/72 unchanged.
When a port the switch had opened fails or is forgotten, atrium-net tells the lease
client `restart`: the old lease is dropped and DHCP starts from DISCOVER (not
INIT-REBOOT, which a closed port's old VLAN could still answer). The rig's switch
(`wired-port.sh`) follows hostapd's own "authorizing port" / "unauthorizing port"
lines, since a failed re-authentication shuts the port with no hostapd_cli event.
Open: periodic re-authentication by the switch is untested (`eap_reauth_period=0`
in the rig).

## 8. Threat model and what runs where

### 8.1 What we protect

- The host: kernel, other jails, the user's data.
- Network credentials (passwords, PMKs, later EAP certificates), kept in Arca.
- Traffic confidentiality and integrity on the link, and that we join the
  network the user meant.
- Location and identity privacy: scans and probes must not track the user,
  and apps must not learn where the machine is from nearby networks.
- Availability, where the protocol allows (deauth floods cannot be fully
  stopped on networks without PMF).

### 8.2 Attackers

| # | Attacker | Can do |
|---|---|---|
| A1 | Anyone in radio range, no credentials | send any frame: beacons, probe responses, auth/assoc, action frames, EAPOL msg 1, deauth, fragments, A-MSDUs; jam; watch our probes and MAC |
| A2 | Rogue AP / evil twin | advertise a remembered SSID, offer weaker security, drive our handshake; with the password (shared PSK/SAE networks) it can impersonate the real AP fully, which no client can prevent |
| A3 | The network after joining | DHCP, DNS, IPv6 RA, every LAN peer |
| A4 | Compromised radio firmware | firmware parses raw radio frames itself; an over-the-air firmware exploit (Broadpwn-style) gives a DMA-capable bus master on PCIe |
| A5 | Malicious USB device | claims to be a Wi-Fi dongle; hostile USB descriptors and responses |
| A6 | Local app (jailed) | wants scan results (location), to change networks, or to read passwords |
| A7 | Compromised `aura-mlme` or `aura-keys` (after a parser bug) | see §8.4 |

### 8.3 Where each piece runs, and why

| Piece | Hostile input it handles | Placement | Reason |
|---|---|---|---|
| Radio firmware | all raw radio input | on the card; treated as untrusted | we can't change it. PCIe: IOMMU (DMAR on Intel, AMD IOMMU on the AI X1 Pro: FreeBSD 15 has an AMD driver, to verify on that box); the driver bounds-checks every ring index and length the device gives. USB: no DMA, so the USB dongle is the safer path by design |
| `aura_mt79` / `aura_intel` | firmware events, RX descriptors, USB/PCIe responses (A4, A5) | kernel, Rust | DMA rings and interrupts need the kernel (no IOMMU-backed user DMA in FreeBSD). Only fixed-format structures are parsed; no IE ever. A USB user-space driver over `ugen` is possible (as Vinculum does) but costs throughput; not proposed, kept as a fallback if the kernel parser count grows |
| `aura_wlan` | data-frame headers, A-MSDU subframes, fragments (A1, A3) | kernel, Rust | per-packet path. KRACK, FragAttacks rules live here: no key reinstall with a reset replay counter, drop non-SPP A-MSDU with an LLC/SNAP-looking DA, no mixed-key fragment reassembly, fragment cache flushed on rekey, unencrypted data dropped once the port is authorized, EAPOL accepted only from the associated BSSID |
| `aura-mlme` (`aurad`'s parsing half) | beacons, probe responses, all IEs, auth/assoc/action frames, SAE commit/confirm framing, scan results (A1, A2) | jailed user space, Rust, fuzzed; uid `_aura`, no network, no files but state dir, capability mode after opening the cdev | the largest hostile parser; holds **no secrets** |
| `aura-keys` (new) | EAPOL-Key frames (MIC checked before key data is parsed), SAE crypto | separate jail, uid `_aura-keys`, no network; its only device node is `/dev/aura/keyN` (keys, controlled port, EAPOL; the kernel refuses every other message there and never shows EAPOL or keys on `ctlN`); SAE frames reach it from `aura-mlme` over a socket; credentials from Arca | holds the password/PMK. Its parser is small and fixed-format, so a parser bug in `aura-mlme` does not hand over the network password. Recommend this split (decision 14) |
| `aura-eap` (W7) | EAP, TLS server certificate chains (A2) | third jail, per join | TLS is a large parser; wpa_supplicant's EAP code has had memory bugs. Client certificate keys stay in Arca/Sigillum and sign through them |
| `atrium-net` | no radio input; requests from Forum/apps | root, narrow | applies results only (addresses, routes) |
| DHCP client | DHCP replies (A3) | jailed, Rust (or FreeBSD `dhclient`'s privsep child until then) | an A3 parser; network.md currently has `atrium-net` run `dhclient`. Recommend a jailed client (decision 15; belongs to the network work, noted here) |
| Forum Wi-Fi picker | SSID strings (A1, A2) | Forum | SSIDs are 32 arbitrary bytes: show non-UTF-8 as hex, strip bidi/control characters, flag lookalikes of remembered names |
| Apps | — | through `atrium-net` only | scan results reveal location: `ListWifiNetworks` needs a capability and a user grant, BSSIDs are never given to apps, only "connected/SSID" with `network_read` (decision 16) |

### 8.4 If a piece is compromised

- **`aura-mlme`:** can join, leave and scan, see management frames and
  EAPOL frames, and send management frames within the regulatory limits the
  kernel enforces. Cannot read or inject data traffic (the data path is in
  the kernel), cannot get passwords or PMKs (in `aura-keys`), cannot reach
  the network or the filesystem. Worst case: steer the machine to a network
  the user already saved, or cut it off.
- **`aura-keys`:** gets the credentials of networks it is asked to join
  (Arca releases one per join request from `atrium-net`, never the list).
- **Kernel driver:** kernel compromise; that is why it parses fixed formats
  only, is Rust, and its parsers are fuzzed against recorded firmware event
  streams.
- **Firmware:** without an IOMMU, host compromise over DMA; with one, it can
  corrupt only its own rings and frames. USB dongles avoid this entirely.

### 8.5 Known attack classes

| Attack | Mitigation |
|---|---|
| KRACK (key reinstallation) | `aura_wlan` refuses installing a key already in use; replay counters never reset on retransmitted msg 3 |
| FragAttacks | §8.3 `aura_wlan` rules; SPP A-MSDU negotiated where supported |
| Dragonblood (SAE side channels, downgrade) | H2E preferred, hunting-and-pecking constant time; group 19 minimum, no group downgrade; anti-clogging tokens; transition-disable honoured |
| Deauth/disassoc floods | PMF on by default; required on WPA3 |
| Evil twin / security downgrade | remember each network's security level; never join a remembered WPA3 network as WPA2 or open; no auto-join of open networks by SSID alone (Karma), only when the user opted in per network (decision 17) |
| Tracking | random MAC per network and per scan; no directed probes except networks marked hidden; randomized sequence numbers in probes |
| Beacon/scan flooding | bounded scan table in `aura-mlme`, per-BSS rate limits; kernel queue limits on the management frame channel |
| Firmware OTA exploit | IOMMU; driver never trusts firmware lengths or indices; firmware crash → full reset |
| Malicious USB device | the driver matches by VID:PID only through dispensatord; descriptor checks in Rust before any transfer |

### 8.6 Residual risks

- Anyone with a shared password can impersonate the AP (WPA2-PSK and
  WPA3-SAE both); only Enterprise with server certificate checks fixes that.
- The kernel driver is still kernel code; Rust removes memory bugs, not
  logic bugs.
- PCIe cards on machines without a working IOMMU trust the firmware. Proposed:
  Atrium warns in settings when Wi-Fi runs without IOMMU protection.

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

14. **Split the daemon in two jails:** `aura-mlme` (all frame and IE
    parsing, no secrets) and `aura-keys` (handshakes, passwords/PMKs, tiny
    parser); later `aura-eap` in a third. *Recommend yes* (§8.3).
15. **Jailed DHCP client** instead of root `dhclient` under `atrium-net`.
    *Recommend yes*; it is network-thread work, noted here (§8.3).
16. **Scan results are location data:** apps need a capability and a user
    grant for `ListWifiNetworks`, never see BSSIDs. *Recommend yes* (§8.3).
17. **No auto-join of open networks** unless opted in per network; never
    join a remembered network at a weaker security level. *Recommend yes*
    (§8.5).

### Open question for Shankar

What Wi-Fi hardware is available for testing: which machines (and their
Wi-Fi chips) can run Atrium bare metal, is a spare M.2/PCIe Intel AX210 or
BE200 card an option, and which access point (WPA3 and 6 GHz capable?) can we
reconfigure freely? The R9700 box is a hosted server with no radio, and the
Mac's Apple Wi-Fi is not a target.
