# Device discovery, driver matching and driver start

**Status:** accepted 2026-11-13 (Shankar: all §8 decisions as recommended, the agent a separate daemon, Dispensator).
**Scope:** how Atrium finds hardware, decides who drives each device (a kernel
module or a jailed user-space driver), starts and stops that driver, and loads
firmware. It gathers what Sigillum, Tactus, Vinculum, Lyra and the GPU driver
each solved on their own into one mechanism.

## 1. Short answer

FreeBSD's kernel side is good and we keep it. newbus probe/attach, the
`MODULE_PNP_INFO` tables, `kldxref`'s `linker.hints`, `devmatch(8)` and the
devctl event stream do the "find the hardware and load the kernel module that
matches it" job well, on PCI, USB, ACPI and FDT alike.

What FreeBSD does not have is the layer above it: a policy that says "this
device goes to a user-space driver in its own jail, with only its own node,
no network, and this firmware". Today `devd.conf` rules, static devfs
rulesets and `rc.conf` blocklists stand in for that. Atrium has already grown
three private versions of this layer (Sigillum's identity-device agent,
Tactus's ruleset 26, Vinculum's ruleset 25 plan). This proposal turns them
into one: a small privileged agent with a declarative match table, fed by
typed device events, that starts device drivers through jaild.

## 2. What Atrium has today

| Piece | Where | What it does | Gap |
|---|---|---|---|
| `atrium-devevents` | crate | reads devd's seqpacket socket; `Event::{Added, Removed, Other}` by devnode; `hidclass` classifies a hidbus by its collections' usage pages from pnpinfo; `hidowner` turns devd lines + `devinfo -v` into operations | events are devnode strings, not typed (bus, pnpinfo, location); HID-only classification |
| portcullisd `identity_devices.rs` | portcullisd | unhides input hidraw nodes in tactusd's own devfs mount (re-checked every 2 s); starts and stops one `sigillum-fido` jail per FIDO key via jaild `devfs_grant` | hard-coded to HID; lives inside the app-launch broker |
| jaild `devfs_grant` + pinned rulesets | `etc/jaild.policy.toml` | ruleset pinned to a jail name/exec/uid; a grant unhides one node, owner = pin uid, mode 0600, before exec; fails the create if it can't | the right primitive; reuse it everywhere |
| devfs rulesets 20–27 | `etc/atrium.devfs.rules` | per-service static rulesets; 24 (Lyra) unhides `dsp*`/`mixer*` wholesale; 26 (Tactus) now mode-only for `hidraw*` | wildcards hand every matching device to one service |
| sysfil profile `device` | `jaild.policy.toml` | base calls + the device's own ioctls | per-driver profiles needed (USB transfer ioctls for ugen, etc.) |
| GPU kernel driver | `atrium-gpu-amd/pci.c` | `MODULE_PNP_INFO("U16:vendor")` for `0x1002`; probe returns `BUS_PROBE_DEFAULT` for AMD display class; per-architecture identify from IP discovery (`identify.c`) | binds only when preloaded (beats `vgapci`'s generic probe) or by `devctl set driver -f`; children modules travel with the base by preload; R9700 image keeps the modules off the module path on purpose |
| Vinculum (spec) | `bluetooth.md` §2.3 | `vinculum-hci` per controller on ruleset 25, `devmatch_blocklist="ng_ubt"`, firmware files read-only in the jail | discovery path was "a devd rule or portcullisd"; not built |
| Sigillum (built S2a–d) | `sigillum.md` §5 | FIDO by usage page `0xF1D0`, CCID by interface class `0x0B`, sensors by VID:PID table; one jail per device; fail closed | the general shape this proposal adopts |
| Lyra | `atrium-lyra-architecture.md` | uses the kernel's `snd_hda`/`snd_uaudio` via OSS `dsp*` | hotplugged headsets need per-node grants, not `dsp*` |
| Boot path | `loader.conf` in the image scripts | preloads `tessera_fs`, `nullfs`, `p9fs`, ZFS; `kld_list+=hidraw` | fine; no policy needed |

## 3. FreeBSD's autoprobing, assessed

### 3.1 How it works

1. **newbus.** Buses (nexus → acpi/ofwbus → pcib → pci → …, uhub → USB
   interfaces, hidbus → collections) enumerate their children. Each driver
   registered on that bus class (`DRIVER_MODULE`) gets `DEVICE_PROBE`; it
   returns a priority (`BUS_PROBE_SPECIFIC` > `VENDOR` > `DEFAULT` > `LOW_PRIORITY`
   > `GENERIC` > `HOOVER`), the best wins and attaches. Unclaimed children stay
   attached to no driver.
2. **Late drivers.** When a module is loaded, `devclass_driver_added` re-probes
   every *unattached* child of the buses that driver sits on. An attached device
   is never re-probed: a better driver loaded later does not displace
   `vgapci`, `ukbd` or `ng_ubt`. `devctl detach` / `devctl set driver -f` do
   that by hand.
3. **devctl events.** The kernel formats `+name at location pnpinfo on parent`
   (attach), `-name …` (detach), `? at … pnpinfo … on parent` (nomatch) and
   `!system=… subsystem=… type=…` (notify) into `/dev/devctl`. One reader,
   `devd(8)`, matches them against `devd.conf` rules and republishes them on
   `/var/run/devd.seqpacket.pipe`.
4. **devmatch.** Modules carry `MODULE_PNP_INFO` tables (field formats like
   `U16:vendor;U16:device;V32:class`); `kldxref` copies them into
   `linker.hints`. `devmatch(8)` walks the device tree (or takes a nomatch
   event), matches unattached devices against the hints and `kldload`s what
   fits. rc.d runs it at boot, devd runs it on nomatch. `devmatch_blocklist`
   in `rc.conf` keeps named modules from being auto-loaded.
5. **Firmware.** `firmware(9)`: `firmware_get` finds an image registered by a
   firmware `.ko`, or, since 13.x, a raw file under `/boot/firmware/`
   (atrium-gpu-amd's PSP path uses the latter). `fwget(8)` (14.0) maps PCI IDs
   to firmware *packages* and installs them from the network.
6. **Buses.** PCI (incl. PCIe hotplug), USB (xhci/ehci/ohci, `uhub`, per-interface
   drivers, `ugen` always present per device, `usb_quirk`), HID (`hidbus`,
   `hidraw`, `hkbd`/`hms`/`hgame`, `usbhid`, HID over I²C via `iichid` on
   laptops), ACPI (ACPICA, `_HID`/`_CID` matching, `acpi_ids`), FDT/OFW
   (`compatible` strings, `ofw_bus_search_compatible`) on arm64 boards, ACPI on
   arm64 servers.

### 3.2 Verdict

| Part | Quality | Notes for Atrium |
|---|---|---|
| newbus probe/attach | **strong**, 25 years old, the same on every bus | keep; our kmods use it as is |
| PNP_INFO + linker.hints + devmatch | **good**; coverage gaps are per driver (drivers with no table or with logic beyond IDs) | keep `devmatch_enable`; every Atrium kmod ships a table |
| Late-load binding | **weak by design**: only unattached devices, no "better driver takes over" | Atrium needs an explicit *claim* step for the GPU and for devices we want in user space (§4.3) |
| devctl/devd | **adequate**: complete events, but stringly typed, one reader, a root C++ daemon whose config is shell commands | keep devd as the reader for now; never put policy in `devd.conf` (§4.2) |
| Device identity | **weak**: unit numbers are reused (`hidraw0` after an unplug can be a different device); stable names exist only for disks (GEOM labels, `diskid`) | key devices by location + serial + a generation (§4.2) |
| User-space drivers | **partial**: excellent for USB (`ugen` + libusb), HID (`hidraw`), serial (`uart`/`cuaU`); **none for PCI** (no VFIO-style IOMMU-backed user DMA/IRQ outside bhyve's `ppt`) | PCI devices stay kernel drivers (Rust per D2); USB/HID/BT/CCID/sensors go to jails |
| Firmware | **good** (`firmware(9)` raw files) | ship firmware in signed Atrium packages; no `fwget`, no network on any device path |
| Hardware coverage | the real weakness (Wi-Fi, laptop sensors, new GPUs), not the machinery | out of scope here; per-driver threads |

## 4. Design

### 4.1 Two kinds of driver, one rule for which

- **Kernel driver** when the device needs DMA, interrupts or MMIO: PCI(e)
  devices (GPU, NVMe, NIC, HDA audio, carillon's ivshmem). FreeBSD has no
  safe way to give those to a process. New ones are Rust kmods (D2) with C
  newbus glue; each ships a `MODULE_PNP_INFO` table.
- **Jailed user-space driver** when a generic kernel node already exposes the
  device safely: USB through `ugen`, HID through `hidraw`, serial through
  `cuaU`/`ttyU`, and fds handed over by another service (Vinculum's BLE/Classic
  channels). One jail per device, own uid, devfs showing only that device's
  nodes, no network, a per-driver sysfil profile, Capsicum after open,
  firmware read-only. This is Sigillum's §4 model and Vinculum's
  `vinculum-hci`, made general.
- **Kernel class drivers that services consume** stay as they are and are
  handed out per node: `snd_*` → `dsp*` → lyrad; `hkbd`/`hms` or `hidraw` →
  tactusd; `atrium-gpu0` → frescod.

### 4.2 The pipeline

```
 kernel newbus ──/dev/devctl──► devd ──seqpacket──► atrium-devevents (typed, fuzzed)
       ▲                         │                          │
       │ kldload (devmatch)      └─ nomatch → devmatch       ▼
       │                                         device agent (root, small)
       │                                           │  match table (TOML, signed)
       └──── devctl detach / set driver ◄──────────┤  stable device ids
                                                   ├─► jaild CreateJail{devfs_grant…}  → jailed driver
                                                   ├─► devfs rule apply on a service's own mount (tactusd, lyrad)
                                                   └─► event to the owner service / Curia "Devices" list
```

1. **Kernel stays FreeBSD.** Boot-path modules preloaded in `loader.conf`
   (storage, Tessera, nullfs, console); everything else by `devmatch`.
2. **devd stays the single reader of `/dev/devctl`** with stock `devd.conf`
   plus `devmatch`. No Atrium policy in `devd.conf`: rules there run shell as
   root and cannot express "one jail per device".
3. **`atrium-devevents` becomes typed.** `Attach{name, unit, bus, location,
   pnpinfo: map, parent}`, `Detach{…}`, `Nomatch{bus, location, pnpinfo,
   parent}`, `Notify{system, subsystem, type, data}`, plus a startup snapshot
   from the devinfo sysctls (`dev.<name>.<unit>.%pnpinfo/%location/%parent`)
   so a restart sees devices already present. Bounded, fuzzed parser; it
   already does this for HID.
4. **Stable device id** = (bus path from `%location`, plus USB serial or PCI
   BDF, plus a generation bumped on every detach). A reused unit number is a
   new device and starts hidden (Sigillum §5.1's rule, generalised).
5. **The device agent** reads a match table and, per device, picks one owner:

   ```toml
   # /usr/local/etc/atrium/devices.d/40-sigillum.toml (shipped by the package)
   [[match]]
   name    = "fido-hid"
   bus     = "hidbus"
   usage_page = 0xF1D0                  # any top-level collection
   owner   = { driver = "sigillum-fido", jail_prefix = "atrium-sigillum-fido-",
               uid = "_sigillum-dev", ruleset = 27, sysfil = "device-hidraw",
               nodes = ["hidraw{unit}"], firmware = "" }
   priority = 100

   [[match]]
   name    = "bt-controller"
   bus     = "usb"
   interface = { class = 0xE0, subclass = 0x01, protocol = 0x01 }
   claim   = { detach = ["ng_ubt"] }    # take it back from a kernel driver
   owner   = { driver = "vinculum-hci", uid = "_vinculum-hci", ruleset = 25,
               sysfil = "device-ugen", nodes = ["ugen{bus}.{addr}", "usb/{bus}.{addr}.*"],
               firmware = "vinculum" }
   ```

   Owners are either a **driver jail** (started through jaild with
   `devfs_grant` for exactly those nodes, stopped on detach, restarted with
   backoff on crash) or a **service** (node unhidden in that service's own
   devfs mount, the service told over its socket). No match = nobody sees the
   device; it is logged and listed in Curia's device list. Fail closed: a
   missed event leaves a device unusable, never exposed.
6. **Packages declare matches**, like `MODULE_PNP_INFO` for user space. Opifex
   installs a driver package's `devices.d` file only if it is signed by a key
   allowed to claim devices (system packages for v1); a third-party app can
   never claim a device class by installing.

### 4.3 Claiming a device from a kernel driver

Two cases need more than "load the module":

- **We want a kernel module that loses to a generic one** (GPU vs `vgapci`).
  Keep preloading the GPU base on machines that have the GPU, and give the
  agent a `claim = { rebind = "atrium_gpu_amd_pci" }` action that runs
  `devctl set driver -f` once, before frescod starts. The vendor base stays
  vendor-only in its PNP table; the per-architecture choice remains inside the
  driver (`identify.c`, IP discovery), so no device-ID lists in policy.
- **We want user space, the kernel attached a driver** (`ng_ubt`, `ukbd` on
  a FIDO key's keyboard interface, `uhid`). Prefer `devmatch_blocklist` for
  whole classes we always drive ourselves (`ng_ubt`); use the agent's
  `detach` for single devices. The kernel driver is never unloaded globally
  for one device.

### 4.4 Firmware

- Kernel drivers: raw files under `/boot/firmware/<vendor>/` via
  `firmware_get_flags(…, FIRMWARE_GET_NOWARN)`, as atrium-gpu-amd does.
- Jailed drivers: `/usr/local/share/atrium/firmware/<driver>/` mounted
  read-only into the jail; the driver uploads it itself (Vinculum's Intel and
  Realtek loaders).
- Shipped as signed firmware packages through Opifex, redistributable binary
  blobs only, never disassembled (amdgpu-firmware-license). No `fwget`, no
  network on any device path; an offline install has the firmware for every
  supported part.

### 4.5 Where the agent lives

The agent needs root for three things only: `devctl` ioctls, `devfs rule
apply` on other jails' mounts, and jaild requests with device grants. Its
input is the kernel's own strings, parsed by `atrium-devevents`; it never
reads device bytes (descriptors are parsed by the kernel or by the jailed
driver). Recommendation: lift portcullisd's `identity_devices.rs` into its own
small daemon (accepted: `dispensatord`) so the app-launch broker does not also hold device power, and
so its crash or restart does not cost app launches (decision 2).

### 4.6 Removable storage

Added 2026-11-13 after Shankar's questions on USB drives. A USB disk is a
kernel-class device like `pcm`: `umass` attaches it, CAM makes `daN`, GEOM
makes partitions, and `dispensatord` gives each partition to one owner, the
storage service (storage.md §8.5, amended to match). What changes is who
parses the bytes.

**Filesystems.** Shankar 2026-11-13: Atrium supports **Tessera and FAT**
(FAT12/16/32, and exFAT because most sticks over 32 GB ship with it), perhaps
**UFS** later. ext4, ZFS and NTFS are not supported on removable media.

| Filesystem | Untrusted disk | Trusted disk (§ below) |
|---|---|---|
| FAT / exFAT | jailed Rust FUSE server, read-write; only ever this way (no kernel `msdosfs` mount of removable media) | same |
| Tessera | jailed FUSE server on the Tessera core (`tessera-decode` is already `no_std`, panic-free, differential-tested; the host tools link it), read-only | kernel `tessera_fs` mount |
| UFS (maybe) | jailed read-only UFS2 FUSE reader in Rust | kernel mount |

The FUSE server jail holds two fds, the partition and `/dev/fuse`; no network,
a sysfil profile, Capsicum. jaild runs `mount_fusefs` from outside, so no jail
gets `allow.mount.fusefs`. FreeBSD's `fusefs(5)` is native VFS code
(rewritten and heavily tested in 2019); it now parses the server's replies,
a far smaller and more regular surface than on-disk FAT or UFS structures.
The session sees the mount through nullfs, `nosuid,noexec`; nothing is mounted
while the session is locked; eject syncs, unmounts, then lets the device go;
a surprise removal only gives the FUSE jail I/O errors.

**Optical discs.** Drives (USB or SATA) attach as `cdN` through CAM, and
`dispensatord` hands the drive to the storage service like a USB disk. Data
discs are ISO 9660 (with Joliet/Rock Ridge) or UDF: both are read-only,
simple formats, so they get a jailed read-only Rust FUSE reader. The kernel's
`cd9660` and `udf` are never used for media, and the `iso9660` label taster is
off. Audio CDs have no filesystem: a jailed reader sends MMC commands (READ CD,
TOC) through the drive's `pass` node and gives Lyra or the player PCM. Burning,
if ever wanted, is the same jailed MMC path. Video DVD/Blu-ray (CSS, AACS) is
out of scope.

**Kernel tasting.** GEOM parses every new disk's partition table and labels
before `dispensatord` sees it, and that cannot be routed to a jail per device.
So the Atrium kernel keeps only `GEOM_PART_GPT` and `GEOM_PART_MBR` (with its
EBR chain; gets a fuzz pass), drops BSD, BSD64, APM, VTOC8 and LDM, and turns
off every label taster except `gpt`/`gptid` (`kern.geom.label.*.enable=0`).
Partitions appear as `daNpM` (GPT) or `daNsM` (MBR) and are handled alike.

**Trust is a key, never an ID.** GPT GUIDs, MBR signatures, pool or volume ids
are bytes anyone can copy onto a stick. A disk is *trusted* only when it
unlocks with a key this machine holds (in Arca). That makes Tessera at-rest
encryption (tessera-fs.md flag `ENCRYPTED`, reserved for v2) a prerequisite for
any kernel mount of removable media, and it must **authenticate** metadata
(AEAD, e.g. the planned AES-GCM), not only encrypt it: XTS alone still feeds
the kernel attacker-influenced bytes and allows rolling back to an old copy.
Until Tessera encryption lands, every removable disk takes the jailed path.

## 5. How existing work maps onto it

| Today | Becomes |
|---|---|
| Sigillum S2d agent in portcullisd (HID FIDO/input split) | first two match entries; code moves, behaviour unchanged |
| Tactus ruleset 26 + per-mount unhide | `owner = service tactusd` for HID collections on pages 0x01 (Generic Desktop) and 0x0C (Consumer), and `atkbd`/`psm` |
| Vinculum ruleset 25 + devd rule + `devmatch_blocklist="ng_ubt"` | `bt-controller` entry; blocklist kept |
| Sigillum CCID (class 0x0B) and sensors (VID:PID) | two more entries, driver jails |
| Lyra `dsp*`/`mixer*` wildcard in ruleset 24 | per-node `owner = service lyrad` on `pcm` attach; ruleset 24 keeps only `laminar` |
| GPU preload + manual `devctl set driver -f` | preload on GPU machines + agent `rebind` claim before frescod |
| R9700 image keeping GPU modules off the module path | unchanged while bring-up runs; afterwards the hint table + claim |

## 6. Build order

- **D0** (this doc): agreed design and decisions.
- **D1** typed `atrium-devevents` (Attach/Detach/Nomatch/Notify, devinfo
  snapshot, stable ids, generation), fuzz target, host tests against recorded
  devd traffic from the VM and the R9700.
- **D2** match table format + evaluator (pure crate, host-tested), the agent
  daemon with Sigillum's current behaviour as its only entries; S2e VM gate
  re-run unchanged.
- **D3** Tactus and Lyra onto per-node grants; ruleset 24 and 26 wildcards
  removed. VM gate: plug/unplug a QEMU `usb-kbd`, `usb-audio` and
  `u2f-emulated` in any order, each lands with its owner only.
- **D4** Vinculum `bt-controller` (with B1), CCID (with Sigillum S3).
- **D5** GPU claim on the R9700 after bring-up finishes; drop the manual
  `devctl set driver -f`.
- **D5b** removable storage: GEOM trim; jailed FAT/exFAT FUSE server;
  jailed read-only Tessera FUSE server; storage-service mount flow. Kernel
  Tessera mounts of removable media wait for Tessera encryption.
- **D6** Curia "Devices" list: every device, its owner, unclaimed ones, and
  why.

## 7. Not in scope

- New kernel drivers or hardware coverage (Wi-Fi, laptop I²C/SPI sensors).
- User-space PCI drivers. FreeBSD has no IOMMU-backed user DMA outside bhyve;
  revisit only if a device class needs it.
- App access to raw devices (WebUSB/WebHID-style). If ever wanted, it is a
  further `owner = app` kind with a per-device user consent in trusted UI, and
  a separate proposal.
- Replacing devd. Possible later (a Rust reader of `/dev/devctl` in the
  agent), but devd is small, its input is the kernel's, and nothing needs it.

## 8. Decisions

**Accepted by Shankar, 2026-11-13: all nine, as recommended.**

1. **Keep FreeBSD's kernel matching (newbus, PNP_INFO, devmatch, devd) and
   add only the policy layer above it.** *Recommend yes.*
2. **One device agent as its own small root daemon**, taking over
   portcullisd's identity-device agent. *Recommend yes*; the alternative is to
   grow it inside portcullisd, which keeps one fewer process but puts device
   power in the app broker.
3. **Declarative match table in `devices.d/*.toml`, shipped by signed system
   packages only for v1.** *Recommend yes*; third-party driver packages later,
   behind Opifex signing.
4. **Default deny: an unmatched device is visible to nobody**, listed in
   Curia. *Recommend yes*; it is already Sigillum's behaviour.
5. **Kernel driver for DMA/IRQ devices, jailed user-space driver for
   USB/HID/serial/BT-handed devices** (§4.1). *Recommend yes.*
6. **Retire wildcard device rules** in rulesets 24 and 26 in favour of
   per-node grants (D3). *Recommend yes*; needs the Tactus and Lyra threads.
7. **GPU binding: preload + an explicit claim step before frescod**, not
   device-ID tables in policy. *Recommend yes*; keeps "no RDNA4 hardcoding".
8. **Firmware ships offline in signed packages; no `fwget`, no network on
   device paths.** *Recommend yes.*
9. **Name.** If decision 2 is yes, the daemon needs a name; proposed
   **Dispensator** (`dispensatord`), the steward who handed out the household's
   stores. Goes into `docs/NAMING.md` if agreed.

**Removable storage (§4.6), Shankar 2026-11-13:**

10. **Filesystems: Tessera and FAT/exFAT; UFS maybe; no ext4, ZFS or NTFS.** Decided.
11. **Untrusted media is parsed in a jailed FUSE server, never by a kernel
    filesystem.** Decided (follows from 10 and the discussion).
12. **Trusted = unlocks with a key this machine holds, never an on-disk id.**
    Decided in discussion; Tessera at-rest encryption with authenticated
    metadata moves up (Shankar: "needed sooner rather than later").
13. **Kernel GEOM trimmed to GPT + MBR/EBR, label tasters off except GPT.**
    *Recommend yes.* Open.
14. **exFAT included under "FAT".** *Recommend yes* (most sticks over 32 GB).
    Open.
15. **Optical: ISO 9660 + UDF via a jailed read-only reader; audio CDs via
    jailed MMC reads; no kernel `cd9660`/`udf` for media.** *Recommend yes.* Open.
