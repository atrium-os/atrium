# Is virtio the right device interface for Solum?

**Date:** 2026-11-27. **Asked by:** Shankar: "is virtio the right abstraction for Solum? It
is widely used by emulators, but do a technical study. Can it be improved upon? Is there a
better way to do this by thinking outside the box?"
**Spec:** [solum.md](../spec/solum.md) §5. This study replaces its first draft's "every
abstracted class is virtio" with §5's three interface tiers.

## 1. Answer

- **virtio is the right interface for guests we don't write:** Linux in the Linux lane,
  and anything else that already ships virtio drivers. There, Atrium is the trusted side,
  which is the trust direction virtio was designed for.
- **virtio is the wrong core interface between Atrium and its own driver domains.** It
  was built so a trusted host could emulate a device to a guest. A driver domain is the
  reverse: Atrium has to distrust the device side. Five things follow (§3): the back end
  wants access to all of the front end's memory; existing front-end drivers trust the
  device; virtio's discovery and config space need exactly the trap-and-emulate that
  Solum is designed not to have; device-level interfaces keep the hostile parsing in the
  main kernel that we want to move out; and its reset story is too weak for restarting a
  crashed driver.
- **The better way:** a driver domain should not pretend to be a device. It should be a
  **service that owns hardware**, and speak the protocol Atrium's own service for that
  class already speaks: Lyra's stream rings for audio, Tactus's normalised HID events for
  input, HCI for Bluetooth, file operations for removable media, Ethernet frames for
  networks, Fresco for graphics. These go over one small Solum-native transport, the
  **link** (§5): offset-addressed shared arenas, records parsed with the hostile profile,
  sealed blobs for bulk data, and an epoch that makes crash and restart part of the
  protocol.
- **Further out of the box:** for drivers we rewrite in Rust, don't use a guest kernel at
  all. Run the driver as a jailed user-space process in the main domain and give it an
  IOMMU domain of its own that Solum programs (§6.1). That gives crash protection and DMA
  confinement at process cost. Where hardware can be split (SR-IOV, or a device that
  implements virtio rings in silicon), passthrough and a standard interface become the
  same thing (§6.2).

## 2. What virtio is, and what it assumes

virtio 1.x (OASIS) defines a device as: a transport (PCI, MMIO or channel I/O) for
discovery, feature bits and config space; one or more virtqueues (split or packed rings)
whose descriptors carry **guest-physical addresses** of buffers; notifications (a driver
"kick" writes a doorbell, the device raises an interrupt); and a device type (net, blk,
console, input, snd, gpu, vsock and so on) that fixes the record formats.

The design assumes three things about where it runs:

1. **The device side is trusted and can see the driver's memory.** A VMM maps all guest
   RAM and follows any descriptor's address. `VIRTIO_F_ACCESS_PLATFORM` and a virtual
   IOMMU were added later to restrict that.
2. **Something emulates the transport.** Config-space reads, BAR accesses and the
   doorbell write trap to the hypervisor and are handled by the device model.
3. **The device rarely disappears.** `DEVICE_NEEDS_RESET` exists, but drivers mostly
   treat a device reset as a fatal or rare event.

Those assumptions hold for a VMM in front of a guest. In a driver domain, none of them does.

## 3. Where virtio does not fit Solum's driver domains

### 3.1 The addressing model

If the back end follows guest-physical addresses, it needs Atrium's whole memory, so a
compromised Wi-Fi domain reads every persona's data. That defeats the point of the
domain. The known ways to restrict it all cost something:

| method | how | cost |
|---|---|---|
| bounce pool (swiotlb, a restricted DMA pool; what Android's pKVM guests and confidential VMs use) | descriptors may only point into one shared region; the front end copies in and out | one copy each way; simple |
| virtual IOMMU (virtio-iommu) | the front end maps each buffer before use | a hypercall plus a TLB invalidation per map/unmap; known to dominate at high rates unless mappings are cached, which gives the isolation back |
| grant tables (Xen PV) | per-page grants that the other side maps | a hypercall per grant; "persistent grants" reuse a fixed pool, which is a bounce pool again |

At the rates of the slow classes every one of these is affordable. But each is a patch
over an addressing model that was wrong for this setup from the start. A transport that
uses **offsets into a region shared with one peer** gets the bounce pool's isolation
without needing a copy wherever the producer writes straight into the region (§5.2).

### 3.2 Front ends trust the device

FreeBSD's and Linux's virtio drivers were written for a trusted host. The Linux
confidential-computing hardening work (for TDX and SEV guests) found that a hostile
device side could break a guest through used-ring lengths, descriptor indices, config
values read twice, and missing bounds checks. FreeBSD's drivers have had no such review.
Using them against a driver domain would put unreviewed C parsers of hostile input into
the main kernel: the opposite of AGENTS.md's rule that hostile parsing goes to jailed
user space. We would end up writing our own hardened front ends anyway.

### 3.3 Discovery needs emulation

virtio-pci needs PCI config space, capabilities and BARs that someone emulates.
virtio-mmio needs a trapped register page. Either way Solum would grow the device model it
was meant not to have, or forward every config access to the back-end domain, which costs
two world switches each. A link needs no discovery by trapping: the signed domain manifest
(solum.md §4.6) already names every link, and Solum hands each domain its links at boot.

### 3.4 The level is wrong for moving hostile parsing out

A device-level interface leaves everything above the device in the main kernel:

- **USB stick as virtio-blk:** the main kernel, or its FUSE servers, still parses an
  untrusted FAT, exFAT or partition table. As a file-level link, that parsing happens in
  the USB domain, next to the descriptor parsing it already does.
- **USB peripherals:** there is no virtio-usb. Atrium's user-space drivers (Vinculum,
  CCID) want USB transfers, and audio and input want streams and events.
- **Audio as virtio-snd:** the main kernel needs a virtio-snd driver plus OSS to hand the
  samples to lyrad, which already moves audio as shared-memory rings (lyra-v1.md). As a
  Lyra-protocol link, lyrad writes its mix straight into the audio domain's ring.
- **Input as virtio-input:** that is evdev-shaped. Atrium chose normalised HID events in
  Tactus and no evdev (atrium-input.md). As a Tactus-protocol link, the HID report parser
  moves into the USB domain.

### 3.5 Crash and restart

Solum restarts driver domains (solum.md §6.1). With virtio, a restart is a surprise
device reset, and each front end has to recover in its own way, in kernel code, with
requests in flight. The Fresco precedent is better: the server has an epoch, and clients
replay their shadow state when it changes (fresco-recovery.md). A transport can carry that
generically: each link has an epoch, every outstanding request fails with "epoch changed",
and each protocol states what is replayed.

### 3.6 What virtio gets right, and what we keep

- **Ring design:** packed rings, descriptor chaining, event-index notification suppression
  so a busy queue doesn't kick per buffer, and multiqueue. The link's ring borrows these.
- **Record formats where they are already the right level:** the virtio-net header
  (checksum and segmentation offload fields) is a good Ethernet-frame header. The net link
  uses it, so the net domain's back end can reuse the rust-vmm virtio-net code.
- **Guests:** Linux speaks virtio natively, so the Linux lane keeps it, with back ends in a
  jailed process in Atrium (solum.md §8).

## 4. Other interfaces surveyed

| interface | model | lesson for Solum |
|---|---|---|
| Xen PV split drivers (netfront/blkfront, grant tables, event channels) | built for distrusting driver domains | grants and event channels are the right idea; per-page grant hypercalls are the cost to avoid; the device-level split still leaves stacks in the guest |
| Hyper-V VMBus (netvsc, storvsc) | channel offers plus ring buffers in explicitly shared pages | rings in a region shared with one peer, discovery by offer rather than emulated PCI: close to the link |
| Fuchsia driver framework (FIDL over channels) | drivers are components that speak typed protocols | the interface is a protocol, not a register file; matches Atrium services |
| seL4 sDDF | shared-memory queues of offsets into data regions, one region per pair of components, with notifications | offset addressing and one region per pair: the link's transport is the same shape. Design reference: sDDF targets seL4 (kernel GPL-2); we take the design, not code |
| VFIO / user-space drivers (DPDK, SPDK) | the IOMMU confines a user process's DMA | the cheapest domain is a process (§6.1) |
| vDPA, virtio in hardware | the NIC implements virtio rings itself | passthrough with a standard interface (§6.2) |
| SR-IOV / Scalable IOV | the hardware gives each domain its own function | per-domain passthrough with hardware isolation (§6.2) |

## 5. The link: Solum's native transport

### 5.1 Shape

- A **link** joins exactly two domains, or a domain and a driver process (§6.1). The
  manifest declares it with its protocol class (an Aqueduct class id), the arena size, and
  which side may restart the other.
- The **arena** is a fixed set of pages shared by those two sides only, mapped at boot or
  when the link is made. It is never remapped while the link lives. It holds:
  - two rings (requests and completions) of fixed-size records, packed-ring style, with
    event-index suppression;
  - a data area that records point into by **offset and length**, checked against the
    arena's bounds on every use; neither side ever sends a physical address.
  - every record is **copied out of the ring once** into private memory before any field
    is checked or used, so offset and length cannot change between the bounds check and
    the access (no double fetch).
- **Notification** is a Solum doorbell hypercall that injects a virtual interrupt into the
  peer's vCPU. With suppression, a busy link costs about one kick per batch. Under load a
  side may poll for a bounded time before re-enabling kicks.
- The **epoch** is a counter in the arena header that Solum owns. Solum increments it when
  either side restarts, and everything in flight under the old epoch fails. Each protocol
  defines what is replayed: Lyra re-opens streams, Tactus re-sends device state, the file
  link re-opens handles by path, the net link just drops frames.

### 5.2 Bulk data without copies or double fetches

Shared memory that both sides can write invites time-of-check-to-time-of-use bugs: the
writer changes a buffer after the reader has validated it. The link offers three ways to
move data, and each protocol picks per message:

1. **Copy** for small records and frames. The receiver copies out before parsing.
   Network receive does this, since the kernel builds mbufs anyway.
2. **Produce in place, then seal.** The producer writes into arena pages, then asks Solum
   to **seal** them: Solum makes them read-only to both sides until the consumer releases
   them. That is one stage-2 permission change per batch of pages, worth it from tens of
   KiB upwards (an audio period batch, a file read). It is the content-addressed idea from
   Aqueduct (immutable payloads) enforced by the hypervisor rather than checked by
   hashing.
   For the seal to mean anything:
   - Every record also carries the link's epoch (§5.1); an arena's pages are not reused
     under a new epoch until the device that could DMA into them has been reset.
   - Solum removes write access to the sealed pages **everywhere it exists**: the producer's
     stage-2, every IOMMU mapping of a device the producer owns, and, when the producer is
     main, main's static IOMMU domain (solum.md §5.3 item 8), followed by an IOTLB invalidation and
     its completion wait, before the seal is reported done. Otherwise the producer's
     device could still DMA into a "sealed" page.
   - The consumer trusts only seal state that Solum owns (a per-page state in Solum's
     table, which the consumer reads by hypercall or from a read-only status page Solum
     writes), never a flag in the producer's ring record.
   - Release is bounded: a seal carries a deadline set per link in the manifest. If the
     consumer hasn't released by then, Solum force-unseals the pages back to the producer
     and revokes the consumer's read mapping first. A consumer that never releases can't
     starve the producer's arena.
3. **Donate.** Whole pages move from one domain to the other (Solum changes the owner and
   invalidates the TLB). Use it for large one-way transfers such as firmware images or big
   file reads.

Which of these wins at which size is measured in S1 (solum.md §10) and not assumed.

### 5.3 Protocols per class

| class | link protocol | consumer in the main domain | parser that moves out of the main kernel |
|---|---|---|---|
| Wi-Fi, other NICs in a domain | Ethernet frames with the virtio-net header | a small `solum-link` ifnet driver; pf, vnet and jails unchanged | 802.11 management, firmware (aura-mlme stays jailed in the domain) |
| USB storage | file operations (the protocol Capsa's FUSE servers already serve) | jaild/Capsa mount it as today, from a link instead of a local FUSE server | partition tables, FAT, exFAT, descriptors |
| USB HID | Tactus's normalised events | tactusd | HID report descriptors |
| USB audio, HDA | Lyra's stream rings and clock | lyrad | codec and USB audio class |
| Bluetooth | HCI (the Bluetooth standard interface) | Vinculum's transport layer | USB transfer layer and the firmware loader |
| other USB devices | USB transfers for one device (a usbredir-like protocol) | the jailed user-space driver (CCID, and so on) | descriptors and the xHCI driver |
| cameras | a frame stream: rings of sealed frame buffers with format, stride and a timestamp on the shared clock, plus control records (mode, exposure, focus) | the app holding a camera grant, through a jailed decoder for MJPEG | UVC descriptors and payload headers |
| graphics (graphics domain) | Fresco | apps, unchanged | the GPU driver |

The main kernel needs only one new driver, the link itself (Rust; its only parsing is of
fixed-size ring records), plus the ifnet glue for frame links. Everything else ends in a
user-space service that already exists.

### 5.4 The same protocols without Solum

A machine without Solum (solum.md §4.7) runs the same driver service as a jailed process
next to its kernel driver, and serves the same protocol over a Unix socket and shared
memory. lyrad, tactusd, Vinculum and Capsa then see one interface either way. That is the
"standardised interface" Shankar asked for: standard across hardware, across machines with
and without Solum, and, as Fenestra shows for Fresco, across a network.

## 6. Thinking further out of the box

### 6.1 Driver processes instead of driver kernels

A driver domain carries a whole FreeBSD kernel so that the C drivers we already have can
run there. For a driver we write in Rust (the planned direction, D2), the kernel adds
nothing but size. Solum can instead offer a **driver process**: a jailed, unprivileged
user-space process in the main domain that

- gets the device's BARs mapped into its address space, except pages that mirror config
  space or hold the MSI-X table or PBA, which stay with Solum,
- gets an IOMMU domain of its own, which Solum programs. Its DMA can reach only pages that
  process has pinned. That protects the rest of main's memory from the **device and its
  driver process**; it does nothing against the main kernel, which chose those pages and
  can change what is behind them,
- gets the device's interrupts as events on a descriptor, and
- serves its class protocol over a link, or over a socket.

A crash is a process restart: milliseconds instead of a guest kernel's boot, and no
memory reserved for a guest. **The isolation is clearly weaker than a domain's:** a
compromised driver process sits on the main kernel's full syscall surface (jailed and
filtered by jail-sysfil.md, but that is the same surface any app has), so a kernel bug
reached from it owns the main domain; and the main kernel controls the process's memory.
What it does give: a device or driver crash costs a process restart, the device's DMA is
confined, and no C kernel sits under the driver. Use it for crash protection and DMA
confinement of trusted-enough Rust drivers, not as a security boundary against hostile
input that a domain should parse. This is the microkernel answer (the seL4 and Fuchsia lesson) without a
microkernel, and on Atrium it extends what dispensatord already does for USB, HID and
serial to PCI devices. FreeBSD has no IOMMU-backed DMA for user processes outside bhyve's
`ppt` (device-assignment.md §3.2); with Solum owning the IOMMU, Solum supplies it.

Plan: driver domains with a FreeBSD kernel for the C drivers we keep (xHCI, HDA, at
first), and driver processes for Rust drivers as they are written. Both speak the same
link protocols, so the main domain's services need no code for either case. This is an
engineering convenience, not a security claim: the two give different protection (above).

### 6.2 Let the hardware be the interface

- **SR-IOV and Scalable IOV:** NICs, and NVMe drives that support it, can expose one
  virtual function per domain. Solum passes the VF through with solum.md §5.3's safeguards
  and gets hardware isolation at native speed. Consumer AMD GPUs don't offer SR-IOV, so the
  R9700 stays a whole-device passthrough.
- **virtio in silicon (vDPA):** a NIC that implements virtio rings is passthrough and a
  standard interface at once. That is useful for the Linux lane, but not a reason to use
  virtio between Atrium's own domains.

### 6.3 Rejected

- **A socket-level or file-system-level network link** (the net domain runs TCP/IP):
  this moves the main domain's pf, vnet and per-app network stacks out of the main kernel,
  which breaks the jail model. Frames stay the level.
- **Block-level links for the main disk:** NVMe is passed through (solum.md §5.1). A
  storage domain, if ever wanted, would use a block link with sealed buffers, not
  virtio-blk.
- **Shared memory with no hypervisor help** (both sides write freely, and checks are done
  carefully): this is exactly the double-fetch bug class that sealing removes.

## 7. Recommendation

1. Keep virtio for guests we don't write (the Linux lane), with back ends jailed in the
   main domain.
2. Between Atrium and its own driver domains, use **links**: offset-addressed arenas, rings
   modelled on virtio's packed ring, an epoch for restarts, and copy, seal or donate for
   bulk data, carrying **Atrium's own class protocols** (§5.3). Reuse the virtio-net header
   for frames.
3. Add **driver processes** (§6.1) as the lighter kind of domain for Rust drivers, using
   the same protocols.
4. Pass through **SR-IOV functions** where hardware offers them.
5. Measure copy, seal and donate, and kick latency, in S1, before fixing the thresholds.
