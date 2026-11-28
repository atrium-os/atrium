# Vinculum: the Bluetooth service

**Status:** accepted 2026-11-07 (Shankar: all nine §16 decisions as recommended; decision 3 settled as Rust throughout). Being built. **Owner:** the Bluetooth thread.
Interfaces agreed with the input thread (Tactus, [atrium-input.md](atrium-input.md) rev. 2 §3.3)
and to be agreed with the audio thread ([lyra-v1.md](lyra-v1.md)).

Atrium needs Bluetooth for keyboards, mice and game controllers, for headphones and speakers
(A2DP, hands-free, LE Audio), and later for sensors, file transfer and app accessories. This
spec decides whether to build on FreeBSD's netgraph Bluetooth stack or write a new one, and
then describes the new one.

**The short answer.** Write a new host stack in Rust, in user space, split across jails, and
load none of FreeBSD's netgraph Bluetooth at run time. FreeBSD's stack covers Bluetooth as it
was around 2005 (Classic, PIN pairing). Almost everything a 2026 device needs (Secure Simple
Pairing, LE Secure Connections, GATT, privacy, audio profiles, isochronous channels) would be
new code either way, and the netgraph design puts the parsing of radio traffic in the kernel,
which is the thing Atrium's security model exists to avoid. The working name is **Vinculum**
("bond", "link": what pairing makes).

## 1. What FreeBSD has today, checked in the source

Read in `freebsd-src` main on 2026-11-07 (`sys/netgraph/bluetooth/`, `usr.sbin/bluetooth/`).

| layer | FreeBSD | state |
|---|---|---|
| transport | `ng_ubt` (USB), `ng_h4` (UART), `ng_bt3c` (PC card) | works; `ng_ubt` does isochronous SCO |
| firmware | `iwmbtfw` (Intel), `rtlbtfw` (Realtek), `bcmfw` (Broadcom), `ath3kfw` (Atheros) | userland loaders, BSD licensed; no MediaTek loader |
| HCI | `ng_hci`, in the kernel | Classic events parsed in the kernel; LE: advertising reports, connection complete and connection update only |
| L2CAP | `ng_l2cap`, in the kernel | Classic basic mode signalling; LE fixed channels ATT (4) and SMP (6) exposed as sockets. No ERTM/streaming mode, no LE credit-based channels |
| sockets | `ng_btsocket`: raw HCI, L2CAP, RFCOMM (in the kernel), SCO | works |
| pairing | `hcsecd` | answers only `PIN_Code_Request` and `Link_Key_Request`, keys in a flat file. `ng_hci_evnt.c` drops `IO_Capability_Request` and `Simple_Pairing_Complete` ("do not need post processing"), so **no Secure Simple Pairing** (Bluetooth 2.1, 2007), which nearly every device since requires |
| LE security | none | no SMP, no LE Secure Connections, no privacy (resolvable addresses) |
| GATT | none | no ATT/GATT client or server, so no HID over GATT, no battery service, no LE anything |
| HID | `bthidd` | Classic HID only; writes keystrokes into `vkbd(4)` and Linux-style `uinput`/evdev, the shape [atrium-input.md](atrium-input.md) rejects |
| audio | none | no AVDTP, A2DP, AVRCP, HFP, no LE Audio. (`virtual_oss` in ports has an SBC path over the sockets; it is not a stack) |
| other | `sdpd`, `btpand` (BNEP/PAN), `rfcomm_pppd`, `hccontrol`, `l2control`, `l2ping` | Classic only |

## 2. Improve the old stack or start again

### 2.1 What "improve" would mean

To reach what a user expects (pair AirPods or a current Logitech mouse, type on a BLE
keyboard at the login screen, play music), the netgraph route would need: SSP and BR/EDR
Secure Connections in `hcsecd` and `ng_hci`; SMP, LE privacy and LE Secure Connections; LE
credit-based L2CAP in `ng_l2cap`; an ATT/GATT stack; ISO channels in `ng_hci` for LE Audio;
AVDTP, A2DP, AVRCP, HFP; HID over GATT; a new HID path that is not `vkbd`/evdev. That is
almost the whole of a modern stack. The netgraph parts it would keep (Classic HCI, basic-mode
L2CAP, RFCOMM) are the smallest and oldest layers.

### 2.2 Why not

1. **The radio would be parsed in the kernel.** Every byte a nearby device sends (advertising
   data, L2CAP signalling, SDP, RFCOMM frames) arrives unauthenticated from anyone in radio
   range, before pairing. `ng_hci` and `ng_l2cap` parse it in kernel C. This is the layer
   where the big Bluetooth bugs live: BlueBorne (2017; Linux kernel L2CAP stack overflow
   CVE-2017-1000251, Android BNEP RCE CVE-2017-0781), BleedingTooth (2020; Linux kernel L2CAP
   type confusion CVE-2020-12351, LE extended advertising heap overflow CVE-2020-24490). A bug
   there is remote, zero-click kernel code execution. Atrium's rule since the Navigator threat
   model is that untrusted input is parsed in a jail with nothing to steal; the input thread
   just moved USB HID descriptor parsing out of frescod for the same reason. Growing the
   kernel's Bluetooth parsers to cover LE, GATT and ISO would grow exactly the wrong surface.
2. **The kernel layers buy little.** In a modern stack the kernel's natural job is moving
   packets. Credit accounting, LE connection management, pairing and every profile are user
   space in every stack built in the last decade (Android, Fuchsia, Apple, ChromeOS Floss,
   Zephyr's host, BTstack). Even BlueZ, which keeps HCI/L2CAP/SCO/ISO in Linux, does pairing,
   GATT and all profiles in `bluetoothd`.
3. **The design fights Atrium's.** The socket API gives any process with the device a raw HCI
   socket; profiles are daemons that each speak to the kernel. Atrium wants one owner per
   controller, and profile channels handed to jailed owners (Tactus, Lyra) with per-device
   permissions. Netgraph would sit underneath as a second, privileged authority.
4. **Upstream is quiet.** The netgraph Bluetooth code has had little feature work since LE
   scanning was added; improving it means carrying a large fork of kernel code alone.

### 2.3 What we keep from FreeBSD

- **Nothing at run time** except USB (`ugen(4)`/`libusb`) and UART (`uart(4)`) as the raw pipe
  to the controller (§5). `ng_ubt`, `ng_hci`, `ng_l2cap`, `ng_btsocket` are not loaded.
- **Knowledge:** the firmware loaders (`iwmbtfw`, `rtlbtfw`, `bcmfw`, `ath3kfw`, BSD-2) are
  ported to Rust into the transport (§5.2); `hccontrol` is a reference for command coverage.
- **A fallback transport:** if isochronous USB through `libusb` proves unreliable for SCO, the
  kernel's `ng_ubt` can be wired straight to an `ng_socket` node (no `ng_hci`, no
  `ng_l2cap`), which gives user space framed HCI packets with no kernel parsing above USB.

## 3. Lessons from other stacks

| stack | what we take | what we avoid |
|---|---|---|
| **BlueZ** (Linux) | the profile split; `ClassicBondedOnly`-style input policy (added after CVE-2023-45866, unauthenticated HID keystroke injection) | kernel L2CAP/HCI parsing; D-Bus as the profile API; GPL code (we read docs only, copy nothing) |
| **Android Fluoride → Gabeldorsche, Floss** | packet parsers generated from a declarative grammar (**PDL**, `pdl-compiler`, Apache-2.0, has a Rust backend); new code in Rust; the interop quirks database; Rootcanal and Bumble for tests (§14) | one big process holding every profile (Fluoride's BNEP was BlueBorne's Android RCE) |
| **Fuchsia** | the closest model: `bt-host` per controller owns HCI/L2CAP/GATT/pairing; profiles (`bt-a2dp`, `bt-avrcp`, `bt-hfp`, `bt-hid`) are separate sandboxed components that receive channels as handles | — |
| **Apple** (`bluetoothd`, audio daemons) | per-profile processes; system pairing UI is the only pairing path; strict accessory privacy | — |
| **Zephyr, NimBLE, BTstack** | compact, well-tested LE host and LE Audio state machines (Zephyr Apache-2.0) as reading references | — |
| **liblc3** (Google, Apache-2.0) | the LC3 codec for LE Audio and HFP super-wideband | — |

Attacks that shaped the policy in §7: KNOB (CVE-2019-9506, encryption key size forced to 1
byte), BIAS (CVE-2020-10135, impersonation during authentication), BLURtooth (CVE-2020-15802,
cross-transport key overwrite), BLUFFS (CVE-2023-24023, session key reuse), the 2023 HID
injection family (CVE-2023-45866 and siblings on Android, macOS and iOS), SweynTooth and
BrakTooth (controller and LE stack crashes from malformed packets).

## 4. Shape: processes, users, jails

```
 controller ── USB (ugen) / UART
      │
 ┌────▼─────────────────┐   HCI packets    ┌───────────────────────────────────┐
 │ vinculum-hci          │◄───────────────►│ vinculumd                          │
 │ one per controller    │  Unix stream     │ HCI, GAP, L2CAP, SMP/SSP, SDP,     │
 │ _vinculum-hci         │                  │ ATT/GATT, RFCOMM, ISO routing,     │
 │ firmware load, framing│                  │ device DB, per-device permissions  │
 │ command allowlist     │                  │ _vinculum, no network, Capsicum    │
 └──────────────────────┘                  └──┬─────────┬──────────┬─────────┬───┘
                                   HID fds    │  audio   │  control │  apps   │ keys
                                              ▼          ▼          ▼         ▼
                                         tactusd   vinculum-audio  Curia /   arcad
                                         (_tactus) (_vinculum-audio) Forum   (_arca)
                                                    │      │  bar
                                                    │      │audio-device (ring, page),
                                                    │      ▼media-control
                                                    │   choragusd ── lyrad
```

| process | user (proposed uid) | jail | holds | parses remote data |
|---|---|---|---|---|
| `vinculum-hci` | `_vinculum-hci` (65025) | own; devfs ruleset 25 `atrium_bluetooth` shows only its controller's `ugen`/`usb` nodes or tty, plus `usbctl` (libusb enumerates through it: USB_READ_DIR lists every USB device's ugen name on the machine, and its quirk get/name ioctls are read-only; quirk add/remove and templates need PRIV_DRIVER, which no jail has; accepted: the jail learns which USB devices exist, not their data) | the device; firmware files read-only | only packet framing (lengths) |
| `vinculumd` | `_vinculum` (65024) | own; no devices, no network | one socket per controller to `vinculum-hci`; service sockets; Arca link | yes, everything from the air up to the profile boundary |
| `vinculum-audio` | `_vinculum-audio` (65026) | own; no devices, no network | channels from vinculumd; a ring to lyrad; choragusd control | AVDTP, AVRCP, HFP AT commands, codec frames |
| `tactusd` | `_tactus` (65018) | input thread's jail | HID fds from vinculumd | HID descriptors and reports (as for USB) |

uids are reserved (as comments) in `jaild.policy.toml` and become grants when B1 lands;
65018 is `_tactus`, 65023 `_nuntius`. The input thread accepted §10 as written (2026-11-07).

**Why three of ours and not one.**
- `vinculum-hci` is separate so that a compromised `vinculumd` cannot talk to the USB device
  directly, and cannot send vendor commands that write controller RAM or firmware patches
  (persistence in the radio). It filters commands (§5.3). It is small.
- `vinculumd` is the big parser. It is in Rust with no `unsafe` outside a reviewed allowlist,
  its parsers are generated (§6.1), and it holds link keys for paired devices but nothing else
  worth stealing: no user files, no network, no display.
- `vinculum-audio` is separate because audio profiles bring the largest amount of
  per-device protocol (codec capability negotiation, AVRCP metadata, the HFP AT command text
  parser) and because its other end is lyrad, which runs at real-time priority. A compromise
  of it reaches the audio ring and the media-control socket, not link keys or HID.

**Who starts what.** `vinculumd` is a services.d manifest started at boot, before the login
screen, so a Bluetooth keyboard works at Vestibulum. When `atrium-devevents` reports a
USB controller (interface 0, class `0xE0/0x01/0x01`), dispensatord requests a ruleset-25
driver jail for that bus/address/interface. It binds `<jail>.sock` in
`/atrium/sockets/vinculum-controllers` (root:_vinculum 0750) and passes the listener
through jaild's service activation. The kernel-bound socket name must match the driver's
assigned path. The driver uid 65025 can accept on its fd and has no directory search
permission. Vinculumd connects to a published socket and checks the publisher uid (root).
The driver checks the connecting host uid (65024). The host serves one controller at a time.
`vinculum-audio` is a services.d manifest started after choragusd. Restart: if
`vinculum-hci` exits, `vinculumd` marks the controller down, every connection on it is gone,
and owners see their channels close; the supervisor restarts it and it reloads firmware. If
`vinculumd` exits, the transports reset their controllers (so no link survives without a host
that knows its keys) and every owner's channel closes.

### 4.1 Vanilla FreeBSD: the same stack, without Atrium

Vinculum is built so a stock FreeBSD machine can use it too, like Tessera and Laminar
(Shankar, 2026-11-07). FreeBSD's own Bluetooth has the gaps of §1 on every FreeBSD laptop,
and because the stack needs no kernel code and no base changes, it can ship as a port
(`comms/vinculum`). This follows the portability doctrine ([portability.md](portability.md)):
a portable core behind a thin seam, with Atrium as one host of it.

**The rule:** the protocol crates and the three programs depend on no Atrium crate. Everything
Atrium-specific sits behind six small traits in `vinculumd` and `vinculum-audio`, each with
an Atrium implementation and a plain FreeBSD one, picked at build time (cargo feature `atrium`)
and at start (config file). CI builds and tests the core with the `atrium` feature off.

| seam | on Atrium | on stock FreeBSD |
|---|---|---|
| **key store** | Arca machine store (§9) | `/var/db/vinculum/keys`, mode 0600, owned by the `vinculum` user, sealed with a machine key in a separate root-only file |
| **process start and confinement** | portcullisd jails, services.d, `_vinculum*` users | an `rc.d` script; the same three processes as their own users, each in Capsicum after start; an optional jail per process for those who want it |
| **controller discovery** | `atrium-devevents` → portcullisd starts `vinculum-hci` | a `devd` rule starts `vinculum-hci` for the `ugen` node; `devmatch_blocklist="ng_ubt"` in `rc.conf` stops the kernel driver taking the controller |
| **HID sink** | tactusd over the §10 socket | a `vinculum-hid` adapter that turns the §10 channel into what stock desktops read today: an `evdev` `uinput` device (what libinput, Xorg and Wayland compositors consume) and `vkbd` for the console, as `bthidd` does |
| **audio sink** | lyrad external device (§11) | a `cuse`/`virtual_oss` output, so any OSS program plays to the headphones as a `/dev/dsp` |
| **control and pairing UI** | `bluetooth-control` socket, Curia and the Forum bar | the same socket and wire, with `vinculum-ctl` (CLI and a pairing agent); a desktop can write its own GUI against the wire |

**Living next to FreeBSD's own stack.** A controller has one host: two stacks issuing HCI
commands to one controller would fight over its state. So on stock FreeBSD each controller
is owned either by the netgraph stack or by Vinculum, never both. Vinculum takes a
controller when `ng_ubt` is not attached to it (blocklisted in `rc.conf`, or detached with
`devctl detach` for one controller) and the `devd` rule starts `vinculum-hci` on its `ugen`
node; the same rule must not also run FreeBSD's `iwmbtfw`/`rtlbtfw`, since `vinculum-hci`
loads the firmware itself. A machine with two controllers can run one of each. Programs
written for FreeBSD's sockets (`hccontrol`, `bthidd`, `sdpd`, `obexapp`, `virtual_oss`'s
Bluetooth path) keep working on netgraph-owned controllers and do not see Vinculum's; there
is no `AF_BLUETOOTH` emulation, because it would bring back the kernel-shaped API this design
leaves. In the `ng_ubt` fallback of §2.3, the kernel driver stays attached but its hook is
connected to an `ng_socket` that `vinculum-hci` reads, instead of to `ng_hci`, so that
controller too belongs to Vinculum alone.

Atrium-only, by design: the app `bluetooth` capability and chooser (§12) need Portcullis's
launch registry; on stock FreeBSD apps get nothing beyond the control socket, which root
can grant by group. The security defaults of §7, the transport split (§5.3) and the parsers
are the same on both, so stock FreeBSD gets the security work too.

Where the code lives: in this repo under `vinculum/`, as its own workspace with no path
dependency outside it, so it can move to a separate repository (or be offered to FreeBSD
as a port) without changes. Rust in FreeBSD base is not settled upstream, so a port is the
realistic route; base would also need its netgraph Bluetooth kept for compatibility, which
is upstream's call, not ours.

## 5. Transport and firmware: `vinculum-hci`

### 5.1 USB and UART

- **USB** through `libusb` on the controller's `ugen` node: control endpoint for commands,
  interrupt IN for events, bulk IN/OUT for ACL and ISO, isochronous for SCO (alternate
  setting chosen from the active SCO links' air mode and count, as `ng_ubt` does).
- **UART** (H4, later H5/3-wire) for laptop and ARM boards whose controller sits on a serial
  line.
- **Test transport:** H4 over a TCP or Unix socket, so Rootcanal (§14) can be the controller.
- The transport speaks to `vinculumd` in a trivial frame: type byte (command, ACL, SCO, event,
  ISO) then the HCI packet. It checks each packet's length field against its frame and drops
  the packet otherwise. It does not look inside.

### 5.2 Firmware

Modern controllers need a firmware patch or full image at every power-up: Intel (AX2xx/BE2xx
`ibt-*.sfi`/`.ddc`), Realtek (`rtl_bt/*`), MediaTek (`BT_RAM_CODE_*`), Broadcom (`.hcd`).
`vinculum-hci` runs the vendor's init sequence and loads the file from
`/usr/local/share/atrium/firmware/bluetooth/` (read-only mount), porting the logic of FreeBSD's loaders (BSD-2)
(BSD-2) and adding MediaTek. Realtek is ported (B1b, `vinculum-hci/src/rtl.rs`); Realtek's files are redistributable unmodified with their licence (`vinculum/firmware/README.md`). Each vendor's firmware licence is checked before we
ship it (as was done for amdgpu, project memory `amdgpu-firmware-license`); files we may not
redistribute are fetched at install by the user's action. After init the vendor commands are
closed (§5.3).

### 5.3 Command allowlist

After firmware load the transport passes only commands on an allowlist (by OGF/OCF) taken
from what `vinculumd` actually issues: link control, link policy, controller and baseband,
informational, status, LE. Vendor-specific commands (OGF `0x3F`) are refused except a short,
named list per vendor (for example reading the firmware version, setting the SCO data path)
that cannot write controller memory. A refused command is answered to `vinculumd` with
`Command Status: Unknown HCI Command` and logged. The point: the host stack is the large
attack surface; even a full compromise of it must not be able to reflash the radio.

### 5.4 Controllers we do not have: the report

We cannot buy every chip, so support for a new one starts from its owner.
`vinculum-hci --report [--controller usb:VVVV:PPPP]` (built 2026-11-25,
`vinculum-hci/src/report.rs`) runs on Atrium, stock FreeBSD, Linux and macOS (root where the
OS needs it for USB) and prints, per USB controller of class E0/01/01: its USB identity,
strings, configuration and HCI endpoints; HCI Reset, then Read Local Version (manufacturer,
LMP subversion: the chip and its ROM), Supported Commands, LMP and LE features, ACL/SCO/LE/ISO
buffer sizes; the vendor's own version read where we know it carries nothing personal
(Realtek ROM version today); and what Vinculum does about that maker's firmware. It loads no
firmware, never reads the device address (no Read BD_ADDR, no vendor read known to carry
one, such as Intel's version TLV), reads no USB serial number and does not scan, so the text
is safe to post. The loop: the owner sends the report; we add the firmware loader or quirk
(firmware licence checked first, §5.2); they run a test build's report again and pair one of
their own devices. Order of loaders by how many machines they reach: Intel (most Intel-platform
laptops), MediaTek (many AMD laptops), Qualcomm, Broadcom/Cypress (older Macs, Raspberry Pi,
often on UART, which also needs H5).

## 6. The host: `vinculumd`

### 6.1 How it is built

- **Rust, one crate per layer** under `vinculum/`: `vinculum-hci-proto` (packets),
  `vinculum-l2cap`, `vinculum-smp`, `vinculum-gatt`, `vinculum-sdp`, `vinculum-rfcomm`, and
  `vinculumd` binding them with the device database and the service sockets. Each layer is a
  sans-I/O state machine (bytes and timers in, bytes and events out), so every one is
  host-testable and fuzzable without a controller.
- **Generated parsers.** Packet layouts for HCI, L2CAP signalling, SMP, ATT, SDP, AVDTP and
  AVRCP are written in PDL and compiled to Rust by `pdl-compiler` (Apache-2.0). AOSP's PDL
  files (Apache-2.0) are the starting point. No hand-written byte slicing for radio data.
- **Bounds everywhere.** Per controller: connections (default 16), per connection: L2CAP
  channels (32), ATT MTU (517), queued packets and bytes, pending GATT operations,
  reassembly buffer (64 KiB). A peer that exceeds a bound or sends a malformed PDU is
  disconnected; repeated offences from one identity are rate-limited for a while.
- **One event loop per controller**, single-threaded, `kqueue`; Capsicum after start (its
  descriptors are the transport sockets, its listening sockets, its state directory and the
  Arca link).
- **Fuzzing:** a target per parser and per state machine (fed sequences, not single packets),
  each with a positive control, run in CI like `arca`'s and `fresco-split`'s.

### 6.2 What it implements (target: Core 6.0)

| area | v1 | later |
|---|---|---|
| HCI | command flow control, ACL and ISO credit accounting per link, event dispatch, controller feature discovery | — |
| GAP, LE | extended advertising and scanning, connections, 1M/2M PHY, data length extension, privacy (resolvable private addresses, resolving list, 15 min rotation), connection parameter policy | Coded PHY, periodic advertising for Auracast, channel sounding |
| GAP, Classic | inquiry, paging, role switch, sniff policy; not discoverable by default | — |
| L2CAP | Classic basic mode; LE credit-based and enhanced credit-based channels; fixed channels (ATT, SMP, signalling) | Classic ERTM/streaming (OBEX, AVRCP browsing) |
| security | SSP (numeric comparison, passkey, Just Works) with BR/EDR Secure Connections; LE Secure Connections; LE legacy pairing only by per-device override (§7); CTKD with the no-downgrade rule | OOB (NFC) |
| SDP | client (find HID, A2DP, HFP records); minimal server for our own records | — |
| ATT/GATT | client and server, one ATT bearer per link shared by every owner, EATT | — |
| RFCOMM | multiplexer; DLCs handed to owners | — |
| ISO | CIS/BIS set-up; ISO data routed to `vinculum-audio` | — |

GATT and RFCOMM are in `vinculumd`, not in the owners, because each multiplexes many users
over one link: one ATT bearer serves the HID service, the battery service and an app's
accessory service at once.

### 6.3 Device database

One record per bonded device, keyed by a random 64-bit **device handle** that never changes
for the bond and is what every owner sees. The record holds: identity address and type,
name, appearance, vendor/product/version (Device ID profile or DIS), services found, which
profiles are allowed (§8), the user who paired it, and for each key its strength (SC or
legacy, MITM or not, key size). Keys themselves are in Arca (§9). Owners never learn the
Bluetooth address: the address is a tracking identifier, and no owner needs it.

### 6.4 Language: Rust throughout (decided 2026-11-07)

Asked 2026-11-07: Tessera and Laminar are C in FreeBSD's conventions and stock FreeBSD uses
them directly; should Vinculum be C too, with Rust only on the Atrium side?

**Why Tessera and Laminar are C.** They run in the kernel (Laminar is the scheduler;
Tessera's core is a freestanding library shared by the kernel module and the loader, with
Rust bindings, `tessera-sys`, for the userspace tools). In the kernel and the loader C is
the only choice. Vinculum is all user space, so that reason does not carry over.

**Why the host should still be Rust.**
- `vinculumd` is where every byte from the air is parsed, before pairing, from anyone in
  range. That is the layer of BlueBorne and BleedingTooth, and every one of those was a
  memory-safety bug in C. Jails and Capsicum limit what a compromise reaches, but here they
  limit less than usual: **`vinculumd` is a keyboard.** A compromised host can type into the
  user's session through the HID hand-off, and it holds every link key. Memory safety in
  this one process matters more than anywhere else in the stack.
- The parser generator we want (`pdl-compiler`) emits Rust (and C++), not C, and AOSP's
  grammar files come with it.
- Stock FreeBSD can use a Rust program fine as a port: the ports tree builds Rust daemons
  routinely. Only **base** would need C, and base inclusion is not a goal (§4.1).

**What C would cost Atrium.** The Atrium seams (Arca client, the Tactus and Lyra sockets,
the control wire) are postcard and Rust today. A C host would need either a second wire
format C can parse simply, or Rust adapters linked into a C program through an FFI layer,
as `tessera-sys` does. That is workable but it is two build systems and an FFI boundary in
the middle of the most security-sensitive process, for a benefit (base inclusion) we are
not seeking. With a Rust host, the Atrium side is ordinary Rust crates.

**And the transport?** A C `vinculum-hci` reusing FreeBSD's firmware loaders was considered.
Shankar decided against it (2026-11-07): the transport only loads firmware and frames
packets, so on its own it is of little use to FreeBSD, and one language keeps one build and
one set of tools. **Everything is Rust**: `vinculum-hci`, `vinculumd`, `vinculum-audio`.
The loaders' logic is ported. Stock FreeBSD uses the whole stack as a port (§4.1).

## 7. Security policy

These are defaults; each "override" is a per-device setting the user makes in the trusted
pairing UI, with the risk spelled out.

| rule | stops |
|---|---|
| **No profile channel on an unbonded link.** HID, audio and app channels are handed out only for a bonded device whose link is encrypted with that bond's key. Incoming HID connections from anything else are refused at L2CAP | CVE-2023-45866 family (keystroke injection by an unpaired "keyboard") |
| **HID keyboards need an authenticated (MITM-protected) key.** A device whose HID descriptor has a keyboard collection and paired with Just Works gets mouse/consumer reports only until re-paired with passkey or numeric comparison | a nearby attacker pairing a fake keyboard during a Just Works window |
| **16-byte encryption keys.** Encryption key size is read after every encryption change; under 16 bytes, disconnect | KNOB |
| **Secure Connections by default.** BR/EDR SC and LE SC; legacy pairing only by override, and never for keyboards. One exception (Shankar, 2026-11-26): on a controller without BR/EDR Secure Connections (LMP features page 2 bit 8 clear, e.g. the RTL8761BU), Classic SSP can only make P-192 keys, so an *authenticated* P-192 key (numeric comparison or passkey, 16-byte key, E0) is kept, keyboards included, and shown as a weaker key; unauthenticated P-192 is still refused, and on a controller that has SC a P-192 key is a downgrade and refused | weak legacy keys, BLUFFS |
| **Mutual authentication.** Re-authenticate on reconnect with role switch disabled during authentication; reject a peer that skips authentication as responder | BIAS |
| **No key downgrade.** A new key (pairing again or CTKD) replaces a stored one only if it is at least as strong (SC ≥ legacy, authenticated ≥ unauthenticated, same size); otherwise the pairing fails and the user is told | BLURtooth |
| **Pairing only while the user asks.** The controller is pairable only while the pairing UI is open (and for 60 s after "pair" on a device); otherwise every pairing request is refused. Every pairing, even Just Works, ends with a confirmation in the trusted UI | drive-by pairing, silent bonding |
| **Not discoverable, not connectable to strangers.** Classic page scan only when a bonded device may reconnect; LE advertising only for our own services and with a resolvable address | tracking, pre-pairing attack surface |
| **Identity is the key, not the name or address.** A device reconnecting with the right address but no valid key is a stranger | spoofing a paired device by address |
| **Bounded and disconnected on error** (§6.1) | malformed-packet crashes (SweynTooth class) |
| **The controller is half-trusted** (§5.3) | radio firmware persistence after a host compromise |
| **HCI logs are dev-only** and carry no keys (link key and LTK fields zeroed in the btsnoop writer) | key leak through logs |

## 8. Profiles and their owners

| profile | role on Atrium | owner | v1? |
|---|---|---|---|
| HID (Classic HIDP) | host | **tactusd** | yes |
| HID over GATT (HOGP) | host | **tactusd** | yes |
| Battery service (BAS), Device Information (DIS) | client | vinculumd itself; levels shown in the Forum bar and settings | yes |
| A2DP | source, SBC | **vinculum-audio** → lyrad | yes |
| AVRCP | target (headset buttons) and controller (absolute volume) | **vinculum-audio** → choragusd | yes |
| HFP / HSP | audio gateway (a headset's microphone) | **vinculum-audio** | after Lyra capture (L5) |
| LE Audio: BAP unicast, PACS/ASCS, CSIP (earbud pairs), VCP, MCP/CCP | client / server as the spec says | **vinculum-audio** | after A2DP |
| Auracast broadcast | source and assistant | **vinculum-audio** | later |
| GATT for apps | client to a chosen device | the app, through vinculumd (§11) | B6 |
| OBEX push/file transfer | receive into quarantine | a later file-receive service | later |
| PAN / BNEP | — | off; if ever, through netd | not planned |
| SPP / RFCOMM serial | — | dev only | not planned |
| MIDI over BLE | — | Lyra, later | later |

**Per-device permission.** On pairing, `vinculumd` lists the device's profiles; the trusted UI
shows them and the user can switch any off ("use as headphones: on; use as keyboard: off").
A profile that is off is never offered to its owner and its incoming connections are refused.
Nothing a device advertises turns a profile on by itself after pairing: a device that
gains a keyboard collection in a later descriptor needs the user to allow it.

## 9. Keys and Arca

Link keys (BR/EDR), LTKs, peer IRKs and our own IRK and identity root are Arca items with
recipient `vinculumd`.

The catch: a Bluetooth keyboard must work at the login screen, before any user has unlocked
Arca, and the controller asks for a link key within a few hundred milliseconds of a
connection. So:

- **Bluetooth bonds are machine-wide**, like Wi-Fi networks and like macOS and Windows: paired
  once, usable by every account, and at the login screen. The record notes who paired it, and
  only an administrator or that user may remove it.
- They need an Arca store that is **not tied to a user's unlock**: a machine store, its key
  sealed to the TPM where there is one (and from a machine secret on disk otherwise, with the
  same warning Arca's setup gives), class "after boot". Built by the Arca thread as arca.md §6.4
  (2026-11-07): `vinculumd` reaches `/atrium/sockets/arca/arcad.sock`, arcad knows it by
  uid (`ARCA_SERVICES=vinculumd=65024`), and it uses STORE / RELEASE / LIST / DELETE on
  item kind 7 (BluetoothKey), class 1 (after boot). No other service, app or root can
  reach those items; LOCKED means no machine store is open, and `vinculumd` then fails
  closed (no bonds, no pairing).
- At start, `vinculumd` lists and releases its items once and keeps them in `mlock`ed memory
  (jail allowlisted for `allow.mlock`, as `atrium-arcad` is), because it must answer
  `Link_Key_Request` and LE encryption requests without a round trip. New keys go to Arca
  at once (STORE); removing a device destroys its item.
- Peer IRKs are loaded into the controller's resolving list so reconnection works with
  rotating addresses.

Per-user behaviour on a machine-wide bond: audio devices follow the active seat (choragusd
already gates by seat); HID input goes wherever frescod routes it, as for USB.

## 10. The HID hand-off to Tactus

`vinculumd` listens on `/atrium/sockets/bluetooth-hid/hid.sock` (directory `_vinculum:_tactus`
0750, peer uid `_tactus` checked). tactusd connects once. The control stream is postcard,
length-prefixed, at most 4 KiB per message, like Lyra's wire:

| direction | message | payload |
|---|---|---|
| tactusd → | `Hello { version }` | |
| → tactusd | `Attach { device, record, descriptor, transport }` + one fd (SCM_RIGHTS) | `device`: the 64-bit handle (§6.3), stable across reconnects; `record`: name, vendor, product, version, `bus: Bluetooth`; `descriptor`: the HID report descriptor (up to 64 KiB, sent in a follow-up `Descriptor` chunk series if over 4 KiB); `transport`: `Classic` or `Le` |
| → tactusd | `Detach { device }` | link lost or device removed; the fd also reaches EOF |
| → tactusd | `Battery { device, percent }` | from BAS, when it changes |

The fd is one end of a `SOCK_SEQPACKET` socketpair, one per device, so each device is its own
channel, like a `hidraw` node. Each packet is a report:

```
u8  kind          // 1 Input, 2 Output, 3 Feature-get, 4 Feature-set, 5 Feature-reply
u8  report_id     // 0 when the descriptor has no report ids
u64 ts_ns         // CLOCK_MONOTONIC when vinculumd received the ACL packet (Input only)
..  report bytes  // as hidraw would give them, without the report id byte
```

- **Classic HID:** `vinculumd` gets the descriptor from the device's SDP record
  (`HIDDescriptorList`), opens the control (PSM 0x11) and interrupt (PSM 0x13) channels,
  strips the HIDP header from `DATA|Input` and adds it to Output/Feature. Report protocol
  always, never boot protocol.
- **HOGP:** the descriptor is the Report Map characteristic. Each Report characteristic's
  Report Reference descriptor gives its id and type; notifications become Input packets with
  that id; Output writes go to the matching characteristic (write without response for
  LEDs). Included-service external report references are resolved by `vinculumd`.
- **Ordering and loss:** Input packets are delivered in order. If tactusd's queue is full
  (it is slow or stuck), `vinculumd` drops the oldest Input packets for that device, never
  blocks, and sends `kind = 6 Dropped { count }`, so Tactus can release held keys.
- Tactus parses the descriptor and reports with the same code as for `hidraw` (rev. 2 §3.1),
  so a malicious Bluetooth keyboard and a malicious USB keyboard meet the same parser in the
  same jail.

## 11. Audio: `vinculum-audio`

Owned jointly with the audio thread; this section is the Bluetooth side, and the lyrad side
is the audio thread's to design.

- **Path.** `vinculumd` hands `vinculum-audio` the AVDTP signalling and media channels (and
  later RFCOMM DLCs, SCO and ISO streams) for devices with audio allowed, over
  `/atrium/sockets/bluetooth-audio/` (peer `_vinculum-audio`), with the device handle and
  record. `vinculum-audio` negotiates codecs, encodes and sends media packets back over the
  channel.
- **To Lyra: a Bluetooth sink is an output device.** The architecture already names it a
  clock domain of its own ([atrium-lyra-architecture.md](atrium-lyra-architecture.md) §3).
  lyrad gains an **external device** backend, designed by the audio thread in
  [lyra-external-device.md](lyra-external-device.md): `vinculum-audio` talks only to
  choragusd, on `/atrium/sockets/audio-device/` (restricted capability `audio-device`),
  announces the device and gets the consumer end of an f32 ring plus a device page it
  writes (state, `delay_frames`, underruns, rate). A codec change (AVDTP reconfigure, A2DP
  to HFP) is a Gone and a new Announce with the same device handle. The A2DP source is paced by our own monotonic clock (A2DP has no
  clock of its own; the sink adapts), so the domain is "system clock at the negotiated
  rate". AVDTP delay reports (A2DP 1.3) and our encode and controller buffering give the
  output delay, which lyrad puts in the clock page so the player keeps lip sync
  ([lyra-v1.md](lyra-v1.md) §6). For LE Audio the domain is the controller's ISO interval
  and presentation delay.
- **Codecs.** SBC (mandatory, royalty-free, written in Rust, small) first. LC3 next (liblc3,
  Apache-2.0, for LE Audio and HFP super-wideband). mSBC for HFP wideband. AAC, aptX and
  LDAC are left out until their licensing is looked at (decision 6).
- **AVRCP.** As target: play/pause/next from the headset go to choragusd as media-control
  actions (`ToMedia::Control`, the path media keys already use), so `vinculum-audio` holds
  the `media-control` capability. As controller: absolute volume goes both ways on the
  `audio-device` socket (`hw_volume`) and maps to the master volume, so the headset and the
  Forum bar agree without `vinculum-audio` holding `audio-control`.
- **HFP.** RFCOMM AT commands are text from the headset; their parser lives here, fuzzed.
  Voice needs a capture path, so it waits for Lyra L5.
- **Output choice** (which device plays) stays choragusd's: when headphones connect they
  appear as an output and choragusd's rules decide whether to switch, as for USB audio.

## 12. Apps: the `bluetooth` capability

`bluetooth` is already in the Portcullis install prompt
([atrium-pkg-install-ux.md](atrium-pkg-install-ux.md) §5). It means, on the Web Bluetooth
model:

- An app asks for devices by filter (service UUIDs, name prefix). **A system chooser** (Curia,
  trusted UI, not the app) shows matching devices nearby; the user picks one or cancels.
- The app gets a GATT client limited to that device and to the services it named, through
  `/atrium/sockets/bluetooth/` (one connection per app, peer uid resolved through the launch
  registry). The grant is remembered per (app, device) and revocable in settings.
- A **blocklist** of services no app gets: HID and HOGP, Fast Pair, FIDO/CTAP, firmware
  update services of HID devices, and anything the audio profiles own.
- Apps never scan, never pair, never see addresses or other apps' devices, and never get
  raw HCI or L2CAP. L2CAP credit-based channels and RFCOMM for apps are later and need their
  own review.

## 13. Control socket and trusted UI

`/atrium/sockets/bluetooth-control/` behind a new restricted Portcullis capability
`bluetooth-control`, held by the settings UI (Curia) and the Forum bar, like `audio-control`:
power on/off, scan for devices to pair, pair, confirm/enter passkeys, list and remove
devices, toggle profiles per device, the app grants of §12, and the chooser. Pairing prompts
(numeric comparison, passkey display or entry) are drawn only by this trusted UI; a pairing
request with no UI open is refused (§7). `vinculum-ctl` (CLI and agent) covers stock FreeBSD.
Curia's Bluetooth page is built (2026-11-15, [curia.md](curia.md) §14) and the capability
`bluetooth-control` exists in Portcullis. The Forum bar's Bluetooth item is built
(2026-11-26, `forum-bar/src/bluetooth.rs`): a plain client (never the agent), on the bar
while a paired device is connected, its menu listing each with its battery level and a
Disconnect, the icon amber at 20% or less. `DeviceInfo.battery` carries the level a
connected device last reported (Battery Service) and `Event::Battery { address, percent }`
each change; `vinculum-ctl devices` prints it and `vinculum-ctl watch [SECS]` prints events.
A Classic bond with no name (paired by address) is asked for it once connected (Remote
Name Request, ahead of a scan's devices); the renamed record is stored before the old is
deleted, and `Devices` goes to every client. If both records survive a crash, the load keeps the named one, since their keys match; only records with different keys for one device are both dropped. Names from devices (advertising, inquiry, Remote Name Request) lose control characters and bidirectional overrides on arrival (`vinculum_control::clean_name`). Battery news is told at once for a 5-point step or reaching 20%, otherwise at most once a minute.
Admission on Atrium: `vinculumd --control-apps REGISTRY APPS_DIR` lets in a uid that the
launch registry names as an app whose installed manifest holds `bluetooth-control = true`
(both read per connection; `vinculumd/src/apps.rs`, no Atrium crate). A user's scan
(`Scan { on: true }`) runs a Classic inquiry beside the LE scan, restarted each time it
ends and paused while a pairing pages its device; `Found` carries the Class of Device
for inquiry results, which pair with `PairClassic`. A result with no name is asked for it
(Remote Name Request, one at a time with the inquiry paused). We only ask: vinculumd never
enables inquiry scan, so it stays undiscoverable (§7). `Power { on }` turns Bluetooth off
or on: off drops every link and refuses scans, pairing, connects and incoming pages until
it is on again, and `--power-file PATH` keeps it across restarts (`vinculum-ctl power`).

## 14. Tests

- **Rootcanal** (AOSP's virtual controller, Apache-2.0) as the controller: `vinculum-hci`'s
  H4-over-socket transport talks to it; several virtual controllers on one Rootcanal give
  link-level tests with no radio, Classic and LE.
- **Bumble** (Google, Python, Apache-2.0) as scripted peers on Rootcanal: a Classic keyboard,
  a HOGP mouse, an A2DP sink that decodes and checks a tone, a headset sending AVRCP keys,
  earbuds as a CSIP set. And hostile peers: malformed L2CAP signalling, SDP, ATT, AVDTP and
  AT commands; key-size downgrade (KNOB); an unpaired device opening HID PSMs; a device
  reconnecting by a paired address without the key; a downgrade by re-pairing with legacy.
- **Host tests** per layer (sans-I/O state machines) and **fuzz targets** per parser.
- **VM:** the arm64 and amd64 cloud VMs run `vinculumd` jailed against Rootcanal over a
  socket (QEMU has no Bluetooth emulation), with tactusd and lyrad in their jails, e2e:
  pair keyboard → keystrokes reach a window; pair sink → tone decoded.
- **Real hardware:** a USB Bluetooth dongle passed through to the Mac's HVF VM (or a laptop),
  against real keyboards, mice and headphones. Which dongle and devices to buy or use is
  decision 7.
- **Qualification.** Shipping a product as "Bluetooth" needs Bluetooth SIG membership and a
  declared design. Not needed to develop; noted for later.

## 15. Build plan

| # | work | gate |
|---|---|---|
| **B0** | `vinculum-hci-proto` from PDL, H4 framing, Rootcanal + Bumble harness in the repo | Bumble peer's HCI traffic parsed by the host tests |
| **B1** | `vinculum-hci` (USB via libusb, H4 test transport, Intel + Realtek firmware, allowlist) and `vinculumd` skeleton (HCI flow control, LE + Classic connections); users, ruleset 25, services.d | controller up in the VM via Rootcanal; on a real dongle on the Mac |
| | *As built 2026-11-07:* host side done (`vinculum-host`, `vinculum-hci` with TCP/UART + allowlist, `vinculumd`; `harness/e2e.sh` passes). **B1b** left: USB via libusb, firmware loaders, users/ruleset/services.d and the VM run | |
| | *As built 2026-11-25 (B1b part 1, the dongles arrived: Raytac nRF52840 and TP-Link UB500):* USB transport in `vinculum-hci` (`usb` / `usb:VVVV:PPPP`) through the libusb-1.0 API (FreeBSD base libusb, BSD-2; libusb-1.0 on a macOS or Linux dev host), three threads behind an H4 socketpair so the relay is unchanged; commands as class requests on the control pipe, events on interrupt IN and ACL on bulk, each reassembled by `h4::Decoder::typed` with the same bounds; SCO and ISO dropped loudly until B5/B7. Realtek loader (`rtl.rs`) ported from FreeBSD's `rtlbtfw` (v1 and v2 patch files, project-id check, config appended, 252-byte Download fragments; an already-patched chip is left alone); checked against the real `rtl8761bu_fw.bin` and a fake controller. Dev bridge `--serve-tcp 127.0.0.1:PORT` (loopback only, whole frames only, Reset when the client leaves) so the VM's stack uses a dongle on the Mac through slirp's 10.0.2.2 without passthrough; checked against Rootcanal with `vinculum-probe`. nRF52840 runs our Zephyr 4.5 `hci_usb` build (`vinculum/firmware/README.md`). First real-radio run on the Mac 2026-11-25 (through `--serve-tcp` and `vinculum-probe`): both dongles reset, report their version and scan (15 nearby devices each); the UB500 takes the patch (LMP subversion 0x8761 to 0xd922) and a restart finds it already running. It found two gaps, both fixed: macOS leaves a device of class 0xE0 (the UB500) unconfigured, so `vinculum-hci` now sets the configuration that holds the HCI interface when none is; Zephyr controllers have no public address (BD_ADDR 0), so `vinculum-host` reads the chip's static random address with Zephyr's read-only vendor Read Static Addresses (0xFC09, added to the PDL, `pdl/CHANGES.md`), sets it with LE Set Random Address and uses it as our own (random) identity address in scans, connections and SMP; the allowlist passes vendor commands per (manufacturer, opcode), the manufacturer taken from the controller's own Read Local Version answer. Left: users/ruleset 25/services.d/dispensatord rule, the VM run through the bridge, then passthrough (ask first) | |
| | *As built 2026-11-25 (B1b part 2):* full stack on the Mac: vinculumd + vinculum-hci on the UB500 ready (public 6C:4C:BC:B5:37:04), `vinculum-ctl scan` lists nearby devices; real over-the-air LE Secure Connections pairing (numeric comparison, authenticated, key size 16, both sides the same number, bond stored) and ATT MTU exchange (server MTU 517) with a Bumble peer on the nRF (`harness/peer_advertiser.py ... le-only`). That run found that bring-up sent BR/EDR Read Buffer Size to the LE-only nRF (Unknown HCI Command); an LE-only controller (LMP feature bit 37) now goes straight to LE Read Buffer Size. Production wiring: driver jails mount read-only, so `vinculumd --transport listen:PATH --transport-uid 65025` listens and `vinculum-hci --host-connect PATH --host-uid 65024` connects, retrying once a second; vinculumd exits when its controller goes and the supervisor restarts it listening. `vinculum/etc/devices.d/25-vinculum.toml` (dispensatord: every E0/01/01 interface to its own `atrium-vinculum-hci-B-A-I` jail, ng_ubt detached, `ugenB.A` + `usb/B.A.0` granted), `vinculum/etc/services.d/46-atrium-vinculumd.toml`, devfs ruleset 25 (`atrium_bluetooth`, basics only) pinned in `jaild.policy.toml` to `atrium-vinculum-hci` as 65025, users 65024/65025 allowed, mount sources added; `scripts/vinculum-service.sh` commissions it. Left: the VM run (bridge first, then passthrough with Shankar's OK), learning libusb's ioctls for sysfil `device` | |
| | *VM run 2026-11-25 (bridge, b1090ed, shared Mac VM, stock 16.0-CURRENT GENERIC arm64):* vinculum-hci and vinculumd in the guest drove the nRF52840 on the Mac through `--serve-tcp` (guest `--controller tcp:10.0.2.2:6402`): ready, static address reported `random: true`, scan of 15 devices with no inquiry, then LE Secure Connections pairing with a Bumble peer on the UB500 (numeric comparison, authenticated, key size 16, bond stored) and ATT MTU 517; all violation counters 0. Not yet run: the jailed services (`vinculum-service.sh`, dispensatord's driver jail) and FreeBSD's libusb, which need passthrough | |
| | *Passthrough run 2026-11-25 (b1090ed, nRF52840 on qemu xHCI usb-host, stock GENERIC):* vinculum-hci on FreeBSD's base libusb (reader threads and a writer on one handle) detached ubt0, then vinculumd was ready (static address, random), scanned 15 devices, paired with LE Secure Connections (numeric comparison, key size 16) and exchanged ATT MTU 517, all counters 0; qemu logged no USB error during it. Found: GENERIC's devmatch loads ng_ubt at attach and again after a kldunload, and ng_ubt starts SCO isochronous transfers (qemu on macOS refused them, `[iso] -99`, the path that once panicked the Mac), so `vinculum-service.sh` now puts ng_ubt in `devmatch_blocklist`. Left: the jailed run (`vinculum-service.sh` + dispensatord) and sysfil `device` | |
| | *UB500 passthrough run 2026-11-26 (8562304, xHCI usb-host, no qemu USB errors):* unjailed on FreeBSD's libusb: `--report` correct (Realtek 0x005d, subversion 0xd922: still patched from the Mac, since passthrough does not power-cycle the dongle, so the firmware download through FreeBSD's libusb is still untested; replug just before the VM starts), ready with public 6C:4C:BC:B5:37:04, scan of 15, LE SC pairing with a Bumble peer on the nRF and MTU 517. The jailed run (deploy-portcullis.sh + vinculum-service.sh) found three faults, fixed after it: FreeBSD's libusb enumerates through `/dev/usbctl` (USB_READ_DIR) before it opens `ugenB.A`, so ruleset 25 now unhides usbctl (root 0644; its quirk ioctls need PRIV_DRIVER); interface 1 (SCO) is also class E0/01/01, so dispensatord started two driver jails per controller: rules take `usb_interface_number`, and ours names 0; and `arca-vm-test.sh` left its test arcad manifest (vinculumd=50804) installed, so arcad refused vinculumd: it now restores the real one, and `vinculum-service.sh` warns. Left: rerun the jailed run | |
| | *Unpatched UB500 through passthrough 2026-11-26 (b516262):* replugged before the VM started (LMP subversion 0x8761), every event read on interrupt IN 0x81 fails with LIBUSB_ERROR_PIPE, from the first Reset, also with dispensatord stopped and no jails; clearing halt on all three endpoints after the claim (b516262, kept) succeeds but changes nothing, and QEMU logs no USB error. The same chip unpatched loads the patch directly on macOS libusb, and patched it works through the same passthrough, so the stall is in QEMU usb-host on macOS, not in vinculum-hci. Cause (found in hw/usb/host-libusb.c): QEMU skips SET_CONFIGURATION for a one-configuration device and, finding no active configuration, claims no interface yet reports success; macOS leaves a class-0xE0 device (the UB500) unconfigured, so every endpoint transfer comes back STALL. The replug both unpatched and unconfigured the dongle; every working run had it configured on the Mac first. Workaround: configure it on the Mac (`vinculum-hci --report`, which loads nothing) before the VM starts; fix goes in our QEMU fork. Left: the jailed run with the dongle patched on the Mac first (`vinculum-hci --firmware-dir`, then start the VM); diagnose with guest `usbdump -i usbus0 -s 256` and QEMU `-trace 'usb_host_*'`; the firmware download through FreeBSD's libusb is proven on bare metal | |
| | *Jailed run 2026-11-26 (6f9e1ece0, UB500 patched on the Mac, then xHCI usb-host):* deploy-portcullis.sh + vinculum-service.sh with the real arcad manifest; dispensatord starts exactly one driver jail (`atrium-vinculum-hci-0-2-0`, uid 65025, ruleset 25 with usbctl; interface 1 shown to nobody), the driver connects to jailed vinculumd (uid 65024) and the controller comes up ready. It found three faults, fixed: libusb's auto-detach calls USB_IFACE_DRIVER_DETACH on every claim, which needs PRIV_DRIVER (claim failed LIBUSB_ERROR_OTHER), so vinculum-hci detaches only when a kernel driver is attached (2ad25d449); vinculumd bound the transport socket 0755, so connect(2) from the driver got EACCES, now 0660 and the driver logs a refusal once (6f9e1ece0); `arca-vm-test.sh` left its test bond key under the vinculumd label, read as a corrupt bond, now deleted at the end (9ecbe9e20). Then (66a0fd0f1, vinculum-ctl run as `_vinculum`, Shankar's OK): status (public 6C:4C:BC:B5:37:04), scan of 15, LE SC pairing with a Bumble le-only peer (numeric comparison, authenticated) 4.5 min after start, bond saved in Arca's machine store; after a vinculumd restart `bonds: 1` and it reconnects to the peer encrypted with the stored LTK. A fourth fault, fixed: vinculumd held one Arca connection from start-up and arcad drops idle clients, so the first bond write failed EPIPE; the store now resends a failed write on a fresh connection (66a0fd0f1). Not checked jailed (the control CLI does not expose them): ATT MTU and violation counters, both passed unjailed on the same chip | |
| | *Review 2026-11-26 (a2041333..66a0fd0f), fixed:* a STORE whose reply is lost may still have landed, so after a key-store I/O failure vinculumd deletes every stored bond it does not hold, and at start two bonds for one device are both dropped (that device pairs again); `--report` gives the kernel's driver back when claim() detached it; `arca-vm-test.sh` deletes its test bond and restores arcad's manifest from an EXIT trap. Open: all driver jails share `_vinculum-hci`, so with two controllers a driver compromised over the air could connect as the other's transport; give each controller its own transport socket or uid before multi-controller support | |
| | *Controller isolation, 2026-11-28:* Dispensatord publishes one listener per USB instance and jaild passes its fd only to that driver. See §4 for the directory and credentials. The two-controller host regression sends distinct H4 commands and events through two live drivers, checks both paths refuse direct connections without search permission and rejects a mismatched listener before opening its radio. A host test verifies publisher uid and rediscovery after detach. Two independent Rootcanal radios ran with separate Bumble advertisers: vinculumd selected 0-4-0 and found only RadioBeta, then after detach selected 0-3-0 and found only RadioAlpha. arm64 FreeBSD release cross-builds passed for dispensatord, vinculum-hci and vinculumd. This closes the 2026-11-26 transport review item. The new jailed wiring still awaits a FreeBSD run. | |
| | *Firmware load in FreeBSD 2026-11-26 (atrium-os/qemu 68beb8fd60, which configures a device the host left unconfigured):* control run first: the old QEMU with the replugged UB500 fails every read LIBUSB_ERROR_PIPE and the Mac shows the device with 0 interfaces; on the fixed QEMU the Mac shows both interfaces claimed by QEMU, and at boot the jailed driver (uid 65025) loads `rtl8761bu_fw.bin` (30210 bytes) through FreeBSD's libusb, LMP subversion 0x8761 to 0xd922, then vinculumd is ready with its bond from Arca. Every B1b path is now proven in the VM | |
| | *B4 on real radio 2026-11-26 (467567bab, all jailed: tactusd, vinculumd, driver jail; fixed QEMU):* a Bumble HID-over-GATT mouse on the nRF pairs (numeric comparison) with vinculumd on the UB500; tactusd gets `VinculumMouse` 1234:5678 [Pointer], motion (raw_dx 10 x6) and clicks, vinculumd reports battery 60%, and after the mouse drops the link vinculumd reconnects by itself and input flows again. Found and fixed: a background LE scan for an away bond ran with the duplicate filter on, so a user's scan never saw devices it had seen and pair by name failed (a user's scan now restarts it: 13 devices instead of 2, pair by name works); tactusd's socket was 0755, so frescod could not connect (now 0660). Left: battery in the Forum bar; B3 Classic HID needs a Classic keyboard or a second Classic dongle | |
| | *B2 Classic over the air 2026-11-26 (Shankar's iPhone, UB500 host, jailed):* inquiry finds the phone; SSP numeric comparison matches on both sides (539315); the link key is authenticated P-192 and vinculumd refuses it ("Secure Connections required"), storing nothing, as §7 says. Cause: the RTL8761BU (patched, 0xd922) has no BR/EDR Secure Connections (LMP features page 2 = 0x25f, bit 8 clear). `--report` now prints pages 1 and 2, bring-up reads page 2 and vinculumd says at start when a controller lacks it (6a9c89560). Shankar decided 2026-11-26: keep authenticated P-192 there, marked weaker, keyboards included (§7 table); built in the same round (1ada890c4). Rerun with the iPhone: numeric comparison 453047 on both sides, bond kept as authenticated P-192 (`devices`: "weaker key (no Secure Connections)"), reloaded after a vinculumd restart, and `vinculum-ctl connect` re-encrypts with the stored key without a prompt. Found: the controller had no local name, so the phone listed a bare address (bring-up now sets one, `--name`, default Atrium, 6e943a2d3); a bond paired by address has an empty name (a Remote Name Request after pairing is still to do) | |
| | *Loose ends 2026-11-26:* battery levels reach the control clients (`DeviceInfo.battery`, `Event::Battery`), the Forum bar's Bluetooth item, Curia's device line and `vinculum-ctl devices`/`watch`; a Classic bond paired by address is named by Remote Name Request; a bond name is cut on a character boundary (a cut mid-character no longer makes a record that will not decode). The driver jail's filter is now `usb-driver`, enforcing (mode 2 ahead of the policy-wide log mode, `[sysfil_class_modes]`, a top-level table an older jaild ignores rather than refusing the policy): base plus the 18 libusb20 ugen ioctls vinculum-hci's calls reach, read from FreeBSD's `libusb20_ugen20.c` with sizes from `usb_ioctl.h`, and no reset, power mode or driver detach/attach. `e2e.sh` checks the name (B3) and the level in the device list and as an event (B4). *Mac run 2026-11-26 (kernel.atrium, atrium-os/qemu 49a9ea15ee):* enforcing found one missing call, `__specialfd` (eventfd, from libusb_init), now in the profile (6c9d6fb61); with it the driver jail on the UB500 reads `sysfil.mode 2, denied 0`, every seen list empty, through firmware check, pairing (numeric comparison), HOGP motion and clicks, automatic reconnect and `battery 60%` in the device list, and again after three dispensatord restarts (QEMU survived each). On stock GENERIC (no sysfil) jaild creates the jail without the filter and says so | |
| **B2** | security manager: SSP + BR/EDR SC, LE SC, legacy by override, key-size check, no-downgrade; Arca machine store items; `bluetooth-control` + `vinculum-ctl` | pair a Bumble keyboard with numeric comparison; KNOB/BIAS/BLURtooth tests refused |
| | *As built 2026-11-07 (part 1):* `vinculum-smp` (SMP PDUs from PDL; crypto checked against Bumble vectors; sans-I/O LE SC initiator, all association models) and host wiring (SMP channel, LE Start Encryption, key size 16, SMP timeout ends the link, LTK requests refused, disconnects drain queued data). `e2e.sh` pairs with Bumble in each model and reconnects with the stored key. Part 2: `vinculum-keys` (bond record; sealed AES-256-GCM file store for stock FreeBSD; Arca machine store client behind `atrium`; RPA resolution by IRK); a bond survives a vinculumd restart in `e2e.sh`; vinculum-hci drops the old session's frames until its own reset completes. Part 3: `vinculumd` policy (pairable window, no key downgrade, keyboard reports only on authenticated SC keys, Security Request from strangers ignored); `e2e.sh` shows a Just Works re-pair refused against an authenticated bond. Part 4: `vinculumd --control` runs as the service with the `bluetooth-control` socket (`vinculum-control` wire: postcard, 4 KiB, uid-checked, 8 clients; prompts only to pairing agents, no agent means no pairing) and `vinculum-ctl` (status, devices, scan, pair, connect, disconnect, remove, agent); `e2e.sh` pairs, lists, reconnects encrypted and removes through it. Part 5: Classic Secure Simple Pairing (`vinculum-host` `classic.rs`): SSP + SC host support at bring-up, we stay central, link keys must be P-256, AES-CCM required on SC bonds, Read Encryption Key Size == 16, unsolicited pairing refused, incoming connections only for Classic bonds; `e2e.sh` pairs over BR/EDR and reconnects with the stored link key from a new process. **B2 done** except page scan for bonded devices that reconnect on their own (B3, with Classic HID) | |
| **B3** | SDP client, Classic HID, HID hand-off to tactusd (§10) | keystrokes from a Bumble Classic keyboard reach a window in the VM; unpaired injection refused |
| | *As built 2026-11-07:* L2CAP basic-mode dynamic channels on BR/EDR (`vinculum-host` `l2cap.rs`: incoming channels only on encrypted links and registered PSMs, 8 per link, MTU 672, 10 s signalling timeout); `vinculum-sdp` (bounded ServiceSearchAttribute client with continuations, fuzzed); `vinculumd --hid PATH` (`hid.rs`): on a bonded Classic link once encrypted, SDP for the HID record (descriptor, name) and PnP record (vendor, product), then control 0x11 and interrupt 0x13, opened by us on links we made and taken from the device on links it made (never both, so the two sides do not race); report ids found from the descriptor; reports to tactusd over `vinculum-hid-wire` (Attach with a SEQPACKET fd, Descriptor chunks, Detach), written without blocking, drops counted and announced with `Dropped`; output and feature reports back to the device; page scan while Classic bonds exist; `vinculum-ctl pair ADDR --classic`. `e2e.sh` pairs a Bumble Classic keyboard through vinculum-ctl, a real `tactusd` gets the device (vendor/product from PnP) and `tactusd --watch` sees the keys; the keyboard then drops the link, pages us, opens its channels and types again. Left: the VM gate (keystrokes in a window), LE HID is B4 (HOGP) | |
| **B4** | ATT/GATT client, BAS/DIS, HOGP | HOGP mouse moves the pointer; battery in the Forum bar |
| | *As built 2026-11-07:* `vinculum-gatt` (sans-I/O ATT client and HID-over-GATT discovery: MTU 247, primary services, characteristics, descriptors; Report Map up to 512 bytes with Read Blob; Report References; input-report notifications on; PnP ID from DIS; Battery Level with notifications; output reports as Write Commands; requests to our empty server answered with errors; 30 s ATT timeout drops the link; capped counts; fuzzed). `vinculumd --hid` runs it on bonded LE links once encrypted and hands the device to tactusd as `Transport::Le` with `Battery` messages; while a bonded LE device is away it scans in the background and reconnects when the device advertises (RPAs resolved with the bond's IRK); LE connection attempts are cancelled after 30 s. `e2e.sh` pairs a Bumble HOGP mouse, a real tactusd sees its motion and clicks, the battery change reaches vinculumd, and after the mouse drops the link it is reconnected without being asked. Left: battery in the Forum bar (Tactus forwards it), feature reports over GATT, the VM gate | |
| **B5** | `vinculum-audio`: AVDTP, A2DP source with SBC, AVRCP, lyrad external device (with the audio thread) | Bumble sink decodes a tone; play/pause from the sink pauses the player; A/V offset within lyra-v1 §8's bound |
| **B6** | app GATT through the chooser (§12) | an app reads a heart-rate sensor it was granted; blocked services refused |
| **B7** | LE Audio unicast (ISO, BAP, CSIP, VCP, LC3) | Bumble LE Audio earbud pair plays in sync |
| **B8** | HFP AG with Lyra capture | call audio both ways |

B3 and B4 are what Tactus needs (its T6). B5 is what headphones need. MediaTek and Broadcom
firmware, UART, Auracast, OBEX come after.

### 15.1 B5 A2DP source, as built 2026-11-28

The first B5 gate runs on the host. `vinculum-audio` discovers an available Classic audio
sink endpoint, reads its capabilities, configures SBC, opens a second L2CAP channel for
media, starts the stream, then closes and releases it. Its PDL parser accepts single-packet
AVDTP replies with the outstanding transaction label and signal ID. The profile supports
48 and 44.1 kHz, joint stereo and stereo, 16 blocks, 8 subbands and loudness allocation.
It keeps the negotiated bitpool within the peer's range and fits whole SBC frames in the
media MTU. The Rust encoder is `nanoaudio-sbc` (MIT). The portable workspace builds without Lyra.

`vinculum-atrium-audio` announces the negotiated format through the existing choragusd
`audio-device` client. It reads interleaved f32 samples on a monotonic schedule, pads a
short read with silence and publishes the consumed position, underrun count and estimated
delay on the device page. For now the downstream delay is supplied at connection time.

Run `sh scripts/vinculum-a2dp-check.sh` with `VINCULUM_VENV` pointing to a Python environment
with Bumble (`0.0.236` tested). The sink uses FFmpeg as an independent test decoder
(`FFMPEG` can name its executable). Two Bumble controllers communicate over an in-process
Classic link. The source-side fixture forwards the Rust profile's opaque L2CAP packets.
A local peer stands in for choragusd and lyrad, passing the ring and page over the actual
audio-device protocol. On the Mac the fixture uses unlinked files for the mappings.

Both runs decoded 48 RTP packets (30,720 stereo frames), with 440 Hz on the left and
880 Hz on the right. One negotiated 48 kHz joint stereo at bitpool 51, the other 44.1 kHz
stereo at bitpool 35. The gate checks the negotiated SBC frame headers, packet lengths,
RTP sequence and sample timestamps, tone amplitude and spectral energy, media channel
release and device removal. The final short read produced one underrun and left Lyra's
consumed position at 30,400 frames.

**Next**: discover the Audio Sink SDP record on a bonded, encrypted Classic link with audio
permission and hand its signalling and media channels to a jailed `vinculum-audio` process.
Use that path with running choragusd and lyrad, then add AVDTP delay reports and verify the
§8 clock bound in [lyra-v1.md](lyra-v1.md). AVRCP play/pause through media-control is open.

## 16. Decisions, with recommendations

**Accepted by Shankar, 2026-11-07: all nine, as recommended (decision 3 as amended).**

1. **A new user-space stack, not netgraph.** *Recommend yes,* for §2: the old stack lacks SSP,
   all of LE security and GATT, and all audio, so nearly all the work is new either way, and
   building it on netgraph would put the radio's parsers in the kernel. Nothing of netgraph
   Bluetooth is loaded on Atrium; FreeBSD's firmware loaders are ported.
2. **Three processes: transport, host, audio, each jailed, plus Tactus for HID** (§4).
   *Recommend yes.* The transport split keeps a compromised host from reflashing the radio;
   the audio split keeps the biggest profile parser away from link keys.
3. **Rust throughout, with packet parsers generated from PDL** (AOSP's grammar files as the
   start). **Decided by Shankar, 2026-11-07** (§6.4): all three processes in Rust, no C
   transport; usable on stock FreeBSD as a port.
4. **Bonds are machine-wide and their keys live in a new Arca machine store** usable before
   login (TPM-sealed where present), so a Bluetooth keyboard works at the login screen.
   *Recommend yes.* The alternative (per-user bonds in the user's Arca) means no Bluetooth
   keyboard at login and re-pairing per account. This adds a store class to arca.md, so the
   Arca thread would build it.
5. **The strict defaults of §7:** no channel on unbonded links; keyboards need MITM-protected
   pairing; Secure Connections required, legacy pairing only by per-device override (never
   for keyboards); 16-byte keys; pairable only while the UI is open. *Recommend yes.* A few
   old devices (pre-2014 LE legacy keyboards) will need the override or won't work as
   keyboards.
6. **Codecs: SBC first, then LC3 and mSBC; no AAC, aptX or LDAC for now.** *Recommend yes.*
   SBC works with every A2DP device; the others need a licence look (AAC patents, aptX
   Qualcomm, LDAC Sony's terms) before we ship them.
7. **Hardware for testing.** *Recommend* one Intel AX210-class USB or M.2 card or a Realtek
   RTL8761B USB dongle (both firmware loaders exist in FreeBSD today), passed through to the
   Mac's HVF VM; plus whichever Bluetooth keyboard, mouse and headphones you already have.
8. **The name Vinculum** (`vinculumd`, `vinculum-hci`, `vinculum-audio`). If agreed it goes
   into `docs/NAMING.md`.

9. **Usable on stock FreeBSD** (§4.1): the core depends on no Atrium crate, Atrium parts
   sit behind six seams with plain FreeBSD versions (file key store, rc.d + Capsicum,
   devd, evdev/vkbd for HID, cuse/virtual_oss for audio, a CLI for pairing), CI builds it without Atrium, and
   it ships as a port. *Recommend yes;* the cost is a trait per seam and a second, small
   implementation of each, and the FreeBSD gap it fills is real. The app capability and
   chooser stay Atrium-only.

## References

- Bluetooth Core Specification 6.0; A2DP 1.4, AVDTP 1.3, AVRCP 1.6, HFP 1.9, HID 1.1, HOGP
  1.0, BAP 1.0, LC3 1.0.
- [atrium-input.md](atrium-input.md) rev. 2 §3.3 (Tactus), [lyra-v1.md](lyra-v1.md),
  [atrium-lyra-architecture.md](atrium-lyra-architecture.md), [arca.md](arca.md),
  [portcullis.md](portcullis.md), [atrium-player-pip-keys.md](atrium-player-pip-keys.md)
  (media-control).
- FreeBSD: `sys/netgraph/bluetooth/`, `usr.sbin/bluetooth/` (read 2026-11-07).
- AOSP `packages/modules/Bluetooth` (PDL files, Rootcanal, interop database), Bumble,
  `pdl-compiler`, liblc3, Fuchsia `src/connectivity/bluetooth`, Zephyr `subsys/bluetooth`.
