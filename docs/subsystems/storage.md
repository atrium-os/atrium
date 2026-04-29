# Subsystem — Storage (Tessera)

> See [NAMING.md](../NAMING.md) for component naming. Tessera is Atrium's content-addressed filesystem; each blob is a "tessera" (mosaic tile), each jail tree is a "mosaic".

## Thesis

Per-app filesystem isolation is **free** if files are content-addressed.

Today, jailing N apps each with their own libc + Qt + glibc-equivalent + framework set costs N× disk. With Tessera, the cost is `1× shared bytes + Σ(unique bytes per app)`. For a desktop with 30 apps that share most of their dependencies, that's roughly the disk cost of one app.

This is the property that makes "every app is its own jail with its own complete library tree" practical. Without dedup, the model is too expensive. With dedup at the file level, it's the right model.

## Comparison

| System | Granularity | Notes |
|---|---|---|
| OSTree (Fedora Silverblue, GNOME Builder) | chunk-level (rsync-style) | Partial dedup; works on full filesystem trees. |
| Linux flatpak runtimes | runtime-level | Apps share a "runtime" (e.g. `org.freedesktop.Platform/22.08`). Coarse. |
| snap | per-snap | No dedup across snaps. |
| Nix store | derivation-level (~package) | Whole packages are addressed; sharing at package boundary, not file. |
| Guix store | same as Nix | |
| Karythra CAS-FS | **file-level** | Identical files share storage regardless of containing package. |
| **Atrium / Tessera** | **file-level** | Same. Sourced from Karythra, ported to FreeBSD. |

File-level dedup is the right granularity for jail-per-app distribution: two apps shipping the same `libc.so.7` from the same FreeBSD release share the bytes; two apps shipping different `libc.so.7` builds (e.g. one with debug symbols) get different blobs and use proportional space.

## Architecture

```
┌─ Tessera store (host filesystem, pinned) ────────────────────┐
│                                                                │
│   /var/lib/tessera/cas/                                        │
│      ab/cd/abcd1234...     (SHA-256 chunked into directories)  │
│      ef/01/ef019876...                                         │
│      ...                                                       │
│                                                                │
│   Each entry is the raw file bytes (one "tessera"). No metadata. │
│   Reference counted (mark-sweep GC).                           │
│                                                                │
└────────────────────────────────────────────────────────────────┘
                            ▲
                            │ symlinks / nullfs / FUSE / in-kernel
                            │
┌─ Per-jail mosaic (sees a normal filesystem) ─────────────────┐
│                                                                │
│   /jail/atrium-edit-1.2.3/                                     │
│      bin/atrium-edit -> /tessera/ab/cd/abcd...                 │
│      lib/libc.so.7   -> /tessera/12/34/12340bee...             │
│      lib/libssl.so.3 -> /tessera/56/78/56785678...             │
│      share/fonts/                                              │
│        DejaVu.ttf    -> /tessera/9a/bc/9abcdef...              │
│      etc/...                                                   │
│                                                                │
│   Inside the jail, paths look normal. Symlinks resolve into    │
│   the Tessera store. Reads come from the shared bytes.         │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

Per-jail writable layer:

- Read-only base = CAS-backed tree.
- Writable overlay (per-jail, on disk) for user-mutable paths: `~/Documents`, `~/.config/<app>`, etc.
- Modeled with `nullfs` + `unionfs` in FreeBSD; each layer is its own mount.

## Operations

- **Install an app.** Drop a tree of files into a staging directory. `tessera import staging/ /jail/<app-id>/`. Each file is hashed; the bytes go into the Tessera store if not already there; the result mosaic is built with symlinks. Total transferred bytes = unique-only.
- **Update an app.** New mosaic replaces old. Old mosaic's symlinks unreferenced; mark-sweep GC reclaims orphan tesserae. New tesserae added.
- **Rollback.** Repoint the jail's mosaic to the previous tag. Old tesserae are still there (unless GC'd).
- **Boot a fresh app instance.** `nullfs` + `unionfs` the mosaic into a jail; jail starts; app runs.
- **GC.** Walk all mosaics; mark every tessera referenced; sweep unreferenced. Probably daily-cron, configurable.

## Implementation strategy (D1.5)

Phased approach:

### Phase 1 — Symlink-based store in userspace (1 month)

- Pure shell + Rust tooling. No kernel changes.
- `tessera import <dir>`: hashes every file, copies to `/var/lib/tessera/cas/<hash>`, creates symlink mosaic.
- Jails mount the mosaic via `nullfs`.
- Already gives the dedup property. Slower reads (extra symlink resolution) but functional.
- Validates the model.

### Phase 2 — FUSE-based Tessera (1.5 months)

- FreeBSD has FUSE support. Write a `tessera-fuse` userspace driver.
- Reads go directly to store paths (or even to an in-memory cache of tesserae).
- Writes go to a per-jail overlay.
- Lookup overhead lower than symlink resolution.

### Phase 3 — In-kernel Tessera (port from Karythra, 3+ months)

- Karythra has a CAS-FS already. Port the kernel module.
- In-kernel hash lookup, no FUSE roundtrip.
- Performance parity with native FS for hot paths.
- Required for production-scale loads.

We don't need to do all three to demo the property. Phase 1 is sufficient for the desktop POC; Phase 3 is required for production.

## Distribution

App = a Tessera-importable mosaic.

- Vendor publishes a manifest (signed): app name, version, root-mosaic hash.
- Opifex (the package manager) fetches: ask the registry for the manifest; recursively fetch any missing tesserae.
- Verify: each tessera hashes to its expected SHA-256. Tampering detected automatically.
- Install: build the symlink mosaic from the manifest.

This is Nix-shaped without the overlay-language baggage. It's also Git-shaped without the Git-specific tooling. It's also OSTree-shaped at finer granularity.

Distribution channels:

- **Local** — `pkg`-equivalent against a remote registry.
- **Federated** — multiple registries (org, vendor, user). Per-jail manifest can pin a registry.
- **Decentralized** — anyone with a hash can serve the bytes; integrity follows from hash. (Long-term option.)

## Integration with other subsystems

- **Sandbox (Portcullis).** Each jail's filesystem is a Tessera-backed mosaic + writable overlay. See [sandbox.md](sandbox.md).
- **Graphics.** Fresco's content-addressed in-memory store and Tessera share the SHA-256 abstraction. A texture uploaded by app A and stored on disk can be referenced by hash in protocol traffic without re-uploading. Cross-process, cross-machine: same property.
- **Updates.** Atomic; old tree stays on disk until GC. Failed update = old tree still resolves. Rollback = repoint symlink.

## Open questions

- **GC policy.** When to reclaim unreferenced tesserae? Conservative (24h after last reference)? Or daily-cron?
- **Capacity.** A 200 GB Tessera store can hold a lot, but eventually fills up. What's the eviction policy when "full"? LRU on access time?
- **Tessera size.** Hashing per-file works for normal sizes (< MB). Multi-GB files (game assets, ML models) need chunking. Defer; not v1 concern.
- **Atomicity.** Mosaic builds need to be atomic w.r.t. the jail's view. Probably stage in a shadow mosaic, atomic rename.
- **Fragmentation.** The store is a flat directory of hashes. Lots of small files. UFS handles it OK; ZFS even better. Worth measuring.
- **Trust.** The store's bytes are bit-perfect; tampering changes hashes. But the registry's manifest signing is a separate concern (Opifex). PKI design is a follow-on.

## What this gives the platform

- **Disk-efficient sandboxing.** App isolation at no per-app library cost.
- **Atomic updates and rollback.** Every install is bit-reproducible from the manifest.
- **Cross-machine content** — same hash means same bytes, anywhere. Useful for cloud-desktop / VDI: a session migrates by sending the running scene's mutation stream; the destination has all the assets cached because it's accessed similar apps.
- **Verifiable provenance.** Hash chain back to vendor signing key.
- **Garbage-collected, not freed.** Leaked storage isn't a per-app crisis; the GC reclaims globally.
