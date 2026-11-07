# Vinculum: the Bluetooth service

**Status:** proposal, 2026-11-07. Decisions in §16 pending (9 since 2026-11-07). **Owner:** the Bluetooth thread.
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
  reused, in C, inside the transport (§5.2, §6.4); `hccontrol` is a reference for command coverage.
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
 │ one per controller    │  socketpair      │ HCI, GAP, L2CAP, SMP/SSP, SDP,     │
 │ _vinculum-hci         │                  │ ATT/GATT, RFCOMM, ISO routing,     │
 │ firmware load, framing│                  │ device DB, per-device permissions  │
 │ command allowlist     │                  │ _vinculum, no network, Capsicum    │
 └──────────────────────┘                  └──┬─────────┬──────────┬─────────┬───┘
                                   HID fds    │  audio   │  control │  apps   │ keys
                                              ▼          ▼          ▼         ▼
                                         tactusd   vinculum-audio  Curia /   arcad
                                         (_tactus) (_vinculum-audio) Forum   (_arca)
                                                    │      │  bar
                                          ring+clock│      │media-control,
                                                    ▼      ▼audio-control
                                                  lyrad  choragusd
```

| process | user (proposed uid) | jail | holds | parses remote data |
|---|---|---|---|---|
| `vinculum-hci` | `_vinculum-hci` (65024) | own; devfs ruleset 25 `atrium_bluetooth` shows only its controller's `ugen`/`usb` nodes or tty | the device; firmware files read-only | only packet framing (lengths) |
| `vinculumd` | `_vinculum` (65023) | own; no devices, no network | one socket per controller to `vinculum-hci`; service sockets; Arca link | yes, everything from the air up to the profile boundary |
| `vinculum-audio` | `_vinculum-audio` (65025) | own; no devices, no network | channels from vinculumd; a ring to lyrad; choragusd control | AVDTP, AVRCP, HFP AT commands, codec frames |
| `tactusd` | `_tactus` (65018) | input thread's jail | HID fds from vinculumd | HID descriptors and reports (as for USB) |

uids are proposals; they get reserved in `jaild.policy.toml` when B1 lands, clear of whatever
the input thread's `_tactus` (65018). The input thread accepted §10 as written (2026-11-07).

**Why three of ours and not one.**
- `vinculum-hci` is separate so that a compromised `vinculumd` cannot talk to the USB device
  directly, and cannot send vendor commands that write controller RAM or firmware patches
  (persistence in the radio). It filters commands (§5.3). It is small and written in C (§6.4).
- `vinculumd` is the big parser. It is in Rust with no `unsafe` outside a reviewed allowlist,
  its parsers are generated (§6.1), and it holds link keys for paired devices but nothing else
  worth stealing: no user files, no network, no display.
- `vinculum-audio` is separate because audio profiles bring the largest amount of
  per-device protocol (codec capability negotiation, AVRCP metadata, the HFP AT command text
  parser) and because its other end is lyrad, which runs at real-time priority. A compromise
  of it reaches the audio ring and the media-control socket, not link keys or HID.

**Who starts what.** `vinculumd` is a services.d manifest started at boot, before the login
screen, so a Bluetooth keyboard works at Vestibulum. When `atrium-devevents` reports a
controller (USB class `0xE0/0x01/0x01`, or a known UART), portcullisd unhides that node in
ruleset 25 and starts a `vinculum-hci` instance for it, with one end of a socketpair; the
other end goes to `vinculumd` over its transport socket (peer uid `_vinculum-hci` checked).
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
`/usr/local/share/atrium/firmware/bluetooth/` (read-only mount), reusing FreeBSD's loaders' C code
(BSD-2) and adding MediaTek. Each vendor's firmware licence is checked before we
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

### 6.4 Language: why the host is Rust and the transport is C

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

**Where C does fit: `vinculum-hci`.** The transport parses only frame lengths, talks to
`libusb` and `uart`, and runs the vendor firmware loaders, which already exist in C in
FreeBSD (`iwmbtfw`, `rtlbtfw`, `bcmfw`, `ath3kfw`, BSD-2). Written in C to style(9), it
reuses that code instead of porting it, builds with FreeBSD's `bsd.prog.mk`, and is the
piece most useful to stock FreeBSD on its own (one firmware-loading, command-filtering HCI
transport for every vendor). Its interface to the host is the plain H4 frame of §5.1, so
the language boundary costs nothing: no FFI, no shared structs.

**Result:** C for `vinculum-hci` (small, kernel-adjacent, FreeBSD style); Rust for
`vinculumd` and `vinculum-audio`, with the six seams of §4.1 for stock FreeBSD; ship as a
port. If FreeBSD ever wants it in base, the host can offer a C ABI (`libvinculum` through
`cbindgen`) without a rewrite, but that is upstream's choice.

## 7. Security policy

These are defaults; each "override" is a per-device setting the user makes in the trusted
pairing UI, with the risk spelled out.

| rule | stops |
|---|---|
| **No profile channel on an unbonded link.** HID, audio and app channels are handed out only for a bonded device whose link is encrypted with that bond's key. Incoming HID connections from anything else are refused at L2CAP | CVE-2023-45866 family (keystroke injection by an unpaired "keyboard") |
| **HID keyboards need an authenticated (MITM-protected) key.** A device whose HID descriptor has a keyboard collection and paired with Just Works gets mouse/consumer reports only until re-paired with passkey or numeric comparison | a nearby attacker pairing a fake keyboard during a Just Works window |
| **16-byte encryption keys.** Encryption key size is read after every encryption change; under 16 bytes, disconnect | KNOB |
| **Secure Connections by default.** BR/EDR SC and LE SC; legacy pairing only by override, and never for keyboards | weak legacy keys, BLUFFS |
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
  same warning Arca's setup gives), class "after boot". This does not exist in arca.md today
  (its key hierarchy starts at a user credential, §6.1), so it is decision 4.
- At start, `vinculumd` asks Arca for its items once and keeps them in `mlock`ed memory
  (jail allowlisted for `allow.mlock`, as `atrium-arcad` is), because it must answer
  `Link_Key_Request` and LE encryption requests without a round trip. New keys go to Arca
  at once (`arca_store`, recipient `vinculumd`); removing a device destroys its item.
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
  Proposed: lyrad gains an **external device** backend: instead of OSS, it writes the
  device's mix into a shared ring that `vinculum-audio` reads, with the device's rate and
  delay published back. The A2DP source is paced by our own monotonic clock (A2DP has no
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
  the `media-control` capability. As controller: absolute volume maps to the output's
  volume in choragusd (`audio-control`), so the headset's volume and the Forum bar agree.
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
request with no UI open is refused (§7). Until Curia exists, a `vinculum-ctl` CLI and a Forum
bar menu cover it, as `forum-ctl` did for displays.

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
| **B2** | security manager: SSP + BR/EDR SC, LE SC, legacy by override, key-size check, no-downgrade; Arca machine store items; `bluetooth-control` + `vinculum-ctl` | pair a Bumble keyboard with numeric comparison; KNOB/BIAS/BLURtooth tests refused |
| **B3** | SDP client, Classic HID, HID hand-off to tactusd (§10) | keystrokes from a Bumble Classic keyboard reach a window in the VM; unpaired injection refused |
| **B4** | ATT/GATT client, BAS/DIS, HOGP | HOGP mouse moves the pointer; battery in the Forum bar |
| **B5** | `vinculum-audio`: AVDTP, A2DP source with SBC, AVRCP, lyrad external device (with the audio thread) | Bumble sink decodes a tone; play/pause from the sink pauses the player; A/V offset within lyra-v1 §8's bound |
| **B6** | app GATT through the chooser (§12) | an app reads a heart-rate sensor it was granted; blocked services refused |
| **B7** | LE Audio unicast (ISO, BAP, CSIP, VCP, LC3) | Bumble LE Audio earbud pair plays in sync |
| **B8** | HFP AG with Lyra capture | call audio both ways |

B3 and B4 are what Tactus needs (its T6). B5 is what headphones need. MediaTek and Broadcom
firmware, UART, Auracast, OBEX come after.

## 16. Decisions, with recommendations

1. **A new user-space stack, not netgraph.** *Recommend yes,* for §2: the old stack lacks SSP,
   all of LE security and GATT, and all audio, so nearly all the work is new either way, and
   building it on netgraph would put the radio's parsers in the kernel. Nothing of netgraph
   Bluetooth is loaded on Atrium; FreeBSD's firmware loaders are ported.
2. **Three processes: transport, host, audio, each jailed, plus Tactus for HID** (§4).
   *Recommend yes.* The transport split keeps a compromised host from reflashing the radio;
   the audio split keeps the biggest profile parser away from link keys.
3. **Rust for the host and audio, with packet parsers generated from PDL** (AOSP's grammar
   files as the start); **C, in FreeBSD style, for the transport** `vinculum-hci`, reusing
   FreeBSD's firmware loaders (§6.4).
   *Recommend yes:* memory safety where the radio is parsed (the host also types into the
   session), C where FreeBSD code already exists. Both work on stock FreeBSD as a port.
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
