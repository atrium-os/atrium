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

### 4.8 Apps and the files on a stick

Shankar 2026-11-14: a stick typically goes to the file manager, but an app
that wants to open a file on it should be able to, with one-shot permission.

1. **No app sees `/media`.** The session's own mount (§6.5) stays the
   user's; app jails run under their own uids (portcullis-peer) and get
   nothing from it by default.
2. **The file manager** (Scrinium) holds the restricted capability
   `removable-files` and is given the whole stick, read-write, for as long
   as it runs.
3. **Every other app asks the system picker.** The picker (Scrinium's,
   drawn outside the app's jail) shows the sticks and the user picks a file
   or a folder; the app never names the scope. A file comes back as an open
   descriptor over Aqueduct; a folder as a mount of that folder alone in
   the app's jail.
4. **One shot.** A grant ends when the app's jail goes, the stick is
   ejected or unplugged, or the session locks or ends. The next launch asks
   again. "Always allow for this app" may come later, listed and revocable
   in Curia.
5. **One writer.** A grant is another FUSE mount served by the stick's own
   server: dispensatord mounts fusefs at `/media/.grants/<n>` and passes the
   `/dev/fuse` descriptor to the server over its control socket with the
   folder's path and the app's uid. The server serves that mount rooted at
   the folder, owned by the app's uid (0700, so the kernel still checks
   modes), from the same open volume, so the stick never has two writers.
   jaild attaches it into the app's jail as a nullfs runtime mount, which it
   removes with the jail.
6. **Who may grant.** dispensatord's grant op takes root only (portcullisd).
   The picker does not go through it: as built (§6.11,
   [scrinium.md](scrinium.md) §4), Scrinium opens a picked file from its own
   standing grant and hands it over Limen (`EMIT_FD`), and asks for a picked
   folder with `PICKED`, which only an app holding the restricted capability
   `file-picker` may call.

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
  `usb-audio`. The third run assigned both correctly and was refused on
  the test gid (fixed). **Passed** 2026-11-13 (9e12750): uaudio detached
  and the probe's jail up with exactly `ugenB.A` + `usb/B.A.0` (uid 65060,
  0600), the CCID reader by nomatch and again after a daemon restart (found
  through usbconfig), both gone on unplug.
- **Not shipped yet**: rule files and jaild pins for `vinculum-hci`
  (ruleset 25) and `sigillum-ccid`; each is one `devices.d` file plus one
  `[[devfs_rulesets.pinned]]` entry with `grant_nodes = ["ugen#.#",
  "usb/#.#.0"]` when its driver lands. Host tests: dispensator 13 (USB
  attach, repeat, unplug and replug at the same address, priority by id,
  kernel detach and re-detach, malformed fields, devinfo at start),
  jaild-policy grant patterns; jaild's validator and grant-name tests pass
  on FreeBSD.

### 6.4 As built: D5b, FAT library (2026-11-13, in progress)

The removable-storage step starts with the part that parses untrusted
bytes. `capsa/capsa-fat` (Capsa, NAMING.md) reads FAT12/16/32 with long
file names:

- boot sector checked against itself and the device size (sector and
  cluster sizes, FAT size against the cluster count, FAT32 version, root
  cluster, active FAT); FAT type by cluster count, as the specification
  defines it; exFAT is recognised and refused here (its own module comes
  next);
- the FAT through a bounded sector cache; every chain walk is bounded by
  the cluster count, so a loop is an error, not a hang; links outside the
  volume, free or bad clusters inside a chain are errors for that
  operation only;
- directories capped at 65536 slots; long names only when their slots are
  complete, in order and match the 8.3 checksum, else the 8.3 name (CP437,
  with NT's lowercase bits); names with `/`, NUL, control characters, `.`,
  `..` or over 255 bytes are skipped, never handed up; lookup is
  case-insensitive by long or 8.3 name;
- byte-addressed `BlockDev`, with `Aligned` for disk devices that take
  whole sectors only.

Tests: volumes made by `mkfs.fat` and `mtools` as FAT12, FAT16 and FAT32
(long and Unicode names, nested directories, a 300-entry directory over
several clusters, files across cluster edges read in odd-sized pieces, 8.3
lookups, label), and 300 randomly damaged volumes walked end to end with no
panic or hang.

Writes: create (exact 8.3 names with NT case bits when the name is one,
else a checksummed long name and a unique `BASE~N` alias), write at any
offset (FAT has no holes, so a gap is zero-filled), truncate both ways,
unlink, mkdir/rmdir (only empty), rename within and across directories
(replacing a file or an empty directory, case-only renames in place, never
a directory into its own subtree, `..` fixed up), mtime; directories grow
by clusters up to the 65536-slot limit; `sync` writes every FAT copy and
marks FSInfo's hints. Names Windows cannot round-trip (`"*/:<>?\|`,
trailing dot or space) are refused. Tests: a scripted session on FAT12, 16
and 32 volumes made by `mkfs.fat`, then `fsck.fat -n` must find nothing
and `mtype` must read every file back by its long name; a volume filled to
`NoSpace` (data and the fixed root) stays sound.

exFAT, read side (`capsa_fat::exfat`): the boot region's checksum and
layout checked (heap after the FAT, heap inside the volume, at most 2^28
clusters so the FAT32 table code is exact), entry sets used only when the
set checksum, stream entry and name entries agree, `NoFatChain` runs
checked against the heap, bytes past `ValidDataLength` read as zeros, free
space from the allocation bitmap. Tested on volumes made by `mkfs.exfat`
and filled through `exfat-fuse` on a loop device (long and Unicode names,
two files grown in turns so they fragment onto FAT chains, a 300-entry
directory), plus 200 damaged copies. exFAT writes come later.

Fuzzing: `capsa-fat/fuzz` target `volume` takes any bytes as the start of
a 64 MiB device and opens, walks, reads, then creates, writes, renames,
removes and syncs.

**The FUSE server** (`capsa/capsa-fuse`, binary `atrium-capsa-fuse`)
speaks the FUSE kernel protocol (7.x, answering min(kernel, 7.31)) itself,
no libfuse: it is handed an open partition and an open, already-mounted
`/dev/fuse` and nothing else. Lookups, attributes (owner = the session
user from `--uid/--gid`; mode 0644/0755, read-only bit and `--ro` give
0444/0555), readdir, read, write, create, mkdir, unlink, rmdir, rename and
rename2 (`NOREPLACE`), truncate and mtime, statfs, fsync/flush (the FAT is
synced on close and after every directory change). Node ids follow
renames; a removed or replaced entry's id answers `ESTALE` instead of
reaching whatever reuses its slot. Request framing is checked before any
field is read. FAT is served read-write, exFAT read-only (read-write since §6.8). Disk devices go
through `Aligned` with `DIOCGSECTORSIZE`/`DIOCGMEDIASIZE`. Tested under a
real kernel FUSE mount in the cloud container (Linux's fuse speaks the same
protocol): a FAT32 volume written through `std::fs` (3 MB file, nested
directories, Unicode name, append, overwrite, truncate, rename across
directories and replacing, rmdir refusal, bad name refused, case-insensitive
lookups, statvfs), then `fsck.fat -n` clean and `mtype` agreeing; and
`--ro` giving `EROFS`.

**Mounting from outside the jail** (jaild, `CreateJailRequest::fuse`):
the request names a block device (relative to `/dev`) and a mountpoint.
jaild loads `fusefs.ko` if needed, opens the partition (read-only when
asked) and `/dev/fuse` itself, creates the mountpoint, and calls `nmount`
with `fstype=fusefs`, `fd=`, `allow_other`, `default_permissions`,
`nosuid` (and `ro`), then pdforks; the child moves the two descriptors to
fd 3 and fd 4 before `jail_attach`, so the server never sees either node
and no jail has `allow.mount`. Only a pinned ruleset whose new
`fuse_devices` patterns (`da#p#`, `#` = 1 to 4 digits) allow the device,
with the mountpoint one plain name directly under the pin's
`fuse_mount_prefix`. The mount is recorded with the jail and
force-unmounted at RemoveJail; a stale mount from a server that died is
cleared before the next one. The server enters Capsicum after reading the
partition's geometry; its sysfil profile `fuse-server` is `base` plus
`DIOCGSECTORSIZE`/`DIOCGMEDIASIZE`.

**The `disk` rule class** (dispensatord): `disk = ["da"]` matches the
partitions GEOM creates on those disks (`!system=GEOM subsystem=DEV
type=CREATE cdev=da0p1`, `daNpM` or `daNsM`; `DESTROY` ends it), and at
start the partition names already in `/dev`. Placeholders `{part}` (the
instance) and `{disk}`. A disk goes only to a driver with `fuse_mount`
(`/media/{part}`), never to a service, and such a driver serves only
disks: its node becomes the request's `fuse`, not a devfs grant. A whole
disk without a partition table (superfloppy) is not matched yet.

Auto-mount (session owner, lock, eject) followed the same day: §6.5. `scripts/vm-fuse-check.sh`
(FreeBSD VM): a FAT32 volume on md(4) served through jaild and jclient
(reads, writes, renames, a 2 MiB file, the session uid, fds 3/4 only, no
node in the jail, fsck_msdosfs clean, msdosfs reads it back), read-only
gives EROFS, refusals for the VM's own disk and a mountpoint outside
/media, then dispensatord serving a GPT partition on a fresh md at
`/media/md98p1` and removing it when the md goes. **Passed** 2026-11-13 on
the amd64 VM (9e12750; output in project files `vm-runs/`). It found one
real case: FreeBSD's `newfs_msdos` can size the FAT two entries short of
the data area's clusters, which capsa-fat refused; it now limits the
volume to what the FAT addresses, as Linux and FreeBSD do. jaild's unit
tests (87) and jaild-policy's (9) pass on FreeBSD.

Next: exFAT writes, a whole-disk (superfloppy) case,
and the removable-media service (session owner, lock, eject).

### 6.5 As built: auto-mount for the session (2026-11-13)

Shankar 2026-11-13: "do the auto-mount". dispensatord does the
removable-media service's job itself (`dispensator::media`), since it
already owns the partitions:

- **Who.** A partition goes to a FUSE driver only while a human session is
  active and unlocked, read from ostiarius's seat file
  (`/atrium/sockets/ostiarius/active-session`) and the lock file beside it
  (`session-locked`, naming the locked user; a lock naming someone else is
  stale and ignored). The driver's argv takes `{session_uid}` and
  `{session_gid}`, allowed only for FUSE drivers; root or an unknown name is
  no session. Plugged in at the login screen: parked until login.
- **Private.** The shipped server runs with `--umask 077`, so the volume is
  0700/0600 and, with `default_permissions`, only the session's user (and
  root) can open it.
- **Follows the session.** The daemon re-reads the two files every tick
  (2 s). Lock, logout and a user switch stop every server (jaild's forced
  unmount); unlock and login start them again; a switch restarts them as
  the new user.
- **Eject.** Over the media socket (§6.6; at first a line protocol on
  `dispensatord/media.sock`), from root or the active session's user;
  `atrium-dispensatord --media` / `--eject PART` is the client. Eject unmounts without force first, so an open file answers
  "busy" instead of losing data, then stops the server; the partition stays
  down (across relogins) until it is unplugged.
- **ostiarius.** `Lock` and `Unlock { password }` requests on its control
  socket set the lock file; unlock re-authenticates the active user; login
  and logout clear it. The lock screen itself is not built yet; it will
  send these.
- **Shipped.** `capsa/etc/devices.d/50-capsa.toml` (every `da` partition,
  GPT or MBR, to driver `capsa`, mounted at `/media/<part>`), devfs ruleset
  `atrium_capsa=31` (basics only), jaild pin 31 (`atrium-capsa-*`, uid
  `_capsa` 65031, `fuse_devices = ["da#p#", "da#s#"]` under `/media`), and
  `scripts/capsa-service.sh` (user, fusefs at boot, binary, rule). Internal
  disks (`ada`, `nda`) never match.

Gate `scripts/vm-automount-check.sh` (amd64 VM, a QEMU usb-storage stick
with an MBR FAT32 partition, hot-plugged): **passed** 2026-11-13. Nothing
mounted at the login screen; on login `/media/da0s1` is alice's alone
(0700), reads the file on the stick, takes her write, and bob cannot list
it; lock unmounts and ends the server's jail, unlock brings it back; bob may
not eject, an open file makes the eject "busy", alice's eject works and
holds across a relogin until a replug; a switch to bob serves it as bob;
logout unmounts; afterwards fsck.fat finds the stick clean with alice's
file on it. Host tests: dispensator 20 (session following, eject, the
shipped rule); ostiarius 17 and portcullis-peer 9 on FreeBSD.

Not yet: the session's apps see `/media` through their jails only where a
manifest grants it (storage.md `removable_media`, not wired); a whole-disk
(superfloppy) stick (done, §6.7); exFAT writes (done, §6.8); the lock screen as the caller of
ostiarius `Lock`/`Unlock` (done, lock-screen.md). The Forum eject button came next (§6.6).

### 6.6 As built: sticks in Forum, over Aqueduct (2026-11-13)

Shankar 2026-11-13 asked whether the mount should reach Forum's desktop or
its file manager, maybe via Aqueduct, and chose "events + Forum icon now";
Scrinium (the file manager) attaches mounts later.

- **The service.** Aqueduct class `media` (16, `aqueduct::classes`), crate
  `atrium-media` (`dispensator/media`): `LIST` (the volumes served now),
  `EJECT { part }` and `SUBSCRIBE`, after which the server pushes `EV`
  events, an `Added(Volume)` for each volume served now and then every
  `Added` / `Removed { part, ejected }`. A `Volume` is part, mountpoint,
  size, free space and read-only; free space alone changing is no event.
  Status bytes: OK, MALFORMED, NOT_ALLOWED, NOT_FOUND, BUSY, FAILED.
- **Who.** dispensatord serves it on
  `/atrium/sockets/removable-media/removable-media.sock` (dir 0755, socket
  0666; the service checks). Allowed: root, the active unlocked session's
  uid, or a session component the launch registry says that user owns
  (Forum's bar runs under its own uid). The peer's uid comes from the
  kernel. When the session locks, logs out or switches, every subscription
  is dropped (the stream ends); the subscriber connects again and is
  refused until it is its session again.
- **Threads.** Connection threads ask the daemon's loop to eject over a
  channel and a wake socket, and wait up to 60 s; the loop owns all state.
  A volume is listed only while its fusefs mount is live (`statfs`), so a
  server that exited (not a FAT volume) is not shown.
- **Forum.** forum-bar draws a USB icon in its system row while a stick is
  served: the menu lists each stick (size, free space) with "Eject", and
  shows the refusal ("busy: close the files open on it first") when an
  eject fails. A thread subscribes and re-subscribes every 2 s. The bar
  draws it, as its other system icons, so no app can draw a fake one. It
  reaches the socket through a new capability `removable-media` (restricted,
  held only by `forum-bar`; portcullis-toml, -jail, -policy and ostiarius).
- **CLI.** `atrium-dispensatord --media`, `--eject PART` and
  `--media-watch` (prints `added PART MOUNT`, `removed PART [ejected]`,
  `end`).

Gate `vm-automount-check.sh`, extended: **passed** 2026-11-13 (0 failed).
Alice lists the stick over Aqueduct and bob is refused (status 2); a
subscriber running as alice hears it added, its stream ends on lock, a new
one hears the eject as `removed da0s1 ejected` and ends on logout; bob's
eject is refused, an open file answers BUSY (status 4), with everything
from §6.5 still passing. Host tests: atrium-media 2, forum-bar 18
(the icon only with a stick, the menu, events), portcullis-jail devfs and
the Forum manifest tests (only the bar holds `removable-media`).

### 6.7 As built: sticks with no partition table (2026-11-13)

Some sticks carry FAT straight on the disk ("superfloppy"). GEOM's CREATE
for a whole disk a `disk` rule matches (`da0`) now yields `Op::Settle`;
the daemon calls `Planner::settle` 1.5 s later and, if no partition of that
disk has appeared, serves the disk itself as `{part}` = `da0`, mounted at
`/media/da0`. A partition appearing later (repartitioned while plugged in)
stops the whole-disk server and takes over. At start, bare disks in `/dev`
settle at once. jaild pin 31 adds `da#` to `fuse_devices`. Host test
`superfloppy`; gate `vm-automount-check.sh` adds a bare FAT32 stick
(`mkfs.fat -C`, no MBR): served whole at `/media/da0`, alice reads it, no
partition nodes, unmounted on unplug; **0 failed** 2026-11-13.

### 6.8 As built: exFAT writes (2026-11-14)

`capsa_fat::exfat` writes now (`exwrite.rs`), and atrium-capsa-fuse
serves exFAT read-write like FAT:

- **Allocation** is the bitmap's, held in memory once anything is written
  (at most `clusters / 8` bytes, 32 MiB for the largest volume) and written
  back by dirty 4 KiB chunks. The FAT is never asked what is free: a
  contiguous (`NoFatChain`) file's FAT entries mean nothing. A file this
  side grows gets a FAT chain; a contiguous file is given one first. FAT
  entries are full 32-bit (end `0xFFFFFFFF`), as exFAT has them.
- **Entry sets** (File, Stream, Names) are written whole with their
  checksum and the name hash, computed through the volume's own up-case
  table (decompressed at open; ASCII if it is missing), which is also what
  name lookups compare through. Times are written as UTC.
- **Directories** have no `.`/`..`; one grows by a zeroed cluster and its
  stream's length follows. A directory is known by its first cluster, so a
  handle stays valid while it grows. exFAT keeps no parent links, so the
  writer remembers where each directory's entry is as it reads them (and
  searches from the root, bounded, otherwise). A directory cannot be moved
  below itself.
- **Writes past the valid data** zero the gap on disk first; truncating up
  leaves ValidDataLength where it was, so the new part reads as zeros.
- **VolumeDirty** is set in the boot sector at the first change and
  cleared by `sync` (with PercentInUse) once the FAT and bitmap are
  written; the boot checksum leaves those fields out. A stick pulled
  without eject is marked dirty, and other systems offer to check it.

Tests: `capsa-fat/tests/exwrite.rs` makes volumes with mkfs.exfat (512 B,
4 KiB and 128 KiB clusters), writes through capsa-fat (long, Unicode and
case-only names, interleaved chains, a directory grown past a cluster,
gaps, truncation, renames across directories and replacing, rmdir, files
another system wrote contiguously), then checks them with `fsck.exfat -n`
and reads them back both with capsa-fat and through the exfat-fuse driver:
both trees must equal the model; 600 random operations the same way; 300
damaged volumes take writes without a panic or hang. capsa-fuse
`exfat_through_the_kernel` does the same under a real FUSE mount. Gate
`vm-automount-check.sh` adds an exFAT stick: alice writes 20 MB and a
Unicode name on FreeBSD, ejects, and on the host fsck.exfat finds it clean
and Linux reads both files byte for byte: **0 failed** 2026-11-14.

### 6.9 As built: Tessera sticks, read-only (2026-11-14)

A Tessera stick is untrusted like any other until at-rest encryption can
tell this machine's own disks apart (§4.6), so it is never mounted by
`tessera_fs`. atrium-capsa-fuse recognises it (`TESSERA1` in either
superblock copy) and serves it read-only through the Tessera core's reader
(`tessera/reader.h`, the code the loader boots from, whose on-disk
decoders are the Rust `tessera-decode`), in the same jail and capability
mode as the FAT server. The reader gained `tessera_reader_stat_ex` (mode,
size, times, links) and `tessera_reader_foreach` (directory entries);
`tessera-sys` binds them, and capsa-fuse's `tessera` feature (default on)
links the core. The gate cross-builds the core for FreeBSD with clang and
links it through `TESSERA_CORE_LIB`.

- **Committed state only.** A journal a crash left behind is not
  replayed: the stick shows what was last committed, and nothing is ever
  written to it.
- **Owners and modes** belong to another machine's users, so they are not
  shown: everything is the session user's (0400 files, 0500 directories),
  as on FAT.
- **Symlinks and special files** are left out, and so are names that
  cannot be file names here (empty, `/`, NUL, not UTF-8). A directory
  lists at most 2^20 entries.

Tests: `capsa-fuse/tests/tessera.rs` reads volumes made by mkfs-tessera
(inline and chunked seed files) through the backend and under a real FUSE
mount (reads match, writes refused, mode and owner as above), and reads 300
damaged volumes without a panic or hang. Populated volumes with trees need
the kernel module, so host tests use seed files only; the reader itself is
the loader's, which atrium-tessera tests on its own. Gate
`vm-automount-check.sh` adds a Tessera stick: served at `/media/da0`,
alice reads its file, writes are refused, bob cannot list it, unmounted on
unplug: **0 failed** 2026-11-14.

### 6.10 As built: D6, the Devices list (2026-11-14)

dispensatord now answers "what is in this machine, who has each device,
and why does this one have nobody". `Planner::inventory` walks the device
registry (every newbus device, and every child no kernel driver took) plus
the disks GEOM made, and gives each one owner:

| owner | meaning |
|---|---|
| `driver` | a jailed driver from a rule, with whether its jail is up now (a stick's server also waits for an unlocked session) |
| `service` | nodes shown in a service's own devfs mount (Tactus, Lyra, Sigillum) |
| `kernel` | FreeBSD's own driver has it and no rule asks (buses, bridges, disks); a note says why a rule that matches did not take it (`matches rule X, but the rule does not take it from ukbd0`) |
| `nobody` | visible to no one (§8 decision 4), with why: `no driver and no rule`, `no rule matches`, `no collections reported` |

Each entry carries the name (`ukbd0`, `ugenB.A:I`, `da0s1`, `parent:location`
for an unclaimed child), the registry id (new per attach), bus, parent and
the kernel's identifying fields (`vendor=… product=… intclass=…`). The
reasons are the planner's own decisions, kept per device and dropped when it
goes.

It is served on Aqueduct class `devices` (17; crate `dispensator/devices`,
`atrium-devices`): `LIST`, and `SUBSCRIBE`, whose events carry only a
generation number (list again). The socket is
`/atrium/sockets/device-list/device-list.sock`; root, the session's user and
that user's session components may ask (as for `media`), checked on every
request and change. A jailed app reaches it only with the new restricted
capability `device-list`, meant for Curia's Devices page: hardware ids
identify the machine, so no op sends the list anywhere. Until Curia exists,
`atrium-dispensatord --devices` prints it (and `--plan … --devices` prints
the planner's list for recorded events).

Tests: `inventory` (owned, kernel, nobody, disks, a device that goes) and
`atrium-devices` over a real socket (list, change events once per change, a
session change drops the stream). Gate `vm-usb-check.sh` adds the list on
FreeBSD: 74 devices; the reader with its driver and rule, running; `uhub0`
FreeBSD's; a QEMU MTP device (class 06, no FreeBSD driver) `nobody: no
driver and no rule`, gone from the list when unplugged; a user with no
session refused: **0 failed** 2026-11-14. The run also showed one real
unclaimed device, QEMU's fw_cfg (`acpi0:\_SB_.PCI0.FWCF`).

Not yet: Curia itself (its Devices page reads this), and the Opifex
catalog lookup for a `nobody` device (§4.7, point 2).

### 6.11 As built: grants, a stick's folder in an app's jail (2026-11-14)

The mount side of §4.8:

- **One server, several mounts.** atrium-capsa-fuse takes `--control-fd 5`:
  a socket jaild hands it with the partition and `/dev/fuse`
  (`FuseServe::control`; jaild accepts exactly one descriptor there and
  checks it is a socket; a service may still not take the caller's stdio).
  dispensatord sends `mount UID GID RO PATH` with a mounted `/dev/fuse`
  descriptor; the server resolves `PATH` from the root (no `.`/`..`, a
  folder only), and serves that mount rooted there, owned by the app's uid
  and 0700 (the kernel's `default_permissions` still checks), from the same
  open volume (`server::Shared`). A remove or rename on one mount is
  followed by the others, so a grant never sees a stale node. At most 64
  grants per server; one ending ends only itself; the session's mount
  ending ends them all.
- **dispensatord** gains `GRANT {part, path, jail, uid, gid, read_only}`
  and `REVOKE {jail}` on the media socket (class `media`, ops 2 and 3),
  for root only: not the session's user, whose own mount has everything,
  and not an app. A grant needs an unlocked session, a served stick and a
  running jail; it mounts fusefs at `/media/.grants/<n>` (0711 above), hands
  the descriptor to the server, and asks jaild to nullfs it into the jail
  at `/media/<part>` (policy `rw_patterns` gains `/media/.grants/*`). It
  ends when the jail goes (checked every reshow), on `REVOKE`, eject,
  unplug, lock and logout. CLI: `atrium-dispensatord --grant PART PATH
  --jail NAME --uid U [--gid G] [--ro]`, `--revoke JAIL`.
- **The file manager** (`ATTACH_ALL`, op 4): the app asks from its own
  jail. dispensatord checks the session is unlocked, the peer's uid is in
  portcullisd's app registry as an app of the session's user, its
  installed manifest (`/var/lib/atrium/apps/<id>/atrium.toml`) holds
  `removable-files` (restricted: portcullisd installs it only with policy
  approval), and the uid's processes run in exactly one jail (`ps -x -U`,
  `jls`). It then keeps a standing grant: every served stick, whole and
  read-write (read-only where the driver is), at `/media/<part>`, as each
  comes, through the same grants as above. Locking ends the mounts and
  unlocking brings them back to the same jail; the jail going, a logout or
  another user's session ends the standing grant. CLI `--attach-all`.
  Capability `removable-files` also gives the `removable-media/` socket.
- **One file** (`OPEN`, op 5, root only, so the picker through
  portcullisd): `{part, path, write}`; dispensatord opens the path on the
  session's mount itself (no `..`, `O_NOFOLLOW`, a regular file only) and
  the reply carries the open file as its one descriptor, which portcullisd
  passes on to the app over Aqueduct. CLI `--open PART PATH [--write]`
  prints the file as read through the descriptor.
- **The picker's folders** (`PICKED`, op 6): `{part, path, app_uid,
  read_only}` from the session's picker, an app whose manifest holds the
  restricted capability `file-picker`, for another app of the same user;
  dispensatord finds that app's one jail and makes the same grant as
  `GRANT`. CLI `--picked PART PATH APP_UID` (`PICKED_RO=1`). The file
  manager (`removable-files` or `file-picker`) may also list sticks and
  eject them. The picker and file manager themselves: Scrinium
  ([scrinium.md](scrinium.md)), gate `vm-scrinium-check.sh`.

Tests: capsa-fuse `grants` (a real FUSE mount on Linux: refusals, only the
folder, owner and mode, `..`, writes seen by the session, the session's
rename followed, a grant ending alone, a read-only whole-stick grant,
fsck.fat clean after); atrium-media `grants_are_roots_and_checked`. Gate
`vm-automount-check.sh` on FreeBSD: alice may not grant; root grants
`/Docs` into a jail (uid 50601, 0700, only `note.txt`); the app reads it
and writes, alice sees the write and cannot read the grant; the whole stick
read-only into a second jail, which cannot write; the first jail removed
ends its grant; locking ends the other. The file manager: alice (not an app)
and an app without `removable-files` are refused; from its jail it gets the
stick (uid 50603, 0700), reads and writes, alice sees its file; locking
ends it, unlocking brings it back, its jail going leaves no grant. One
file: root's descriptor reads and writes; alice, a folder and `..` are
refused: **0 failed** 2026-11-14. atrium-media `open_hands_over_a_file`.

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
