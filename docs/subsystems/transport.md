# Subsystem — Transport

> See [NAMING.md](../NAMING.md) for component naming.

## Thesis

The Fresco protocol is **transport-agnostic**. The wire format (commands, completions, input events, content-addressed blobs) is a layered shape that runs over any reliable bidirectional channel. We've shipped one transport (ivshmem); the architecture supports several more without protocol changes.

This matters because the same software stack covers four scenarios that are normally siloed:

- **Local/native** — app and server on the same FreeBSD machine.
- **VM/QEMU** — guest app talks to host server.
- **Remote desktop** — app and server on different machines, over IP.
- **Future hardware** — a scenegraph-aware GPU that consumes mutation streams directly.

In each, the protocol is identical — only the channel differs.

## Today's transport — ivshmem (QEMU)

```
┌─ FreeBSD guest ─────────────┐         ┌─ macOS host ─────────────┐
│  atrium-edit                │         │  fresco-server           │
│   ↓ /dev/fresco0            │         │                          │
│  fresco.ko (kmod)           │         │                          │
│   ↓ MMIO BAR2 (mapped)      │ shmem   │   ↓ mmap                 │
│  ivshmem PCI device         │ region  │   ivshmem-server (QEMU)  │
│   ↓ doorbell (BAR0+0x0c)    │  ◄──►   │                          │
└─────────────────────────────┘         └──────────────────────────┘
```

- 32 MiB shared-memory region, fixed layout (see [graphics.md](graphics.md)).
- One kmod per guest providing `/dev/fresco0`.
- Per-slot ring slices for cmd, comp, input.
- Doorbell via PCI register write.
- Wakeup via 1 ms kernel callout polling head pointers (MSI-X is broken on FreeBSD/aarch64 + QEMU/HVF; polling is the workaround).

This transport is the development environment. It's also viable for any QEMU/KVM/Hyper-V/VirtualBox/Xen guest scenario.

## Native local transport — host cdev (D1)

When the server runs natively on FreeBSD on real hardware, the transport simplifies:

```
┌─ FreeBSD host ─────────────────────────┐
│  atrium-edit (jailed by Portcullis)    │
│   ↓ /dev/fresco0                       │
│  fresco.ko (kmod)                      │
│   ↕  mmap'd ring buffers in kernel mem │
│  fresco-server (also reads/writes     │
│   the same ring memory)               │
│  Doorbell via kqueue / pipe(2)        │
└────────────────────────────────────────┘
```

- Same ring layout as ivshmem (per-slot cmd/comp/input, slot allocator).
- Memory is kernel-allocated (or anon-shmem); both the kmod (on guest's behalf) and the server (privileged userspace) mmap it.
- Doorbell is a kqueue notification — server kqueues the cdev, kmod KNOTEs on ring activity.
- No QEMU; no PCI MMIO. Pure FreeBSD primitives.

Same wire format. Same kmod logic with a small swap: instead of the ivshmem PCI BAR, the kmod allocates anonymous shared memory and exposes it via mmap; the server `open()`s a privileged peer-cdev (`/dev/fresco-srv0`) to access the same backing.

## Network transport — TCP / QUIC (D6+)

Same wire format over a socket. Per-client one TCP connection (or QUIC stream); cmd/comp interleaved; input multiplexed.

```
┌─ Client (FreeBSD or anywhere) ──┐    ┌─ Server (FreeBSD desktop) ──┐
│  atrium-edit                    │    │  fresco-server              │
│   ↓ libfresco                   │    │                             │
│   TCP socket / QUIC stream      ├────┤   listening on tcp:5523     │
│                                 │    │                             │
└─────────────────────────────────┘    └─────────────────────────────┘
```

Fresco is an **especially good fit for remote desktop** because of two properties:

1. **Content-addressed**: first frame uploads atlases / icons; later frames reference by hash. Bandwidth scales with mutations, not pixels.
2. **Retained-mode**: a static UI sends ~0 bytes per frame.

A typical remote-desktop session:

- First frame: 5–10 MB (font atlas + icons + wallpaper + initial scene).
- Steady state typing: 50–200 bytes per keystroke.
- Steady state idle: 0 bytes.
- Resize: ~50–200 bytes per cursor tick (geometry update only).
- Open a new window: 1–5 KB (decorations + window state) plus app's content (size depends on app).

Compare RDP/VNC: every frame is full-pixel-buffer compression, often 10–100 KB even for "nothing changed."

Practical considerations:

- **Protocol layer**: cmd/comp on TCP (in-order, reliable). Input on UDP (low latency, drop-tolerant) or QUIC (one stream).
- **Encryption**: tunnel, don't embed — SSH channel / WireGuard / QUIC+TLS carry the crypto; the protocol stays plaintext inside ([aqueduct.md §7.2](../spec/aqueduct.md)).
- **Authentication**: the aqueduct remote-session handoff ([../spec/aqueduct-remote.md](../spec/aqueduct-remote.md)) — one-time SSH userauth mints a session credential with a capability set (full session by default; view-only / single-app restricted mints for weak endpoints). A remote session is the authenticated user's session trust domain, never more trusted than a local jailed app.
- **Bandwidth**: tens of KB/s steady state. Modems are fine.
- **Latency**: dominated by RTT. The protocol adds nanoseconds.
- **Backpressure**: ring queues + TCP backpressure. Server applies flow control.

Notable: this is the **same protocol** apps already use. No client-side rewrite. A Fresco app today using ivshmem can run unmodified over a TCP transport just by changing what cdev / socket it opens.

### Implementation strategy

1. Add a `Transport` trait in libfresco. Today's ivshmem and tomorrow's tcp are both impls.
2. `fresco_open_url("fresco://192.168.1.5:5523")` chooses the TCP transport.
3. Server's existing `net_link` stub gets fleshed out into a real listener.
4. Per-connection state mirrors per-slot state in the ivshmem world. Slot allocation is per-connection.

## Future hardware — direct scenegraph submission

Scenegraph-aware GPU hardware:

- The "transport" is the GPU's command buffer.
- Server submits scene mutations via a hardware queue.
- GPU has a "scenegraph processor" that ingests mutations and produces draws.

Speculative but the protocol is **already shaped for it**. Tile-based deferred renderers (Mali, Adreno, Apple GPUs) already do something close internally — split scene into tiles, retained per-tile primitive lists, deferred shading. Exposing that retained structure to userspace via a Fresco-shaped command queue is a small architectural step from there.

Not a 2026 problem. But if you're a GPU vendor designing a 2030 architecture, this is the ABI that'd save you the effort of supporting Vulkan/D3D/Metal as separate fronts.

## Trust boundaries by transport

| Transport | Mutual trust | Encryption | Replay | Tampering |
|---|---|---|---|---|
| ivshmem (QEMU) | guest trusts host | none (memory) | n/a | impossible (memory) |
| Native local cdev | both | none (memory) | n/a | impossible |
| TCP/TLS | mutual auth required | TLS | sequence numbers | TLS detects |
| QUIC | mutual auth required | built-in | built-in | detected |
| Future hw | transitive (CPU trusts GPU) | n/a | n/a | n/a |

Content-addressed blobs give **integrity for free**: a man-in-the-middle who substitutes a blob's bytes can't keep the SHA-256 hash consistent. The receiver re-hashes and rejects. So even on an unencrypted transport, the *content* is authenticated by hash.

## Multi-transport coexistence

A single fresco-server can serve clients on multiple transports concurrently:

```
fresco-server
  ├─ /dev/fresco-local0  (jailed local apps via cdev)
  ├─ vsock/qemu          (any QEMU/Firecracker guest)
  ├─ tcp:5523            (remote-desktop clients over TLS)
  └─ unix:/run/fresco/admin  (admin tooling)
```

Each transport has its own slot pool. Clients on different transports can't see each other's slots; the per-slot isolation property holds across transports.

This is what makes "VDI / cloud desktop / local desktop / VM-host" all the same product:

- **Cloud desktop**: server is at a datacenter. Users connect over TCP.
- **VDI**: same, intra-corp.
- **Local desktop**: server is on the user's machine. Apps are local jails.
- **Hybrid**: some apps local, some streamed from a remote server (e.g. CAD on a workstation, browsing locally).

All the same architecture.

## Open design questions

- **Discovery.** How does a client find a remote Fresco server? mDNS, manual URL, directory service? Probably all three, depending on context.
- **Migration.** Can a session move between transports mid-flight (suspend-resume from local to remote)? Architecturally yes — the scene is content-addressed; the destination either has the blobs cached or fetches them. Not a v1 feature.
- **Multi-server federation.** Can one window be "hosted" on one server and another on a different one (analogous to X displays)? Speculative; needs more thought.
- **Caching layers.** A "Fresco proxy" between client and server can cache CAS blobs to amortize repeated uploads — useful for VDI farms where one user's atlas should serve many. Future.
- **Compression.** Mutation deltas are small; compression overhead might exceed savings. Bulk uploads (textures, audio) probably want zstd. Per-msg or per-stream? TBD.

## Cross-references

- [graphics.md](graphics.md) — wire-format details.
- [storage.md](storage.md) — content-addressed blobs are the same hash space Tessera uses.
- [sandbox.md](sandbox.md) — Portcullis's capability boundary affects which transports a jail can use.
