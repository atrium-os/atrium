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
| Karythra CAS-FS | file-level | Identical files share storage regardless of containing package. |
| **Atrium / Tessera** | **chunk-level (CDC)** | FastCDC content-defined chunks, 64 KiB average (tessera-fs.md §6.5); descends from Karythra, ported + extended on FreeBSD. |

Chunk-level dedup subsumes file-level (identical files chunk identically) and additionally shares partial overlap: two apps shipping the same `libc.so.7` share all bytes; two shipping *slightly different* builds share the unchanged chunks. Below the chunk level, [tessera-binsplit](../spec/tessera-binsplit.md) (D1.7) adds **function-granularity** dedup of ELF binaries — 1.89× aggregate compression measured across 9 Atrium binaries under a pinned toolchain.

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

> **Updated 2026-06-10 — the diagram above is the original D1.5
> *plan* (symlink mosaic), kept for historical context. What
> shipped is a real in-kernel POSIX filesystem, `tessera_fs.ko`:**
> dual-superblock atomicity, journaled group-commit fsync,
> immutable CDC-chunked pack files + hash-keyed manifests, with
> the inode table as the only mutable layer. Full POSIX
> (pjdfstest sweep), mmap/exec, snapshots via
> `/.tessera/snapshots/<gen>/`, background repack; steady-state
> performance matches/beats ZFS on multi-write fsync. There is no
> symlink indirection and no FUSE — jails mount subtrees of one
> shared volume and see an ordinary filesystem; dedup happens in
> the CAS layer underneath. GC is **mark-sweep reachability**
> over live inodes' manifests + pinned GC roots (tessera-fs.md
> §11/§15) — no on-disk refcounts. Dedup is **per-domain policy**
> (global / deferred / salted, tessera-fs.md §20) and jails see
> quota-scoped `statfs` (tessera-quotas.md §3.6), which together
> close the dedup existence oracle. Normative specs:
> [tessera-fs.md](../spec/tessera-fs.md),
> [tessera-vfs.md](../spec/tessera-vfs.md),
> [tessera-quotas.md](../spec/tessera-quotas.md),
> [tessera-impl.md](../spec/tessera-impl.md).

Per-jail writable layer:

- Read-only base = the app's tree under `/var/lib/atrium/apps/`.
- Writable overlay per jail under `/var/lib/atrium/overlays/` (its own dedup + quota domain).
- Composed with `nullfs` + `unionfs`; all trees are subtrees of the same Tessera volume (portcullis.md §4).

## Operations

- **Install an app.** Drop a tree of files into a staging directory. `tessera import staging/ /jail/<app-id>/`. Each file is hashed; the bytes go into the Tessera store if not already there; the result mosaic is built with symlinks. Total transferred bytes = unique-only.
- **Update an app.** New mosaic replaces old. Old mosaic's symlinks unreferenced; mark-sweep GC reclaims orphan tesserae. New tesserae added.
- **Rollback.** Repoint the jail's mosaic to the previous tag. Old tesserae are still there (unless GC'd).
- **Boot a fresh app instance.** `nullfs` + `unionfs` the mosaic into a jail; jail starts; app runs.
- **GC.** Walk all mosaics; mark every tessera referenced; sweep unreferenced. Probably daily-cron, configurable.

## Implementation strategy (D1.5) — historical

> Outcome 2026-06-10: the phased plan below was overtaken — D1.5
> went **directly to the in-kernel filesystem** (Phase 3 shape,
> reimplemented from the Karythra reference rather than ported
> wholesale) per the POC priority of doing it right the first
> time. Phases 1–2 never shipped. Kept for the record of why.

Phased approach (as originally planned):

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

## Open questions (status 2026-06-10)

Resolved by the shipped specs:

- ~~**GC policy.**~~ Mark-sweep with pinned GC roots + expiry (tessera-fs.md §11/§15); `tessera repack --gc/--full/--aggressive` (tessera-vfs.md §13.6).
- ~~**Tessera size / chunking.**~~ FastCDC content-defined chunking from day one, 64 KiB average (tessera-fs.md §6.5).
- ~~**Atomicity.**~~ Manifest swap is atomic by construction; journal + dual superblock (tessera-fs.md §1, §4).
- ~~**Fragmentation.**~~ Pack files (not a flat hash directory) + free-extent map + background repack (tessera-fs.md §5, §9).
- ~~**Trust / signing.**~~ Registry-of-manifests + Sigstore transparency log ([atrium-pkg-registry.md](../spec/atrium-pkg-registry.md)).

Still open:

- **Capacity / eviction when "full".** Quotas bound per-domain use (tessera-quotas.md), but a pool-level eviction/pressure policy (which snapshots decay first, operator alerts) is unspecified beyond snapshot log-decay retention.
- **Cross-volume / at-rest encryption layering** (GELI vs blob-layer; see tessera-fs.md §17 and the §20.3 oracle constraint on convergent encryption).

## What this gives the platform

- **Disk-efficient sandboxing.** App isolation at no per-app library cost.
- **Atomic updates and rollback.** Every install is bit-reproducible from the manifest.
- **Cross-machine content** — same hash means same bytes, anywhere. Useful for cloud-desktop / VDI: a session migrates by sending the running scene's mutation stream; the destination has all the assets cached because it's accessed similar apps.
- **Verifiable provenance.** Hash chain back to vendor signing key.
- **Garbage-collected, not freed.** Leaked storage isn't a per-app crisis; the GC reclaims globally.
