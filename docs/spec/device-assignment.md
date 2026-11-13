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
| portcullisd `identity_devices.rs` (until 2026-11-13; now `atrium-dispensatord`, §6.1) | `atrium-portcullisd-daemon` (rc.d never starts the multi-tenant portcullisd; sigillum.md §16.2) | unhides input hidraw nodes in tactusd's own devfs mount (re-checked every 2 s); starts and stops one `sigillum-fido` jail per FIDO key via jaild `devfs_grant` | hard-coded to HID; lives inside the app-launch broker |
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
   already does this for HID. On 16-CURRENT `devinfo -v` prints
   `name <description> pnpinfo …`, so the parser skips the description.
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
               nodes = ["u2f/{u2f_unit}", "hidraw{unit}"],   # first present wins
               firmware = "" }
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

**Kernel drivers that are fine as the node provider.** FreeBSD 15+ `u2f(4)`
(loaded by default on 16-CURRENT) attaches to a FIDO collection as `/dev/u2f/N`
(devd/devinfo name `uhidN`) and sets `HQ_NO_READAHEAD` on the hidbus, after
which the key's `hidrawN` delivers only the first report of each answer. So
the FIDO rule grants `/dev/u2f/N` when u2f is attached and falls back to
`hidrawN` only when it is not (Sigillum S2e, `b680fa0`): jaild's `devfs_grant`
takes `dir/name`, the pin's `grant_nodes` are `["hidraw#", "u2f/#"]`, and the
driver's sysfil profile adds `USB_GET_DEVICEINFO` and `0x41705570`. A rule's
`nodes` list is therefore ordered, first present wins.

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

### 4.7 When nothing matches

Accepted 2026-11-13 (Shankar asked: ignore it, or point the user at a driver
the way Ubuntu points at NVIDIA's?).

1. **Always listed, never exposed.** The device stays visible to nobody (§8
   decision 4) and appears in Curia's Devices list as *no driver*, with its
   class and IDs.
2. **Look it up locally.** Opifex's catalog index carries every published
   driver package's `devices.d` match entries, much like Ubuntu's modalias
   lists or FreeBSD's `linker.hints`. `dispensatord` matches the device
   against the *downloaded* index on the machine. Device IDs are never sent to
   a server, because a list of hardware IDs is a fingerprint.
3. **Tell the user only when it helps.** A Praeco notice appears only when a
   catalog driver matches *and* the device is user-facing: a display, network,
   audio, input, storage, camera or printer, or anything the user just
   plugged in. Unmatched hubs, bridges and sensors at boot stay silent in the
   list. The notice says "a driver for <device> is available", and installing
   it goes through Opifex's trusted install sheet, because a driver package
   gets device power. For v1, only system-signed packages can declare
   matches (decision 3).
4. **Activation.** A jailed driver starts at once. A kernel module loads
   through devmatch on the next nomatch rescan. When a generic driver already
   holds the device, the package's `claim` runs once (§4.3). A reboot is asked
   for only when the module cannot detach what holds the device.
5. **No driver anywhere.** The Devices list says *not supported*, with a
   fallback where one exists. A GPU without a driver keeps the firmware
   framebuffer (efifb) and the Tier-2 CPU renderer, and Atrium still boots
   and runs. An opt-in "report this device" sends the IDs to the hardware
   list only when the user presses it.
6. **Closed kernel drivers are not offered.** An NVIDIA GPU is the case Ubuntu
   solves with NVIDIA's own module. NVIDIA's FreeBSD driver is a closed kernel
   module, and running it would put unreviewed closed code in ring 0, outside
   every isolation boundary Atrium has. So it gets the fallback above until
   there is a native driver behind the per-architecture GPU interface.
   Closed *user-space* pieces, if ever needed, would go through Hospes in a
   jail, never into the kernel.

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
- **D2** (boot prerequisite: `nullfs` preloaded; two concurrent first nullfs
  mounts panicked 16-CURRENT, sigillum.md §16.2) match table format + evaluator (pure crate, host-tested), the agent
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

### 6.1 As built: D1 and D2 (2026-11-13)

- **D1** `atrium-devevents/src/devctl.rs`: `parse_line` turns each devd line
  into `Attach`/`Detach`/`Nomatch`/`Notify` with a bounded `Fields` list
  (quoted values with escapes; line ≤ 8 KiB, ≤ 64 fields, keys ≤ 32, values
  ≤ 512; anything else is dropped, never a panic). `devinfo_events` reads
  `devinfo -v` (incl. FreeBSD 16's `<description>`) as parent-first events.
  `Registry` gives every device an id that is never reused and removes a
  whole subtree when its top detaches (the kernel reports only the top, and
  never an unclaimed child leaving). `DeviceWatcher::recv_line` hands raw
  lines out. Fuzz target `atrium-devevents/fuzz` (`devctl`: parser, registry
  and the HID classifier): 913,380 runs in 3 min, no crash. Not yet: traces
  recorded on the R9700.
- **D2** `dispensator/` (lib + `atrium-dispensatord`):
  - `rules.rs`: the `devices.d/*.toml` format: `[[rule]]` (class `hid` for
    now, `priority`, `any_page`, ordered `nodes`, `owner = driver:X |
    service:Y`), `[driver.X]` (exec, argv, uid/gid ≠ 0, `jail`/`root` with
    `{hidraw}` only, devfs ruleset ≠ 0, sysfil profile, read-only mounts),
    `[service.Y]` (its devfs mount, reshow period). Unknown keys, a name
    defined twice (one package redefining another's driver), unknown
    placeholders, `..`, relative paths and dangling owners are refused. The
    daemon refuses rule files not owned by root or writable by group/other.
  - `plan.rs`: devd lines and `devinfo -v` to ops (`Show`, `StartDriver`,
    `StopDriver`, `Log`); highest priority wins; no rule, or no collections,
    is nobody. Test `same_as_the_old_agent` runs portcullisd's old
    `hidowner` traces (plus the u2f(4) ones) through both and gets the same
    ops; `jail.rs`'s test checks the Sigillum jail request field by field
    against the old `driver_request`.
  - Shipped rules: `sigillum/etc/devices.d/40-sigillum.toml` (FIDO page,
    priority 100, `u2f/{u2f}` then `hidraw{hidraw}`) and
    `tactus/etc/devices.d/30-tactus.toml` (any other HID, priority 50,
    service tactusd); installed by `sigillum-service.sh` /
    `tactus-service.sh`.
  - The daemon: the old agent's runner, generalised (devfs per-mount unhide
    with reshow, jaild `CreateJail` with `devfs_grant`, unmount before start,
    stop by `{hidraw}` alone, procdescs held), plus a state file
    `/var/run/atrium/dispensator.state`; `--check` and `--plan` run anywhere.
    rc.d `dispensator/etc/atrium-dispensatord` (after jaild; loads `hidraw`
    and `nullfs`, refuses to start if the rules do not load);
    `deploy-portcullis.sh` installs and starts it.
  - portcullisd's agent (`identity_devices.rs`, the spawn in
    `atrium-portcullisd-daemon`) is removed; `vm-sigillum-check.sh` now
    commissions dispensatord.
  - Tests: 8 host tests; FreeBSD (x86_64) type-check of dispensatord and
    portcullisd clean. **Not yet run: the S2e VM gate against dispensatord.**

### 6.2 As built: D3 (2026-11-13)

- **S2e gate passed against dispensatord** (Sigillum thread, amd64 VM,
  f444666, with tactusd): key jail `atrium-sigillum-fido-1` driven through
  `/dev/u2f/0`, its `/dev` holding only that node, `_sigillum-dev` 0600, no
  network; tactusd saw the keyboard and not the key; ceremonies OK; a
  hot-plugged key got its own jail and lost it on unplug, the first untouched.
- **A second rule class, `driver`**: a device a kernel driver attached, by
  driver name (`driver = "pcm"` matches `pcm0`, `pcm1`...), placeholder
  `{unit}`; `all_nodes = true` grants every node template (a sound card is
  `dspN` and `mixerN`). A device the registry sees removed (directly, or as
  part of a detached subtree such as `uaudio0`) is released. Driver owners
  were refused `all_nodes` (a jail got one node; lifted in §6.3) and must name their jail
  and root by the class's instance placeholder only.
- **Rules**: `lyra/etc/devices.d/35-lyra.toml` (`pcm` → lyrad, `dsp{unit}` +
  `mixer{unit}`); `tactus/etc/devices.d/30-tactus.toml` gains `atkbd` →
  `atkbd{unit}` and `psm` → `bpsm{unit}`. `lyra-service.sh` installs Lyra's.
- **Rulesets**: 24 no longer unhides `dsp*`/`mixer*`, 26 no longer unhides
  `atkbd*`/`bpsm*` (modes kept). lyrad opens `/dev/dsp` and `/dev/mixer`:
  `devfs_lookup` runs sound(4)'s `dsp_clone`/`mixer_clone`, which return the
  default unit's cdev, and the lookup then uses *that* cdev's entry in the
  same mount, so the default name works exactly when the default card has
  been shown (read in `sys/fs/devfs/devfs_vnops.c` and
  `sys/dev/sound/pcm/dsp.c`, main, 2026-11-13). tactusd already rescans every
  2 s in its jail (no devd socket there), so per-node showing reaches it.
- **Gate**: `scripts/vm-devices-check.sh` (rulesets, lyrad and tactusd
  mounts, `/dev/dsp` through the clone, isolation between the two, USB sound
  card hot-plug and unplug). 10 host tests. **Passed 2026-11-13** (Sigillum
  thread's amd64 VM, intel-hda + PS/2, 0a76cb7): lyrad saw dsp0/mixer0 and
  `/dev/dsp`/`/dev/mixer` resolved, tactusd saw atkbd0/bpsm0, neither saw the
  other's nodes, a USB card came and went as dsp1. Both gate scripts now fail
  at once when the VM does not answer (an empty answer had passed the
  "does not contain" checks).
- `deploy-portcullis.sh` now passes the cross sysroot flags when it builds
  dispensatord (the Sigillum gate found the plain build failing).

### 6.3 As built: D4 groundwork (2026-11-13)

D4's two drivers (Vinculum's USB controller transport, B1b, and Sigillum's
CCID driver, S3) do not exist yet. What they need from this layer does:

- **A third rule class, `usb`**: one interface of a USB device, from the
  attach (`+ng_ubt0 at ... on uhub0`) or nomatch (`? at ... on uhub0`) line
  the kernel sends per interface, which carries `ugen=ugenB.A`, `interface=`,
  `vendor`/`product` and `intclass`/`intsubclass`/`intprotocol`; `devinfo -v`
  replays the same at start. A rule matches by `usb_interface = [class,
  subclass, protocol]` (a prefix: `[0x0B]` is every smart-card reader) and/or
  `usb_ids = ["vvvv:pppp"]`; one of them is required, so no rule takes "every
  USB device". Placeholders `{bus}`, `{addr}`, `{interface}` and `{usb}` =
  `B-A-I`, the instance placeholder (jail names take no dots). The device key
  is `ugenB.A:I`.
- **Going away**: an unclaimed interface never gets a detach line, so the
  device's end is the `!system=USB subsystem=DEVICE type=DETACH ugen=...`
  notify; it releases every interface of that ugen and forgets them in the
  registry, so the same address plugged again starts afresh. A kernel driver
  leaving an interface is not the device going.
- **Kernel drivers**: an interface a kernel driver has stays the kernel's
  unless the rule says `detach_kernel = true`; then dispensatord runs
  `usbconfig -d ugenB.A -i I detach_kernel_driver` before starting the owner,
  and again if a module devmatch loads later takes it back (the jail is
  kept). Whole classes we always drive ourselves still belong in
  `devmatch_blocklist` (§4.3); `detach_kernel` is for the rest. Kernel-claimed
  interfaces no rule wants are left alone quietly; unclaimed ones are logged
  ("shown to nobody").
- **Several nodes per jail**: libusb on FreeBSD opens `/dev/ugenB.A`, a devfs
  symlink to `/dev/usb/B.A.0`, and drives every endpoint through ioctls on
  it, so a USB driver jail needs both. jaild's `CreateJail` gains
  `devfs_grants` (up to 7 more nodes, each allowed by the pin, only with
  `devfs_grant` set; omitted from the wire when empty, so existing requests
  are unchanged); a symlink is only unhidden, its target gets the owner and
  0600. Grant patterns take `#` anywhere (1 to 4 digits): `ugen#.#`,
  `usb/#.#.0`; grant names may hold dots between digits and letters. Driver
  owners may now use `all_nodes`; `{node}` in argv is the first node.
- **At start**: the USB stack keeps no newbus child for an interface no
  driver claimed (it probes with a temporary device and deletes it after
  the nomatch), so `devinfo -v` never shows one. dispensatord therefore also
  runs `usbconfig` at start (`dump_device_desc dump_curr_config_desc` per
  `ugenB.A`), reads only the numeric `idVendor`/`idProduct`/`bInterface*`
  fields (alternate setting 0), and treats each interface devinfo did not
  show as claimed, hubs excepted, as a nomatch. A device can describe
  itself however it likes anyway, so this trusts it no further than devd's
  own lines do. Found by the first VM gate run (an unclaimed CCID reader
  was missing from `devinfo -v`).
- **Gate**: `scripts/vm-usb-check.sh` with a stand-in driver
  (`dispensator/examples/usb-probe.rs`), QEMU `usb-audio` (uaudio detached
  for the rule) and `usb-ccid` (nomatch, then a daemon restart). First two
  runs (Sigillum thread's VM): the reader was assigned `ugen0.5` +
  `usb/0.5.0`, and the jail was refused for the gate's own test uid (fixed);
  QEMU's `usb-serial`, the first choice for the kernel-claimed case, never
  enumerated in the guest behind QEMU's added hub, so the gate uses
  `usb-audio`.
- **Not shipped yet**: rule files and jaild pins for `vinculum-hci`
  (ruleset 25) and `sigillum-ccid`; each is one `devices.d` file plus one
  `[[devfs_rulesets.pinned]]` entry with `grant_nodes = ["ugen#.#",
  "usb/#.#.0"]` when its driver lands. Host tests: dispensator 13 (USB
  attach, repeat, unplug and replug at the same address, priority by id,
  kernel detach and re-detach, malformed fields, devinfo at start),
  jaild-policy grant patterns; jaild's validator and grant-name tests run on
  FreeBSD (type-checked here).

## 7. Not in scope

- New kernel drivers or hardware coverage (Wi-Fi, laptop I²C/SPI sensors).
  Shankar 2026-11-13: Wi-Fi gets Atrium's own drivers, as the GPU does; FreeBSD's
  linuxkpi-based `iwlwifi`/`rtw88`/`rtw89` are not used. Spec: [wifi.md](wifi.md).
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
    Accepted 2026-11-13.
14. **"FAT" = FAT12/16/32 + exFAT, all with VFAT long names.** Accepted 2026-11-13 (FAT16: SD ≤ 2 GB, cameras; FAT32: SDHC, most sticks, ESP; exFAT: SDXC > 32 GB).
15. **Optical: ISO 9660 + UDF via a jailed read-only reader; audio CDs via
    jailed MMC reads; no kernel `cd9660`/`udf` for media.** Accepted 2026-11-13.

**Unmatched devices (§4.7), accepted by Shankar 2026-11-13:**

16. **Driver lookup against the locally downloaded Opifex index; device IDs
    never leave the machine.** Accepted 2026-11-13.
17. **Notify only for user-facing or just-plugged devices with an available
    driver; install through Opifex's trusted sheet.** Accepted 2026-11-13.
18. **No closed-source kernel drivers (e.g. NVIDIA's); fallback to efifb +
    Tier-2 CPU rendering.** Accepted 2026-11-13.
