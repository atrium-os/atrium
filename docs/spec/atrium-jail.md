# `atrium-jail` — jail snapshot, export, import

**Status:** spec, 2026-05-08
**Owner:** D2.5 packaging track
**Companion to:**
- `docs/spec/atrium-pkg.md` — registry-distributed package format
- `docs/spec/atrium-pkg-registry.md` — registry, signing, attestation
- `docs/spec/atrium-devjail.md` — per-project dev environments
- `docs/spec/portcullis.md` — jail manifests + capabilities

How a working Atrium jail moves between machines without going
through the public registry. Aimed at developer-to-developer
exchange, sneakernet / airgapped distribution, and "send my
colleague the exact thing that works on my machine." Designed so
the wire is content-addressed — a 1 MB code change ships as
~1 MB, not as a re-tarred 1 GB rootfs.

This is the **third** distribution path in the Atrium
ecosystem. The other two (source-build and binary-install) flow
through the registry; this one is direct-transfer between
machines that already trust each other.

## 1. Principle

> **A jail snapshot is a manifest plus the set of Tessera CAS
> blobs that compose its rootfs and persistent volumes.
> Distribution is by hash: the recipient fetches only the blobs
> they don't already have. The same wire format works for HTTP
> pull, peer-to-peer, sneakernet, and registry publication —
> they differ only in transport.**

What this is *not*:

- Not a `tar | xz | scp` of the entire rootfs every time.
  Compressed archives destroy block-level similarity; a 1 MB
  change in a 1 GB jail would re-transfer ~1 GB. Unacceptable.
- Not a separate format from registry packages at the wire
  level. Both ship CAS blobs + a manifest; only the manifest
  shape and trust model differ.
- Not a docker-style layered image format. Atrium has Tessera
  CAS as the universal substrate — there is no need for a
  separate "image layers" concept on top of a content-
  addressed filesystem.

## 2. Two artifact forms

| Form | Use | Wire shape |
|---|---|---|
| **Online** | "Send me your jail" between machines on a network | Manifest + missing-blob fetch over HTTP |
| **Offline** | Sneakernet, airgap, backup, archival | Single `.atrium-jail.tar.zst` file containing manifest + all referenced blobs |

The offline form is a **packaging** of the online form — same
manifest schema, just bundled into one file with all blobs the
recipient might need (since you can't fetch over the wire). The
client treats them identically after the bytes arrive: parse
manifest, ingest any missing blobs into local Tessera CAS,
materialize the jail.

## 3. Snapshot manifest

A jail snapshot is described by a manifest (TOML), referenced by
its own content hash:

```toml
schema = "atrium-jail/1"

[snapshot]
id = "blake3:7c4f8e2b..."           # hash of this manifest's canonical form
created_at = "2026-05-08T14:23:00Z"
hostname = "alice-laptop"             # informational; not trusted
atrium_release = "0.5.0"

# Optional: this snapshot is an incremental update of a prior one.
# Recipient who has the parent already only fetches blobs not in parent.
[parent]
id = "blake3:3a1b9c8d..."
url_hint = "https://example.com/snapshots/3a1b9c8d.atrium-jail"

[origin]
# What this jail was for. Informational. Not enforced; the importing
# user decides what to call it locally.
suggested_name = "vscode-myproject"
description = "Development environment for myproject, Rust 1.86 toolchain"

# The portcullis manifest that should be applied at import time.
# Same schema as docs/spec/portcullis.md.
[manifest]
# ... full manifest content inline ...

# Tessera CAS roots that compose the jail.
[rootfs]
hash = "blake3:9f2e7a1b..."           # root hash of the rootfs tree

[[volumes]]
name = "home"
hash = "blake3:5d8c4f3a..."

[[volumes]]
name = "scratch"
hash = "blake3:c12d4e5f..."

# All CAS blobs referenced by rootfs + volumes, with size for
# planning + an optional URL hint per blob. Recipient deduplicates
# against local CAS before fetching.
[[blobs]]
hash = "blake3:9f2e7a1b..."
size = 4096
url_hint = "https://example.com/blobs/9f2e7a1b"

[[blobs]]
hash = "blake3:abc123..."
size = 1048576
# ... etc, potentially thousands of entries

# Signing — same Sigstore mechanism as registry packages.
[signature]
sigstore_rekor_uuid = "..."
publisher_identity = "..."        # who exported this snapshot
```

The manifest's own `id` is the hash of its canonical TOML form.
Two machines that produce byte-identical manifests have the same
snapshot id, which gives us natural caching and dedup at the
manifest level too.

## 4. Export flow

`atrium-jail export <jail-name> [--parent <snapshot-id>] [--output <path>]`:

1. Verify the jail is **shut down** (no live mounts, no running
   processes). Refuse to export a running jail to avoid
   capturing inconsistent state. (Future: live-snapshot via
   Tessera snapshot-gen, but v1 requires stopped.)
2. Walk the jail's rootfs + persistent volumes. For each
   subtree, compute its Tessera CAS root hash (already
   maintained by the filesystem — this is essentially free).
3. Walk the CAS to enumerate all reachable blobs (chunks +
   manifest nodes). Build the `[[blobs]]` list with sizes.
4. **If `--parent` given**: load the parent manifest, subtract
   its blob set from the current blob set. The result is the
   *delta blob list*. A 1 MB change typically yields a few-KB
   manifest + a handful of changed CAS blobs.
5. Construct the manifest (§3), write it, hash it, sign it via
   Sigstore (same flow as `atrium-pkg publish`).
6. Output:
   - **Online form** (default): write the manifest to
     `<output>.json` (or print URL after upload to a
     user-supplied bucket / their own server / a peer).
     Blobs stay in local CAS; recipient fetches them by hash.
   - **Offline form** (`--bundle`): pack manifest + all
     referenced blobs (or just delta blobs if `--parent`) into
     a single `.atrium-jail.tar.zst`. Use zstd, not xz —
     zstd preserves block boundaries better and decompresses
     much faster, and we don't need the last few % of
     compression ratio because dedup happens at the CAS level
     anyway.

Typical outputs (rough order-of-magnitude):

| Scenario | Online manifest | Online delta blobs | Offline bundle |
|---|---|---|---|
| Full snapshot, fresh jail (1 GB rootfs) | ~50 KB | ~1 GB | ~600 MB |
| 1 MB change relative to parent | ~50 KB | ~2 MB | ~2 MB |
| Same snapshot already on recipient | ~50 KB | 0 B | ~50 KB |

The "1 MB change ships as ~2 MB" gap exists because Tessera
chunks at variable boundaries (typical 64 KB–1 MB) — a single
edited file usually touches 1–4 chunks, plus a handful of
manifest tree nodes. Chunk granularity is tunable via
`tessera.chunk_size` mount option for workloads that benefit.

## 5. Import flow

`atrium-jail import <source> [--name <local-name>]`:

1. Resolve `<source>` to a manifest:
   - URL → HTTP GET, save manifest, verify hash matches the
     URL's claimed snapshot id.
   - Path to `.atrium-jail.tar.zst` → unpack to a temp dir,
     read manifest from there.
   - Path to `.json` manifest directly → use as-is.
2. **Verify Sigstore signature** if present. (For dev-to-dev
   exchange, signature is encouraged but not required — the
   user is making a direct trust decision. The CLI shows the
   publisher identity prominently if signed, "unsigned" if
   not.)
3. **Show the user**:
   - Origin (hostname, suggested name, description)
   - Capability declarations from the embedded manifest
   - Blob set size: total + delta-not-already-on-this-machine
   - Signature status
   - "Continue / cancel" prompt
4. **Fetch missing blobs.** Walk `[[blobs]]`, query local
   Tessera CAS for each hash, fetch the missing ones:
   - Bundle source → unpack from the bundle
   - URL source → HTTP GET each missing blob from
     `url_hint` (or fall back to peer cache / DHT per
     `atrium-pkg-registry.md` §9)
   - Verify each blob hash on arrival; mismatch = abort.
5. **Materialize the jail** as a new portcullisd-managed jail:
   - Allocate a local jail name (default: `suggested_name`,
     prompt on conflict).
   - Synthesize the portcullis manifest from `[manifest]`
     embedded in the snapshot (with the new local jail name).
   - Point `rootfs` and `[[volumes]]` at the imported CAS
     hashes (Tessera mounts content-addressed roots
     directly — no copy required, the jail's filesystem is
     just a view onto the CAS).
   - Apply capability declarations exactly as specified.
6. **Do not auto-start.** User runs `portcullis start <name>`
   when ready; same as any other jail.

Step 5 is the punchline of using Tessera as substrate:
materializing a 1 GB jail is "mount a content hash" — milliseconds,
not seconds. The fetch is the only slow phase, and it scales
with the *delta*, not the full size.

## 6. Wire-level dedup story

Three dedup levels stack:

1. **Local CAS dedup** (free, automatic). Any blob already in
   recipient's Tessera CAS — from a previously imported jail,
   from a registry-installed package, from another snapshot of
   the same source — is never fetched. Two jails sharing a
   `glibc` blob means importing the second is "size of the
   second jail minus glibc."
2. **Parent-snapshot delta** (`--parent` at export time). When
   exporting an updated version of a jail the recipient already
   has the parent of, ship only the changed blobs. Reduces the
   manifest's `[[blobs]]` list to just the delta.
3. **Peer cache** (per `atrium-pkg-registry.md` §9). LAN-
   discovered peers can serve missing blobs by hash; same
   content-addressed safety as registry-distributed packages.

The result: in the steady state of a developer pushing
incremental updates of a dev-jail to a colleague, transfers are
"diff size" not "snapshot size." Even the *first* transfer
benefits from local CAS dedup against any base packages the
recipient has already installed from the registry, which often
covers half of any nontrivial dev-jail (toolchains, runtime
libraries, etc.).

## 7. Trust model

Snapshot import is a **direct trust decision** between two
machines. There is no registry curation, no bot check, no
typosquat check — the user is choosing to import based on who
they got the snapshot from.

What the system enforces:

- **Capability declarations are surfaced and gated.** The
  embedded manifest's capability requests trigger the same
  install-time prompt as any registry package. The user must
  approve. A snapshot cannot smuggle in elevated capabilities.
- **Content addressing prevents tampering.** The recipient's
  blob hash check (step 4 above) means a man-in-the-middle on
  the URL or the bundle cannot substitute different content.
- **Sigstore signature (optional but recommended).** If the
  exporter signed, the recipient sees the verified identity
  ("alice@example.com via github.com/alice/dev-jails"). If not,
  the recipient sees "unsigned, source: <URL or filename>".

What the system does *not* enforce:

- **Snapshot author identity.** If alice signed and bob hosts
  the URL, the recipient sees alice's identity even though
  they fetched from bob. The signature attests the content,
  not the channel. (Same as registry packages.)
- **Snapshot legitimacy.** The user is responsible for
  deciding whether to trust alice's snapshot in the first
  place. Atrium's job is to make the trust decision visible
  and informed; the decision itself is human.

## 8. Volumes and persistent data

A snapshot can include or exclude persistent volumes:

```bash
atrium-jail export myproject --volumes=all
atrium-jail export myproject --volumes=none           # rootfs only
atrium-jail export myproject --volumes=home,scratch   # specific
atrium-jail export myproject --volumes=home --exclude-volume=scratch
```

Default is `--volumes=all` for full reproducibility. The
exporter's responsibility to remember to exclude scratch /
caches / secrets when sharing.

**Critical:** the export tool refuses by default to include
volumes containing files matching common secret patterns
(`.env`, `id_rsa*`, `*.pem`, `.aws/credentials`, etc.) and
prompts the user to confirm or exclude. This is a UX guardrail
against accidental secret exfil through a casual snapshot share.
`--unsafe-include-secrets` overrides for users who know what
they're doing.

## 9. Boot images vs jail snapshots

A **boot image** (the Atrium installer / live USB) is a special
case of a jail snapshot: it's a snapshot of the system jail
(jail id 0). Same format, same import tooling. Building an
Atrium release becomes "atrium-jail export system-jail
--bundle". Installing Atrium becomes "atrium-jail import
atrium-0.5.0.atrium-jail.tar.zst". The bootstrap installer is
one tool wearing two hats.

This unification is downstream of using Tessera as the
universal substrate: there is no architectural difference
between "the OS" and "an app" — both are content-addressed
filesystems with capability manifests. Boot is just a
distinguished jail.

## 10. CLI surface

```
atrium-jail export <name>
    [--parent <snapshot-id>]
    [--bundle | --output <path>]
    [--volumes=all|none|<list>]
    [--exclude-volume=<list>]
    [--unsafe-include-secrets]
    [--sign | --no-sign]              # default: sign if Sigstore identity available

atrium-jail import <source>
    [--name <local-name>]
    [--from-bundle <path>]
    [--from-url <url>]
    [--no-verify-signature]           # explicit opt-out, requires user confirmation

atrium-jail list
    # snapshots stored locally (e.g., recently exported, parent references)

atrium-jail diff <snapshot-a> <snapshot-b>
    # human-readable summary of what changed: blobs added/removed, capability diff, manifest diff

atrium-jail prune
    # garbage-collect snapshots not referenced by any local jail or pinned by user
```

## 11. Storage of snapshots

When a snapshot is created or imported, its **manifest** is
stored under `/var/lib/atrium/snapshots/<id>.json` (cheap, small).
The **blobs** are simply in Tessera CAS, content-addressed,
shared with everything else. There is no separate "snapshot
storage" — manifests are pointers into CAS.

Snapshot pruning (`atrium-jail prune`) removes manifest files
whose blobs aren't referenced by any live jail or pinned
manifest. Tessera's normal CAS GC then reclaims any blobs that
become unreferenced.

## 12. Registry coexistence

A jail snapshot **may** be published to an `atrium-pkg`
registry. If so, the registry manifest schema gets a `[snapshot]`
section pointing at the snapshot manifest's URL + hash:

```toml
[distribution]
kind = "snapshot"

[distribution.snapshot]
manifest_url = "https://example.com/snapshots/7c4f8e2b.json"
manifest_hash = "blake3:7c4f8e2b..."
```

The bot check, signing requirements, and capability surfacing
are identical to source / binary distributions. From the
recipient's perspective, "install this from the registry" and
"import this snapshot directly" land the same content; only the
discovery path differs.

This is mostly useful for "blessed dev environments" — a team
maintains an `atrium-pkg`-published snapshot of their canonical
dev-jail; new team members `atrium-pkg install` it instead of
following a long setup README. Versioning, attestation, and
update flow all reuse the registry machinery.

## 13. Open questions / future work

- **Live snapshots** (without stopping the jail). Tessera has
  per-mount snapshot-gen support; combining that with
  filesystem freeze + flush-pending-writes would let us export
  a running jail. v1 punts; require stopped.
- **Encrypted snapshots** for sensitive content distributed
  out-of-band. Wrap CAS blobs with recipient pubkey before
  bundling. Out of scope for v1.
- **Snapshot signing tied to dev-jail provenance**. If a
  snapshot is exported from a dev-jail whose `atrium.toml` has
  declared source repos, attach attestations linking the
  snapshot to specific commits. Useful for "this snapshot was
  built from commit abc123 of github.com/foo/bar."
- **Streamed import** — start materializing the jail before all
  blobs have arrived, fetching on-demand as files are accessed.
  Tessera supports content-addressed lazy fetch already; need
  to wire it through the import flow. Useful for "click install,
  start using in seconds, full content fills in over the next
  minute."
