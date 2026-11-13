# Sigillum: identity verification

**Status:** accepted 2026-11-13 (Shankar: all eleven §17 decisions as recommended), proposed the same day at his request ("identity verification in Atrium.
Fingerprint, FIDO2, USB stick certificate etc. The Navigator needs to be able to access these
when needed. Apply the standard Atrium security, jails and protections"). §17 records the
decisions. S1 (§16) is built and host-tested (`sigillum/`); the rest is not.
**Owner:** the identity thread. The name was accepted with §17 (Latin *sigillum*: a personal seal,
the ring a Roman pressed into wax to prove a document was his).

Atrium needs one place that answers two questions for the rest of the system:

1. **Is the user present, and is it really them?** (user presence and user verification:
   a touch, a fingerprint, a PIN, the passcode)
2. **Prove it to someone else with a key the user carries.** (a FIDO2 security key, a
   smartcard or USB token holding a certificate, later a phone)

Sigillum is that service. It owns every authenticator device and every user-verification
prompt. It is not a key store: keys Atrium itself holds live in [Arca](arca.md), and Arca
stays the platform passkey authenticator (arca.md §8.4). It is not the login screen: that is
[Vestibulum](vestibulum.md) with ostiarius. Sigillum is what both of them, the Navigator and
apps call when a person or a device the person holds has to vouch for something.

## 0. The short version

- **Devices.** FIDO2/U2F keys over USB HID, smartcards and USB tokens over CCID (PIV,
  OpenPGP card), fingerprint sensors over USB, and later FIDO over Bluetooth LE (through
  Vinculum), NFC (through CCID contactless readers) and phones (hybrid). Each attached
  device gets **its own driver process in its own jail**, which sees exactly one device node,
  has no network, and parses everything the device says. A malicious USB device reaches only
  that jail.
- **The broker, `sigillumd`,** holds no device and no network. It runs ceremonies (register,
  assert, sign, verify the user), applies policy, and talks to clients by kernel peer uid,
  like Arca. It never parses device bytes beyond a small fixed record the driver sends.
- **The trusted prompt, `sigillum-ui`,** draws every sheet ("Sign in to example.com with
  your security key", "Enter the PIN for your YubiKey", "Touch the sensor"). PINs and
  passcodes go from frescod to `sigillum-ui` and never reach an app or the Navigator.
  Nothing touches a device until the user presses on that sheet.
- **The Navigator** stays the WebAuthn client (it checks the RP ID against the origin its
  courier connected to and builds `clientDataJSON`, arca.md §8.4). It asks Sigillum for a
  ceremony; Sigillum offers Arca's platform passkeys and any security key, and returns the
  assertion. TLS client certificates on smartcards are signed through Arca's courier path:
  the courier asks Arca, Arca asks Sigillum, the card signs.
- **Arca's verify-each-use class** (arca.md §6.2), which today answers `UNSUPPORTED`
  (`arcad/src/server.rs`), is served by Sigillum's `VERIFY_USER`.
- **Fingerprints never unlock a cold store.** A fingerprint can unlock the lock screen and
  satisfy verify-each-use only while the user's keys are already in memory, within limits
  (48 hours since the passcode, 5 failed matches, never after a reboot). Templates never
  leave the sensor where the sensor can match on chip, and never leave the driver jail
  otherwise.

## 1. What exists today, checked in the source

Read 2026-11-13 in `freebsd-src` main and in this repo.

| item | finding | consequence |
|---|---|---|
| `hidraw(4)` | one node per **HID device** (per `hidbus` instance), not per top-level collection: `hidraw_identify` adds one child per `hidbus`, unit = the `hidbus` unit (`sys/dev/hid/hidraw.c`) | a FIDO key's FIDO interface is its own `hidrawN`. A YubiKey's OTP keyboard is a separate USB interface, so a separate `hidbus` and a separate node. [atrium-input.md](atrium-input.md) §3.1 says "every top-level collection"; that is slightly wrong and harmless for Tactus |
| `hidraw_open` | **exclusive**: a second open gets `EBUSY` | whoever opens a key first owns it. Today that is tactusd (next row) |
| devfs ruleset 26 `atrium_input` | unhides `hidraw*` for tactusd, all of them | **today tactusd can open every FIDO key.** tactusd parses untrusted HID; if it is compromised it can send CTAP commands, including `authenticatorReset` within 10 s of a replug. §5.1 fixes this |
| `hidbus_child_pnpinfo` | each top-level collection's devd event carries `page=0x.... usage=0x.... vendor product` | the kernel already parsed the report descriptor; userspace can classify a device as FIDO (usage page `0xF1D0`) **without parsing any device bytes as root**. A FIDO collection has no kernel driver, so devd reports it as `nomatch` on `hidbusN` |
| `u2f(4)` (FreeBSD 15+, loaded by default in 16-CURRENT) | attaches to a FIDO collection as `/dev/u2f/N` (named `uhidN` in devd and devinfo, for old software), and sets the bus's `HQ_NO_READAHEAD`: the interrupt pipe is restarted only by a read or poll of `/dev/u2f/N` (`sys/dev/hid/u2f.c`, `usbhid.c`, read 2026-11-13) | the same key's `hidrawN` then sees one report per request and misses the rest of every multi-report answer (found in the S2e VM gate). A key `u2f(4)` drives is driven through `/dev/u2f/N`, as libfido2 does; `hidrawN` only when no `u2f` driver attached |
| CCID | no kernel driver; `ugen(4)` per USB device. Ports have `pcsc-lite` + `ccid` (C, one daemon for every reader) | we write our own CCID transport in Rust, one jail per reader (§6.2) |
| fingerprint | FreeBSD has nothing (no `libfprint` port that works with current sensors, no kernel driver) | §6.3; needs hardware before it is real |
| `libfido2` | in base as a private library for OpenSSH `sk-*` keys (`lib/libfido2`) | not used: C, and it parses device responses in the calling process. Its tests and known quirks are a reference |
| Arca | `Class::VerifyEachUse` answers `UNSUPPORTED` "needs Vestibulum's check (A0c)" (`arca/arcad/src/server.rs:106`); `Kind::P256` exists for passkeys but nothing signs with it yet (A4) | §8 provides the check; §9.2 adds Arca's passkey ops |
| Vinculum | FIDO/CTAP is on the app blocklist (bluetooth.md §12) | right; §6.4 routes FIDO-over-BLE to Sigillum instead |
| Tactus | unknown usage pages become `Raw`, delivered to apps holding `input.raw_hid` (atrium-input.md §3.5, §4) | a FIDO collection must never be `Raw`: §5.1 |
| QEMU | `-device u2f-emulated` / `u2f-passthru` (U2F HID) and `-device usb-ccid -device ccid-card-emulated` exist upstream | VM gates for the HID and CCID paths without hardware (§15), once our QEMU build has `libu2f-emu` and `libcacard` |

## 2. Threats

The attackers of the Navigator threat model (§1 there) plus the ones that come with devices.

| attacker | wants | stopped by |
|---|---|---|
| **hostile page** (A1) | log in as the user somewhere else (phishing); silently probe which keys are attached (tracking); make the user approve something they can't see | RP ID is checked by the broker against the origin the courier actually connected to (§9.1); no device is touched before a press on Sigillum's own sheet, which names the site (§7.2); every failure looks the same to the page (§9.4) |
| **compromised Navigator worker or engine** | use a credential for another origin; read a PIN | workers never talk to Sigillum; only the broker does, and only under its own label (§4). PINs go to `sigillum-ui` (§7.3) |
| **compromised app** | assert for an RP it does not own; reach a device directly | RP IDs an app may use come from its signed manifest and a verified association (§13); apps never see device nodes; Tactus never exposes FIDO collections (§5.1) |
| **malicious USB device** (BadUSB, a fake key) | exploit a parser; impersonate a keyboard | its bytes are parsed only in its own driver jail (§6); `sigillumd` re-checks a fixed-size record; the device is classified by the kernel's pnpinfo, never by a root parser |
| **compromised driver jail** | forge a fingerprint match; replay a signature; reach other devices | one device per jail, no network, no other nodes; a forged "match" from a host-matching sensor is bounded by §11's policy (it unlocks nothing cold); secure sensors' match results are authenticated end to end (§11.2) |
| **thief with the laptop** | unlock with a lifted fingerprint; guess PINs | biometrics never unlock the store at boot (§11.4); PIN retries are the device's own counters; the passcode stays the root (arca.md §6.1) |
| **tracker** | a stable identifier across sites | attestation defaults to `none` and the AAGUID is zeroed (§9.3); credential IDs are per RP; devices are never enumerated to anyone but the trusted UI |
| **shoulder-surfer / overlay** | trick the user into approving the wrong site | the sheet is a trusted window with the same press rules as Arca's fill chip (OBSCURED, arca-autofill.md §4.1); it shows the RP the broker verified, not a page string |

Out of scope: an attacker with root on the host, a compromised kernel, and a key whose
firmware is malicious (it can sign anything; it could not learn other keys' secrets).

## 3. What Sigillum serves

| consumer | asks for | through |
|---|---|---|
| Navigator broker (`navigatord`) | WebAuthn `create` / `get` for an origin it verified; TLS client certificate choice | §9; §10 via Arca's courier |
| Arca (`arcad`) | "verify the user" for a verify-each-use item; passkey creation and assertion run by Sigillum on Arca's platform authenticator | §8; §9.2 |
| Vestibulum (ostiarius, the lock screen) | unlock the lock screen by fingerprint or security key; FIDO2 `hmac-secret` as a second factor mixed into the root key (arca.md §6.1) | §12 |
| apps (`identity` capabilities) | WebAuthn for their own RP IDs; "sign with a certificate the user picks" | §13 |
| system UI (Curia, admin prompts) | step-up verification before a privileged change | §8 |
| SSH (later) | `sk-ecdsa` / `sk-ed25519` keys through Arca's SSH agent (arca.md A5) | §9 path, RP `ssh:` |

Sigillum serves nobody else. In particular no app, worker or engine ever gets a device fd,
a list of devices or a raw APDU channel.

## 4. Processes, users, jails

```
 USB HID (hidraw) ─┐    USB CCID (ugen) ─┐    USB sensor (ugen) ─┐   BLE (vinculumd fd)
                   ▼                     ▼                       ▼            ▼
          ┌──────────────┐       ┌──────────────┐       ┌──────────────┐ ┌──────────────┐
          │ sigillum-fido │       │ sigillum-ccid │       │ sigillum-bio  │ │ sigillum-fido │
          │ one per key   │       │ one per reader│       │ one per sensor│ │ (BLE transport)│
          │ CTAPHID, CBOR │       │ CCID, APDU,   │       │ sensor proto, │ │               │
          │ PIN protocol  │       │ PIV, OpenPGP  │       │ match / enrol │ │               │
          └──────┬────────┘       └──────┬────────┘       └──────┬────────┘ └──────┬────────┘
                 │ driver socket, fixed records (§14.2)                              │
                 └───────────────┬───────────────────────┬─────────────────────────┘
                                 ▼                       ▼
                     ┌──────────────────────────────────────────┐     ┌───────────────────┐
                     │ sigillumd  (_sigillum, no devices,        │◄───►│ sigillum-ui        │
                     │  no network, Capsicum, mlock)            │     │ (_sigillum-ui)     │
                     │  ceremonies, policy, client labels        │     │ sheets, PIN entry  │
                     └───┬────────────┬──────────────┬──────────┘     └─────────┬─────────┘
                         │            │              │                          │ frescod
              navigatord │      arcad │   ostiarius / │ apps (identity caps)     │ (press purpose
              (broker)   ▼            ▼   lock screen ▼                          ▼  IDENTITY, keys)
```

| process | user (proposed uid) | jail | holds | parses device data |
|---|---|---|---|---|
| `sigillumd` | `_sigillum` (65028) | own; no devices, no network; `allow.mlock`; Capsicum after start | client socket, the driver socket (one connection per driver), the UI socket, its links to arcad, ostiarius, `sigillum-ui`, the launch registry read-only | only the fixed records of §14.2 |
| `sigillum-fido` | `_sigillum-dev` (65029) | one jail per device, devfs ruleset 27 `atrium_identity` showing **only that device's node**; no network | one `hidraw` fd (or a BLE channel fd from vinculumd) | yes: CTAPHID, CBOR, COSE keys, authenticator data |
| `sigillum-ccid` | `_sigillum-dev` (65029) | as above, the reader's `ugen` nodes only | one reader | yes: CCID, ATR, APDUs, PIV/OpenPGP objects, X.509 certificates on the card |
| `sigillum-bio` | `_sigillum-dev` (65029) | as above | one sensor; for host-matching sensors, the templates Arca releases to it (§11.3) | yes: the sensor protocol, images on host-matching sensors |
| `sigillum-ui` | `_sigillum-ui` (65030) | own; no devices, no network | a frescod window, frescod's activation socket as consumer of purpose `IDENTITY` | input events only |

- **Why drivers are separate from the broker.** Everything a device says is attacker-chosen
  (a fake key is a $5 microcontroller). The driver jail is where that is parsed, and it holds
  one device and nothing else. The broker holds the policy and every client's trust; it reads
  only §14.2's records, each a fixed set of capped fields. It is the Vinculum split
  (bluetooth.md §4: `vinculum-hci` small and device-facing, `vinculumd` big) turned the other
  way round, because here the big parser is the device-facing one.
- **One uid for every driver jail, one jail per device.** Jails already stop one driver from
  signalling or tracing another; a uid per device adds a range to manage and buys little.
  Recorded as decision 4 (§17).
- **sysfil** (jail-sysfil.md): a new profile `device` = `base` plus `ioctl` limited to the
  node's own ioctls (`HIDIOCGRDESC`, `HIDIOCGRDESCSIZE`, `HIDIOCGRAWINFO`, `HIDIOCGRAWNAME`
  for hidraw; the `USB_*` transfer and interface ioctls for ugen), no `socket` except the
  one `connect` to the driver socket before Capsicum, no `open` after start (Capsicum). `sigillumd` and `sigillum-ui` take
  the `base` profile.
- **Who starts what.** `sigillumd` and `sigillum-ui` are services.d manifests started before
  the login screen (the lock screen and Vestibulum need them). Drivers are started per
  device by portcullisd's identity-device agent on the kernel's classification (§5.1),
  through jaild: jail `atrium-sigillum-fido-N` on ruleset 27, shown only that key's node,
  `u2f/M` when `u2f(4)` drives it and `hidrawN` otherwise (§2; jaild's `devfs_grant`,
  owned `_sigillum-dev`, 0600), with `/atrium/sockets/sigillum-dev`
  read-only as its one socket path. The driver connects to `sigillumd`'s driver socket
  (peer uid `_sigillum-dev` checked), then enters Capsicum. (Amended in S2: the first
  draft had portcullisd pass a socketpair, which jaild's exec path cannot carry.) A driver
  that exits loses its device's state; its ceremonies fail with `DEVICE_GONE` and the supervisor does not restart it until the device
  is replugged or re-enumerated.

## 5. Owning the devices

### 5.1 HID: keep FIDO keys away from Tactus

1. **Classify by the kernel's pnpinfo.** `atrium-devevents` gains a `Classified` event: for
   each `hidbusN`, the set of (page, usage) of its top-level collections, read from the
   devd `nomatch`/`attach` lines the kernel already formats. A `hidbus` with any collection
   on usage page `0xF1D0` (FIDO Alliance) is **identity**; everything else is **input**.
   `hidrawN` belongs to `hidbusN` (same unit). No userspace process parses a report
   descriptor to make this decision.
2. **Default hidden.** Ruleset 26 stops unhiding `hidraw*` wholesale. When a device is
   classified *input*, portcullisd unhides its node in tactusd's own devfs mount
   (`devfs -m ... rule apply`, no ruleset change), and re-checks every 2 s because a
   restarted tactusd gets a fresh mount. When it is *identity*, jaild mounts the driver
   jail's devfs on ruleset 27 and unhides that one node in it before the driver runs. A
   node that appeared but is not yet classified is visible to nobody, and a unit number
   reused by a different device starts hidden again. This fails closed: a missed event
   leaves a device unusable, never exposed. (As built in S2d; the first draft added
   per-node rules to the rulesets themselves, which a fast unplug and replug of a different
   device under the same unit could race.)
3. **Tactus refuses the class anyway.** tactusd's classifier (atrium-input.md §3.5) gains a
   row: usage page `0xF1D0` → `Identity`, never opened, never `Raw`. Belt and braces for
   a vanilla FreeBSD Tactus with a plain ruleset.
4. **Mixed devices.** A device with a FIDO collection and a keyboard collection on the same
   interface (rare; most keys put OTP on a separate interface) goes to Sigillum, and its
   keyboard part does not type. Logged, and the trusted UI says why.

This is a change to the input thread's ruleset and to `atrium-devevents`. It is S2's first
step and needs that thread's agreement (decision 5).

### 5.2 CCID readers and tokens

USB interface class `0x0B` (Smart Card). portcullisd unhides the device's `ugen` nodes in a
ruleset-27 mount and starts `sigillum-ccid`, exactly as Vinculum's ruleset 25 does for a
Bluetooth controller. Tokens that are a reader and a card in one (YubiKey CCID interface,
Nitrokey, most national eID sticks) look the same. A YubiKey therefore appears as two
Sigillum devices (FIDO over HID, PIV/OpenPGP over CCID) and one ignored keyboard interface
(OTP), each its own jail.

### 5.3 Fingerprint sensors

USB sensors are vendor-class devices (`0xFF`). They are matched by a table of (vendor,
product) that a `sigillum-bio` driver supports; an unknown sensor stays unclaimed. Sensors
behind laptop SPI/I²C controllers (Intel THC, some Goodix) need kernel work and are out of
scope until a machine needs them.

### 5.4 Bluetooth LE

FIDO over BLE is GATT service `0xFFFD`. Vinculum keeps it on the app blocklist and instead
hands a bonded authenticator's FIDO channel to `sigillumd` over a socket, like the HID
hand-off to Tactus (bluetooth.md §10): one SEQPACKET fd per device, framed as CTAP BLE
fragments. `sigillumd` passes the fd to a fresh `sigillum-fido` jail; the driver never sees
HCI. Pairing stays in Vinculum's trusted UI.

## 6. Drivers

Each driver is a small binary over a shared sans-I/O library, so every parser runs in host
tests and fuzzers with no device. All in Rust (LANGUAGE-POLICY D2), `unsafe` only for the
fd and ioctl layer.

### 6.1 `sigillum-fido`

- **Transport:** CTAPHID (64-byte reports, `INIT` with an 8-byte nonce, channel id,
  sequenced continuation packets, `KEEPALIVE`, `CANCEL`, `ERROR`) over hidraw; CTAP BLE
  framing over the Vinculum fd. Messages capped at 7609 bytes (the CTAPHID maximum).
- **Protocol:** CTAP 2.1 `authenticatorGetInfo`, `MakeCredential`, `GetAssertion`
  (`GetNextAssertion` for discoverable credentials), `ClientPIN` with PIN/UV auth protocol
  2 (protocol 1 only for keys that offer nothing else), `Reset` only from the trusted UI with
  the user's confirmation, `Selection`. CTAP1/U2F (`REGISTER`, `AUTHENTICATE`) for old keys,
  mapped onto the same records.
- **Parsing:** its own bounded CBOR decoder (depth ≤ 8, ≤ 7609 bytes, canonical integers,
  no tags, no indefinite lengths, no floats), not a general-purpose crate. Authenticator
  data and COSE keys are parsed and checked here, and summarised for `sigillumd` (§14.2).
- **What it never does:** talk to a second device, cache a PIN after the ceremony that
  needed it, or send `Reset` unless `sigillumd` passes the trusted UI's confirmation.

### 6.2 `sigillum-ccid`

- **Transport:** USB CCID bulk messages (`PC_to_RDR_*` / `RDR_to_PC_*`), T=1 and extended
  APDUs; card insertion and removal via the interrupt endpoint.
- **Applets, in order:** PIV (NIST SP 800-73-4: the YubiKey PIV applet, most corporate
  cards), OpenPGP card 3.4, then PKCS#15 profiles for national eID as needed. Each is a
  small state machine: select, read the certificate objects, `VERIFY` with the PIN,
  `GENERAL AUTHENTICATE` / `PSO:COMPUTE DIGITAL SIGNATURE` on a digest `sigillumd` provides.
- **Certificates** read from the card are parsed here (X.509, DER, capped at 8 KiB each)
  and summarised: subject, issuer, validity, key type, SHA-256 of the DER, key usage. The
  DER itself goes to `sigillumd` as an opaque blob only for the courier to send in the TLS
  `Certificate` message; `sigillumd` never parses it.
- **PIN:** sent once per operation (or once per card session, per decision 7), straight from
  `sigillum-ui` through `sigillumd`, in locked memory, zeroized after `VERIFY`.

### 6.3 `sigillum-bio`

- **Match on chip first.** Sensors that store templates and match inside the sensor
  (Synaptics Prometheus-class, Goodix MoC, Elan MoC) only report "match, finger k" or "no
  match". Of those, the ones that authenticate that report with a key set up at pairing
  (§11.2) are **secure sensors**; the rest are **plain**.
- **Host-matching sensors** send images; the driver does the matching. These are the
  weakest class (§11.1) and are supported only if decision 8 says so.
- **What leaves the jail:** a match result (finger index, quality) and, for enrolment,
  progress. Never an image, never a template in the clear.

## 7. `sigillumd` and `sigillum-ui`

### 7.1 Clients and labels

Like Arca (arca.md §4.1), `sigillumd` takes the peer uid with `getpeereid` on each accepted
connection and resolves it:

| peer | how it is known | what it may ask |
|---|---|---|
| the web broker | an app label (publisher key, app id) equal to `SIGILLUM_WEB_BROKER`, the same setting arcad has as `ARCA_WEB_BROKER` | ceremonies for **any** RP ID, because it derives the RP ID from the courier's verified endpoint (§9.1) |
| an app | launch registry label | ceremonies for its own RP IDs only (§13) |
| arcad | fixed uid `_arca` (65020) | `VERIFY_USER`; it is the platform authenticator for §9.2 |
| ostiarius | fixed uid (`_ostiarius`) | unlock and enrolment ops (§12) |
| `sigillum-ui` | fixed uid `_sigillum-ui` | answers to prompts it was shown |
| drivers | `_sigillum-dev`, on the driver socket only; first record `Hello`, exactly once | §14.2 records |

A uid with no label gets `UNKNOWN_PEER` and the connection is closed.

### 7.2 Ceremonies and the press

A ceremony is one request from one client: register, assert, sign, verify the user. For
each:

1. `sigillumd` checks the label against the request (RP ID allowed, the user's session
   unlocked where that is required).
2. It asks `sigillum-ui` to show a sheet. The sheet names **who asks** (from the label,
   never from the request) and **for what** (the RP ID the broker verified; the certificate's
   subject; Arca's own item description for verify-each-use). Request strings supplied by an
   app are shown only as quoted secondary text, never as the title.
3. **Nothing touches a device until the user presses** on the sheet. The press is a frescod
   activation token with a new purpose `IDENTITY`, consumer `_sigillum-ui`, refused as
   `OBSCURED` if the sheet was covered, moved or faded within 500 ms (the rules of Arca's
   fill chip, arca-autofill.md §4.1). The one exception is a fingerprint sheet for the lock
   screen, which Vestibulum itself drew.
4. Then the drivers are asked, the user touches the key or sensor or types a PIN, and
   `sigillumd` checks the result (§14.2) and answers.

- One ceremony per seat at a time; a second waits or gets `BUSY`.
- A ceremony ends at its limit, on the client closing its connection, or on Cancel from the
  sheet. Ending sends `CANCEL` to every driver involved. The limit is the site's own
  `timeout` (passed through as `timeout_ms`) kept within 30 s and 10 min, or 5 min when the
  site names none (WebAuthn's suggested default and ceiling); `VerifyUser` uses 5 min.
  (Amended 2026-11-13 from a fixed 120 s, after Shankar asked why sign-ins time out.)
- Three clocks can end a sign-in, and only this one is ours: the site's (above, honoured),
  the site's server, which accepts its one-time challenge only for a while (not ours to
  change), and the key's own touch wait (about 30 s on a CTAP2 key; a U2F key answers "not
  yet" until touched). The driver asks the key again when the key's wait runs out, while
  the ceremony lasts, so the key's clock never ends a ceremony; the sheet stays up. PIN
  entry happens before the request reaches the key, so it never counts against the key.
- `sigillumd` keeps no history of ceremonies beyond Arca-style audit lines (who asked, RP,
  device class, result, time); values are never logged.

### 7.3 Secure input

PINs, passcodes and the FIDO2 `clientPIN` are typed into `sigillum-ui`'s own window. frescod
routes keys for that window to `_sigillum-ui` only (the secure-input path frescod already
has for Arca's credential slot, F4f). `sigillum-ui` sends the PIN to `sigillumd` in locked
memory, which forwards it to the driver; the driver encrypts it under the device's PIN
protocol shared secret. The PIN is in three processes for the length of one operation and
in no app.

## 8. User verification (`VERIFY_USER`)

The check Arca's verify-each-use class needs, and the step-up any system UI can ask for.

- **Request:** (user, purpose) where the purpose is a fixed enum plus, from arcad only, the
  item's own description from the store. Apps cannot call it; an app wanting step-up uses
  WebAuthn with UV for its own RP (§13).
- **Methods, in the order the sheet offers them:** a secure fingerprint sensor (§11), a
  security key with built-in UV (on-key fingerprint, or its PIN), the user's passcode
  (checked by portcullisd's `VerifyCredential` through ostiarius, ostiarius-privsep.md §2.2,
  never by Sigillum itself).
- **Reply:** `Verified { method, strength, at }` on the caller's own connection. arcad
  trusts it because of the peer uid; there is no bearer token to replay. Strength is
  `passcode` > `key-uv` > `biometric-secure` > `biometric-plain`, and an Arca item may name
  a minimum (decision 6).
- **Rate:** five failed biometric matches in a row disable biometrics until the passcode is
  entered; a burst of verify requests is an anomaly reported to the monitor (arca.md §10).

## 9. WebAuthn for the Navigator

### 9.1 Who checks what

| step | done by | why there |
|---|---|---|
| origin → RP ID allowed (RP ID is the origin's host or a registrable suffix of it, not a public suffix) | broker | it knows the origin from the courier's TLS endpoint; workers and pages never state it |
| `clientDataJSON` (type, challenge, origin, `crossOrigin`, `topOrigin`) and its hash | broker | trusted values only (arca.md §8.4) |
| which authenticators to offer; the sheet; the press | `sigillumd`, `sigillum-ui` | §7.2 |
| `rpIdHash` in the returned authenticator data = SHA-256(RP ID); UP set; UV set if required; the credential's public key well formed | driver, then **again** in `sigillumd` | the driver is the parser; the broker-side check means a compromised driver can't swap RPs |
| the signature | the authenticator (key or Arca) | — |
| packaging the `PublicKeyCredential` for the page | broker | it builds the JS-visible object from `sigillumd`'s reply |

`sigillumd` receives (RP ID, `clientDataHash`, user handle and names for `create`,
`allowCredentials`, requested UV, resident key, attestation preference, extensions) and
returns (credential id, authenticator data, signature or attestation object, transports,
the authenticator's class). It never sees the challenge's meaning or the origin's page.

### 9.2 Platform passkeys stay in Arca

Arca is the platform authenticator (arca.md §8.4). Sigillum adds the ceremony around it:

- New arcad ops, callable only by `_sigillum`: `P_CREATE (user, persona, rp_id, user_handle,
  display names, cred_protect)` → (credential id, COSE public key); `P_ASSERT (user, rp_id,
  credential id, authenticator-data flags, clientDataHash)` → (authenticator data,
  signature); `P_LIST (user, rp_id)` → credential ids and display names, for the sheet only.
  Arca checks `rp_id` against the item's recipient, as it does for every key item.
- UV for a platform passkey is `VERIFY_USER` (§8) done by `sigillumd` just before `P_ASSERT`,
  so Arca's verify-each-use class is satisfied by the same mechanism.
- The sheet offers the user's Arca passkeys for this RP first, then "Use a security key",
  then (later) "Use a phone". Conditional mediation (`mediation: "conditional"`, passkeys in
  autofill) is drawn by Arca's fill chip (arca-autofill.md), which then calls Sigillum with
  the chosen credential.

### 9.3 Privacy defaults

- **Attestation:** `none` by default. The driver replaces the attestation statement with
  `fmt: "none"` and zeroes the AAGUID before anything leaves it. `direct` or `enterprise`
  only when the user agrees on the sheet ("example.com wants to know the make and model of
  your key"), per RP, remembered in Arca.
- **No enumeration.** Pages learn nothing about which keys exist. `allowCredentials` probing
  (asking with an id to see whether it errors fast) is answered by the sheet, not the device,
  until the user presses; a key without the credential then gets the standard "this key
  isn't registered here" on the sheet, and the page gets `NotAllowedError` after the
  ceremony ends, never sooner.
- **Extensions** supported: `credProtect`, `credProps`, `hmac-secret` and `prf` (for apps
  and sites that derive keys), `minPinLength` (enterprise). Not `largeBlob` (a store on the
  key the page can read) until someone needs it.

### 9.4 Errors

The page sees only WebAuthn's own errors: `NotAllowedError` for anything the user or policy
refused or that timed out, `InvalidStateError` for "already registered" on `create`, and
`SecurityError` for an RP ID the broker rejected. The detail goes to the sheet and the audit
line.

## 10. Certificates on smartcards and tokens

### 10.1 TLS client authentication in the Navigator

1. The courier's TLS handshake gets a `CertificateRequest` (signature algorithms, CA names).
   It stops and reports it to the broker (it has no keys).
2. If Arca holds a **binding** for this endpoint (an item of a new kind `TokenKeyRef`,
   recipient = the endpoint, value = card serial, applet, slot, SHA-256 of the certificate),
   arcad asks `sigillumd` to sign; otherwise the broker asks `sigillumd` for a chooser.
3. The chooser sheet lists certificates from attached cards that match the CA names (plus
   software certificates Arca holds, arca.md §8.5), with subject, issuer and expiry. The
   user picks one; Arca stores the binding ("use this certificate for
   `https://portal.example`"), so the next visit asks only for the PIN, or for nothing if
   decision 7 allows a card session.
4. Signing: the courier sends Arca the TLS 1.3 `CertificateVerify` content's digest; arcad
   forwards it to `sigillumd` with the binding; `sigillumd` asks `sigillum-ccid`, which asks
   for the PIN (through the sheet) and has the card sign. The signature goes back the same
   way. The courier never sees a PIN, the card's other objects or another certificate.
5. Algorithms: ECDSA P-256 and P-384, RSA-PSS 2048–4096 (RSA PKCS#1 v1.5 only for TLS 1.2
   servers). The card's key never leaves the card; there is nothing to export.

This keeps Arca's one rule ("only the intended recipient", arca.md §3): the binding names
the endpoint, and arcad releases the card's *effect* only to that endpoint's courier.

### 10.2 Other signing

- **Apps** with `identity.certificate` (§13) get "sign this digest with a certificate the
  user picks" for document signing (PDF, email S/MIME), with a chooser and a PIN every time.
- **SSH** with a PIV key later goes through Arca's SSH agent (A5) to Sigillum.
- **Disk or login with a smartcard** (PIV-based login, as enterprises do) is out of scope
  until someone asks; §12 covers the FIDO2 second factor instead.

## 11. Fingerprints

### 11.1 Classes

| class | where matching happens | what a compromised driver can do | allowed for |
|---|---|---|---|
| **secure** | on the sensor; result authenticated by a key shared at pairing | nothing beyond withholding a result | lock screen, verify-each-use (when the item allows biometrics), UV for platform passkeys |
| **plain on-chip** | on the sensor; result unauthenticated | claim a match | lock screen only, by default |
| **host matching** | in `sigillum-bio` | claim a match; leak templates it holds | lock screen only, and only if decision 8 allows them at all |

### 11.2 Secure sensors

Sensors with a pairing protocol (the Synaptics and Goodix MoC families do this with a
TLS-like session keyed at first pairing) are paired once by `sigillum-bio`; the pairing key
is an Arca **machine store** item (arca.md §6.4, a new kind `SensorKey`, recipient
`sigillumd`), released to `sigillumd`, which verifies each match report's MAC itself. The
driver relays the sensor's bytes and could drop them, but cannot forge one.

### 11.3 Templates

On-chip sensors keep templates on the sensor; `sigillumd` keeps only (user, finger index,
sensor id) to know whose finger it is. For host-matching sensors, templates are per-user
Arca items (kind `Template`, recipient `sigillum-bio`, class after-first-unlock so the lock
screen works), released to the driver jail at lock time and zeroized when the session ends.

### 11.4 Policy

A fingerprint works only when **all** hold (Apple's rules, which have held up):

- the user's Arca store is unlocked or screen-locked (keys in memory), never at boot or
  after sign-out: the root key needs the passcode (arca.md §6.1);
- less than 48 hours since the last unlock of any kind;
- not both "the passcode hasn't been entered for 6.5 days" and "no fingerprint unlock in the
  last 4 hours";
- fewer than 5 failed matches in a row;
- the sensor is enrolled for that user, and the enrolment was made with the passcode.

Enrolling, removing fingers and changing these limits all need the passcode. Fingerprint
data never goes to any app; there is no app API for biometrics, only `VERIFY_USER` results
through WebAuthn UV (§13).

## 12. Vestibulum and the lock screen

- **Unlock the lock screen** with a fingerprint (§11.4) or a security key: ostiarius asks
  `sigillumd`, which runs a `VERIFY_USER` ceremony on the lock screen's own sheet, then
  ostiarius unlocks as for the passcode. Nothing changes for login at boot: passcode.
- **FIDO2 second factor for the root key** (arca.md §6.1, "optional FIDO2 key"): at setup the
  user registers a key with `hmac-secret`; ostiarius asks `sigillumd` for `hmac-secret(salt)`
  at login and mixes the 32 bytes into the Argon2id output (HKDF, both required). Losing the
  key needs the recovery key. Sigillum provides the call; ostiarius owns the derivation.
- **Admin step-up** (Curia, package install with system scope) uses `VERIFY_USER` with a
  minimum strength of `key-uv` or `passcode`.

## 13. Apps

Two new Portcullis capabilities, both consent-at-install:

```toml
[capabilities]
identity.webauthn    = ["example.com", "login.example.com"]   # RP IDs this app may use
identity.certificate = true    # "sign with a certificate the user picks"
```

- **RP IDs** must be the app publisher's verified domains: at install, Opifex fetches
  `https://<rp>/.well-known/atrium-apps` through Nuntius and checks it names the app's
  publisher key and app id (like Apple's associated domains and Android's asset links).
  Without that, an app gets only its private RP `app:<publisher>/<app id>`, which no website
  shares, so it can't phish a web account.
- Apps never get a device list, a PIN, a raw CTAP or APDU channel, or biometrics. Tactus
  never classifies FIDO collections as `Raw` (§5.1), so `input.raw_hid` doesn't reach them
  either.
- Insula's per-service keypairs (insula.md §13.3) stay in Arca, unchanged.

## 14. Wire

### 14.1 Clients ↔ `sigillumd`

`/atrium/sockets/sigillum/sigillumd.sock`, `u32 LE length | postcard`, at most 16 KiB per
message (a `create` with long user names is under 2 KiB; an attestation object under 4 KiB).
Requests carry a client-chosen ceremony id so one connection can cancel its own ceremony.

| request | from | reply |
|---|---|---|
| `Register { rp_id, client_data_hash, user, exclude, resident_key, uv, attestation, ext }` | broker, app | `Registered { cred_id, auth_data, att_obj, transports, authenticator }` |
| `Assert { rp_id, client_data_hash, allow, uv, ext }` | broker, app | `Asserted { cred_id, auth_data, signature, user_handle }` |
| `ChooseCertificate { endpoint or app, ca_names, sig_algs }` | broker, app | `Chosen { binding }` |
| `SignWithBinding { binding, digest, alg }` | arcad, app | `Signed { signature }` |
| `VerifyUser { user, purpose, min_strength }` | arcad, ostiarius, system UI | `Verified { method, strength, at }` |
| `HmacSecret { user, salt }` | ostiarius | `Secret { 32 bytes }` |
| `Cancel { ceremony }` | any | — |

Every refusal is one of `Denied`, `NotAllowed` (user or timeout), `UnknownPeer`, `Busy`,
`Malformed`, `Locked`.

### 14.2 Drivers ↔ `sigillumd`

Over the driver connection, `u32 LE length | postcard`, at most 8 KiB. The driver's records are
the only device-derived data `sigillumd` reads, and every field is capped:

- `Hello { class, vendor, product, name (≤ 64 bytes), caps }` — caps for FIDO from
  `GetInfo` (versions, options `rk`/`uv`/`clientPin`/`up`, PIN protocols, extensions,
  `maxMsgSize`), already parsed.
- `Registered { cred_id (≤ 1023), rp_id_hash, flags, sign_count, cose_alg, public_key
  (point, ≤ 133 bytes), auth_data (≤ 4 KiB, opaque), att_obj (≤ 4 KiB, opaque) }`
- `Asserted { cred_id, rp_id_hash, flags, sign_count, auth_data (opaque), signature (≤ 512),
  user_handle (≤ 64) }`
- `Certificates { list of (sha256, subject, issuer, not_before, not_after, key, usage, der
  (≤ 8 KiB, opaque)) }`, `Signed { signature }`
- `Match { finger, quality, mac (secure sensors) }`, `NoMatch`, `EnrolProgress { n, of }`
- `NeedPin { retries }`, `NeedTouch`, `Error { code }`

`sigillumd` re-checks the fixed fields against the opaque blobs it forwards where that is a
byte comparison (the `rp_id_hash` and flags are the first 33 bytes of `auth_data`), and
otherwise passes the blobs on without reading them.

## 15. Tests

- **A soft authenticator** (`sigillum-softkey`): a CTAP 2.1 authenticator over CTAPHID in
  memory, P-256, with PIN protocol 2, discoverable credentials and a sign counter. It is the
  device in every host test; the same code can later sit behind QEMU's
  `u2f-passthru`-style plumbing for VM tests.
- **Host tests** of every parser (CBOR, CTAPHID, authenticator data, COSE, CCID, APDU, PIV
  objects, X.509 summary) against recorded traces from real keys, plus fuzz targets (one per
  parser, as Vinculum has).
- **Policy tests** of `sigillumd` as a pure function from (label, request, state) to a reply,
  like arcad's `server.rs`: a non-broker app asking for another RP, a driver record whose
  `rp_id_hash` differs from the request, a fingerprint after 48 h, a ceremony with no press.
- **VM gates** (S2, S5): QEMU `usb-ccid` with an emulated PIV card; a FIDO HID device in the
  guest; devevents classification; tactusd unable to open the FIDO node; the driver jail's
  devfs showing one node; sysfil `device` profile in deny mode.
- **Hardware** (when Shankar has them on the Mac or the R9700 box): a YubiKey 5 (FIDO2 +
  PIV + OpenPGP), a SoloKey or Nitrokey 3 (open firmware), a USB fingerprint sensor of a
  secure class.

## 16. Build plan

| phase | work | gate |
|---|---|---|
| **S0** | this spec | Shankar's decisions (§17) |
| **S1** *(built 2026-11-13, §16.1)* | `sigillum/` workspace: `sigillum-cbor` (bounded CTAP2 canonical CBOR), `sigillum-ctap` (CTAPHID framing, CTAP2 GetInfo/MakeCredential/GetAssertion/ClientPIN v2 client, authenticator-data and COSE parsing), `sigillum-softkey`, `sigillum-proto` (§14 wires), `sigillumd` policy core (labels, ceremonies, press gate, driver record checks, fingerprint policy) | host tests: a broker registers and asserts against the soft key with PIN; an app is refused another RP; a lying driver record is refused; fingerprint policy limits |
| **S2** | `sigillum-fido` on hidraw; devevents classification; default-hidden hidraw (§5.1) with the input thread; ruleset 27; services.d manifests; `sigillum-ui` sheets on frescod with purpose `IDENTITY` | VM: a FIDO HID device reaches only its driver jail; tactusd gets `EACCES`/no node |
| **S3** | the Navigator broker's WebAuthn client against Sigillum; Arca `P_CREATE`/`P_ASSERT`/`P_LIST` (with the Arca thread's A4) | e2e: webauthn.io-style local RP registers and logs in with the soft key and with an Arca passkey |
| **S4** | `VERIFY_USER` with the passcode through ostiarius; Arca verify-each-use served | arca-vm-test: a verify-each-use item is usable after the sheet, not before |
| **S5** | `sigillum-ccid`: CCID, PIV, certificates; `TokenKeyRef` in Arca; courier `CertificateVerify` through Arca | VM: QEMU `usb-ccid` emulated card authenticates to a TLS server requiring a client certificate |
| **S6** | `sigillum-bio` for one secure sensor; enrolment; lock screen unlock | needs the sensor |
| **S7** | FIDO over BLE through Vinculum | with Vinculum's B-phases and a BLE key |
| **S8** | hybrid (phone as authenticator): QR, BLE advert via Vinculum, tunnel through a dedicated Nuntius-style jail allowed only the tunnel servers | needs a phone and decision 9 |
| **S9** | NFC keys and cards through CCID contactless readers | needs a reader |

### 16.1 As built: S1 (2026-11-13)

- **Where:** `sigillum/`, its own workspace (README there). 32 host tests, `cd sigillum &&
  cargo test`.
- **`sigillum-cbor`:** strict decoder (one item, depth ≤ 8, ≤ 2048 items, ≤ 7609 bytes,
  shortest integers, no tags, floats, indefinite lengths or duplicate keys; key order not
  enforced on input) and a canonical encoder. RFC 8949 Appendix A vectors pass.
- **`sigillum-ctap`:** CTAPHID framing and reassembly (sequence, channel, keepalive,
  error, the 7609-byte cap); CTAP 2.1 requests and responses with caps; authenticator data
  and COSE keys (P-256 points checked on the curve); PIN/UV auth protocol 2, with HKDF and
  HMAC checked against values computed independently in Python. A random-bytes and
  mutation run over every parser (`tests/hostile.rs`) stands in for fuzz targets until S2.
- **`sigillum-softkey`:** an in-memory CTAP 2.1 authenticator over CTAPHID (P-256, packed
  self-attestation, PIN protocol 2, resident keys, sign counter) with knobs for "user does
  not touch" and three lies (wrong RP hash, no UP, a credential that wasn't allowed).
- **`sigillum-fido`:** the driver logic of §6.1: `GetInfo` to `Hello`, PIN-or-not, the
  checks of §9.1 on the driver side, attestation rewritten to `none` with a zero AAGUID.
- **`sigillumd`:** the broker as a pure state machine (`broker.rs`): labels (§7.1), one
  ceremony at a time, the press gate, PIN round trips, passcode verification through an
  `CheckPasscode` action, the byte comparisons of §14.2, timeouts, cancel, unplug; the
  fingerprint policy of §11.4 (`bio.rs`); RP ID syntax (`rp.rs`).
- **The gate** (`sigillumd/tests/ceremonies.rs`): the Navigator's broker registers with a
  PIN-protected soft key and signs in with UV, and a relying party verifies the attestation
  object and the ES256 signature; a key without a PIN works without UV and refuses required
  UV; a wrong PIN asks again and the key's retry counter recovers; nothing reaches the key
  without a valid press; an app can use only its own and its private RP; a lying key is
  caught by the driver and five kinds of lying driver record by the broker; busy, cancel
  (only by the client that started the ceremony) and unplug end ceremonies cleanly; only
  arcad and ostiarius may ask `VerifyUser`.
- **Not yet, by design (S2):** the daemon loop and sockets, the launch registry lookup,
  `sigillum-ui`, frescod's `IDENTITY` purpose, the hidraw binary and its jail, U2F-only
  keys, real-key traces. Everything so far has been tested only against our own soft key,
  so a spec misreading shared by both sides would not show; a real key in S2 is the check.

### 16.2 As built: S2 (2026-11-13)

- **S2a, HID classification** (`atrium-devevents/src/hidclass.rs`, commit `5fc598e`):
  parses devd's hidbus and hidraw lines and decides, per hidbus, whether its hidraw node is
  input, identity (any top-level collection on usage page `0xF1D0`) or unknown. `tactusd`
  now skips identity devices itself. Hiding hidraw by default in devfs is S2d.
- **S2b, the `sigillum-fido` binary** (`4601448`): opens one `/dev/hidrawN`, reads vendor,
  product and name with the hidraw ioctls, enters Capsicum, says `Hello`, serves. U2F-only
  keys work (register and sign through `CTAPHID_MSG`, mapped to WebAuthn shapes), which is
  what QEMU's `u2f-emulated` offers the VM gate. A broker `Cancel` reaches the key as
  `CTAPHID_CANCEL` within 100 ms.
- **S2c, the daemon** (`sigillumd/src/daemon.rs`, `resolve.rs`, `bin/sigillumd.rs`):
  - Three sockets: clients (0666; label from the peer uid at accept, through the launch
    registry, read fresh per connection), drivers (peer uid `_sigillum-dev`;
    `Hello` first and once, or dropped; at most 8), the trusted UI (peer uid
    `_sigillum-ui`, one at a time). Reader thread per connection, one thread owns the
    broker and every write; writes time out at 2 s and drop the peer; a malformed or
    oversized message drops the peer.
  - With no UI connected a ceremony ends at once with `NotAllowed`; when the UI leaves,
    every ceremony ends. The sheet lists only FIDO devices for WebAuthn sheets.
  - The UI wire (`sigillum-proto/src/ui.rs`): `Show`, `AskPin`, `Touch`, `AskPasscode`,
    `Close` to the UI; `Press` (with frescod's verdict on the press token), `Pin`,
    `Passcode` back.
  - The driver turns the key's `UPNEEDED` keepalive (or U2F's first "conditions not
    satisfied") into one `NeedTouch` per request, which becomes the sheet's "touch your
    key".
  - Resolver choices, all fail-closed: unsigned apps are unknown (a private RP ID is keyed
    by publisher, so all-zero would merge every unsigned app); a system uid also in the
    launch registry is unknown; session components are unknown; apps get **no web RP IDs
    yet**, only their private one, because Opifex does not verify `identity.webauthn`
    (§13) yet.
  - Passcode checks go through a `Passcode` trait; until S4 wires ostiarius every passcode
    is refused, so `VerifyUser` cannot succeed yet.
  - Hardening: no core dumps, not traceable (`PROC_TRACE_CTL`), before anything else.
  - Gate (`sigillumd/tests/daemon.rs`): the real daemon on real sockets, the driver's real
    serve loop over the soft key, a scripted UI and a client, in one process: register with
    PIN, then assert with UV; the UI sees the sheets, the PIN ask, the touch prompt and the
    closes. Refusals: no UI, UI leaving mid-ceremony, wrong uids on the driver and UI
    sockets, a driver that skips or repeats `Hello`, an oversized client message. 43 host
    tests in the workspace; the FreeBSD target type-checks.
- **S2d, devices and jails:**
  - `etc/atrium.devfs.rules`: ruleset 26 no longer unhides `hidraw*` (mode only); new
    ruleset 27 `atrium_identity` = basics only.
  - jaild: `CreateJailRequest.devfs_grant` (one node) and, on a pin, `name_prefix` and
    `grant_nodes` (`hidraw#`). A grant needs a pinned ruleset whose pattern matches; jaild
    applies `unhide`, owner = the pin's uid and mode 0600 to that jail's mount after
    mounting devfs and before exec, and fails the create if it can't. Ruleset 27 is pinned
    to `atrium-sigillum-fido-*`, `/usr/local/bin/atrium-sigillum-fido`, uid 65029.
  - `etc/jaild.policy.toml`: uids and gids 65028–65030, ruleset 27, the sysfil profile
    `device` (base plus `HIDIOCGRAWINFO` and `HIDIOCGRAWNAME`), and
    `/atrium/sockets/sigillum-dev` as a read-only mount source.
  - `atrium-devevents::hidowner`: devd lines and `devinfo -v` (devices present at start)
    to operations: show input, start or stop an identity driver, log. A unit that changes
    kind without a detach stops its driver first.
  - portcullisd `identity_devices.rs`: the agent thread carrying them out (devfs for
    tactusd, jaild `CreateJail`/`RemoveJail` and mount cleanup for drivers), the 2 s
    re-show, and `PORTCULLIS_IDENTITY_DEVICES=off`.
  - `sigillum/etc/services.d/37-atrium-sigillumd.toml` and `scripts/sigillum-service.sh`
    (users, socket directories 0755 / 0750 `_sigillum-dev` / 0750 `_sigillum-ui`, binaries,
    jail roots).
  - Tested on the host: the planner (4 tests) and the request shape; jaild's validator
    tests (a per-device pin grants one matching node; the shipped policy pins 27) and the
    portcullisd test type-check for FreeBSD but run only there (`sockmux` needs kqueue), so
    they run in the VM gate.
- **S2f, the trusted sheets** (`daab114`): frescod's activation purpose `IDENTITY` (8):
  a button press or Enter in a sheet window that was whole, uncovered and still for 500 ms,
  the same check as `FILL`; sheet windows are top-level windows of `FRESCOD_SHEET_UIDS`
  (default `_sigillum-ui`). `sigillum-ui` (own crate): one dialog window per sheet; a choice
  is spent with frescod before `sigillumd` hears of it; PIN and passcode bytes go into
  locked memory and are drawn as rects, never glyphs; Cancel and Escape need no token.
- **S2e, the VM gate** (`scripts/vm-sigillum-check.sh`, 2026-11-13: **0 failed**). amd64
  16-CURRENT VM, QEMU's `u2f-emulated` key (the atrium-os/qemu fork built against
  libu2f-emu; `scripts/build-qemu.sh`) and a USB keyboard; sigillumd and tactusd started
  jailed by portcullisd-bootstrap, the driver jail by portcullisd's agent. Checked: the key
  gets `atrium-sigillum-fido-1`, driven through `/dev/u2f/0`, whose `/dev` holds that node
  only (plus null, zero, random, urandom), owned `_sigillum-dev` 0600, driver running as
  `_sigillum-dev`, `ip4=disable`; tactusd's `/dev` has the keyboard's `hidraw0` and not the
  key; a client the launch registry names as the web broker registers for `example.com`
  (RP ID hash checked) and signs in (ES256 signature over authData and the client data
  hash verified), with the sheets shown to a scripted UI running as `_sigillum-ui`; a UI
  under another uid and an unregistered uid are refused; a second key hot-plugged through
  the QEMU monitor gets its own jail from the devd path and unplugging removes only that
  one. jaild (84), portcullisd (39) and frescod activation/credential (9) unit tests pass
  in the VM. What the gate found and fixed:
  - the agent ran in the multi-tenant `portcullisd`, which rc.d does not start; it now
    runs in `atrium-portcullisd-daemon`, the root service that is jaild's one client;
  - FreeBSD 16's `devinfo -v` prints `name <description> pnpinfo ...`: the parser skipped
    nothing and found no device at start;
  - `u2f(4)` (§2): drivers use `/dev/u2f/N` (bare 64-byte reports, name from
    `USB_GET_DEVICEINFO`, added to the `device` sysfil profile; `devfs_grant` takes
    `dir/name` and shows the directory entry too; the pin allows `u2f/#`);
  - the client's idle limit is 5 s, not 1 s (a U2F-only key sends no keepalives), and it
    drains reports left in the node before `INIT`;
  - jaild's env allow-list lacked `SIGILLUM_ARCA_UID`, `SIGILLUM_OSTIARIUS_UID`,
    `SIGILLUM_WEB_BROKER`;
  - a restarted agent first unmounts what a dead driver jail left;
  - two first nullfs mounts at once paniced the 16-CURRENT kernel (`getnewvnode: not
    registered vector op` in `null_nodeget`); `sigillum-service.sh` loads nullfs at boot;
  - QEMU's `u2f-emulated` lost wakeups and so the second packet of a request
    (`scripts/qemu-patches/u2f-emulated-lost-wakeup.patch`, applied by build-qemu.sh).
  Only an emulated U2F key so far: CTAP2 (PIN, UV, resident keys) on real hardware waits
  for a YubiKey on the Mac VM.
- **Not yet:** Capsicum in `sigillumd` itself (it reads the launch registry per connection; a
  pre-opened directory fd comes later), `allow.mlock` for `sigillumd`.

## 17. Decisions, with recommendations

**Accepted by Shankar, 2026-11-13: all eleven, as recommended.**

1. **One identity service, Sigillum, separate from Arca and Vestibulum.** *Recommend yes.*
   Arca is a store with no device and no parser of anything external (arca.md §6.3); putting
   USB parsing next to every secret would break that. Vestibulum draws the login screen;
   folding devices into it makes the login daemon the biggest parser on the machine.
2. **A driver jail per device, the broker parses only fixed records** (§4, §14.2).
   *Recommend yes.* Cost: one small process per attached key or reader.
3. **Arca stays the platform authenticator; Sigillum runs the ceremony** (§9.2). *Recommend
   yes*, which matches arca.md §8.4 and needs three small arcad ops from the Arca thread.
4. **One uid `_sigillum-dev` for every driver jail.** *Recommend yes* (§4); jails already
   separate them.
5. **hidraw nodes hidden by default, unhidden per node after the kernel's pnpinfo
   classifies the device** (§5.1), which changes Tactus's ruleset 26. *Recommend yes*; today
   tactusd can open every FIDO key. Needs the input thread.
6. **Verify-each-use items can require a minimum strength**, default `biometric-secure`
   (a passcode, key UV or secure sensor satisfies it). *Recommend yes.*
7. **Smartcard PIN: once per signature by default**, with an opt-in "keep this card
   unlocked for 5 minutes while the session is unlocked". *Recommend yes*; TLS client auth
   on a corporate portal would otherwise prompt on every new connection, so the opt-in
   matters.
8. **Host-matching fingerprint sensors: lock screen only, or not at all.** *Recommend lock
   screen only*, off by default, with the setup screen saying they are weaker.
9. **Hybrid (phone) needs a network jail** that reaches only the FIDO tunnel servers
   (`cable.ua5v.com`, `cable.auth.com`). *Recommend deferring* to S8; it is the only part
   with network.
10. **Attestation `none` by default; `direct` only with the user's per-RP yes.** *Recommend
    yes* (§9.3).
11. **The name Sigillum** (`sigillumd`, `sigillum-fido`, `sigillum-ccid`, `sigillum-bio`,
    `sigillum-ui`). If agreed, it goes into `docs/NAMING.md`.

## 18. Open questions

- **TPM as an authenticator.** A TPM-held passkey (device-bound, non-exportable) is
  Arca's business (arca.md §6.1, A4); Sigillum needs nothing new for it.
- **Which secure fingerprint sensor to buy.** Laptop sensors are mostly SPI or I²C; USB
  ones with match-on-chip and an authenticated channel are a short list. S6 waits for a
  choice.
- **Enterprise attestation and `minPinLength`** need a policy owner (Curia) that doesn't
  exist yet.
- **PIV management** (generating keys on a card, importing certificates) is a tool's job,
  not the service's; whether that tool exists in v1 is open.

## References

- FIDO Alliance, Client to Authenticator Protocol (CTAP) 2.1 and 2.2; W3C Web
  Authentication Level 3.
- USB CCID 1.1; ISO/IEC 7816-4; NIST SP 800-73-4 (PIV); OpenPGP card 3.4.
- FreeBSD `sys/dev/hid/hidraw.c`, `sys/dev/hid/hidbus.c`, `lib/libfido2` (read 2026-11-13).
- [arca.md](arca.md), [arca-autofill.md](arca-autofill.md), [vestibulum.md](vestibulum.md),
  [ostiarius-privsep.md](ostiarius-privsep.md), [atrium-input.md](atrium-input.md),
  [bluetooth.md](bluetooth.md), [atrium-navigator-threat-model.md](atrium-navigator-threat-model.md)
  §6.4, [jail-sysfil.md](jail-sysfil.md), [limen.md](limen.md), [portcullis.md](portcullis.md).
