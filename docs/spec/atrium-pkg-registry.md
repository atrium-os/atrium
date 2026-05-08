# `atrium-pkg` registry — distribution, signing, attestation

**Status:** spec, 2026-05-08
**Owner:** D2.5 packaging track
**Companion to:** `docs/spec/atrium-pkg.md` (package format, install path)

How Atrium packages are published, discovered, signed, and
distributed — without the Atrium project running an App Store.
The design goal is **zero ongoing infrastructure cost** for both
small FOSS publishers and the Atrium project itself, while
preserving end-to-end signature verification and capability-gated
install. Achieved by leaning on three external public-good
services: GitHub (hosting), Sigstore (keyless signing +
transparency log), and the publisher's own CI.

`atrium-pkg.md` covers the *package format* and *what happens on
install*. This doc covers everything *between* `atrium-pkg
publish` and `atrium-pkg install` — the registry, the signing,
the trust root.

## 1. Principle

> **The registry is an index of manifests, not a host of blobs.
> Manifests live in a public git repo; blobs live wherever the
> publisher already hosts releases; signatures live in Sigstore's
> public transparency log. The Atrium project operates the
> default index repo on GitHub and pays $0/month for it.**

What this is *not*:

- Not an App Store. No review queue (capability sandboxing does
  the work mechanically), no payment infrastructure, no
  revenue share, no curation beyond mechanical bot checks +
  typosquat detection.
- Not a binary host. We do not store, mirror, or re-upload
  publisher blobs. The registry is bytes-of-text-only.
- Not a single-vendor lock-in. The index format is portable git
  + TOML; a federation of registries is supported from day one
  (default + user-added).

## 2. Three distribution paths

`atrium-pkg-registry` distributes packages over the registry's
indexed path. There are three valid distribution shapes total
across the Atrium ecosystem; only the first two flow through the
registry:

| Path | Audience | Through registry? |
|---|---|---|
| **Source** | FOSS publishers, audit-conscious users | yes |
| **Binary** | Commercial / closed-source / fast-install | yes |
| **Snapshot** | Dev-to-dev jail exchange (`atrium-jail export`) | no — direct transfer |

Snapshot import is a separate path documented in
`docs/spec/atrium-jail.md`. The registry concerns itself
only with source + binary.

## 3. Registry repository layout

The default registry is a single git repository:

```
github.com/atrium-os/atrium-pkg-index
```

Layout:

```
atrium-pkg-index/
  registry.toml                # registry metadata, root maintainer keys, schema version
  pkgs/
    fo/foo/                    # 2-char shard (like crates.io, like git's loose object dirs)
      manifest.toml            # latest-version pointer (just "version = ...")
      1.0.0.toml               # version-pinned manifests (full)
      1.0.1.toml
    fi/firefox/
      manifest.toml
      125.0.toml
      ...
  ATTESTATION-POLICY.md        # human-readable trust policy
  CODEOWNERS                   # maintainer scopes for human-review-required PRs
```

**Sharding rationale:** flat `pkgs/<name>/` does not scale past
a few thousand entries on most filesystems and is slow to
shallow-clone. Two-character prefix sharding gives ~676 buckets;
GitHub repos and consumer filesystems both handle this well.
Same shape as `crates.io` index and Git's `.git/objects/`.

**Index growth budget:** at 200k packages × 20 versions × ~2 KB
per manifest = ~8 GB git history (compressed: ~2 GB). Past that,
shard the *index itself* by first-letter into separate repos.
Don't pre-optimize.

## 4. Manifest schema

A version-pinned manifest is a single TOML file. Example
(`pkgs/fi/firefox/125.0.toml`):

```toml
schema = "atrium-pkg/1"

[package]
name = "firefox"
version = "125.0"
description = "Mozilla Firefox web browser"
homepage = "https://www.mozilla.org/firefox/"
license = "MPL-2.0"

# Distribution shape. Exactly one of "source" or "binary".
[distribution]
kind = "binary"               # or "source"

[distribution.targets.freebsd-aarch64]
url = "https://github.com/mozilla/firefox/releases/download/125.0/firefox-125.0-freebsd-aarch64.atrium-bin"
url_alternates = [
  "https://download.mozilla.org/firefox-125.0-freebsd-aarch64.atrium-bin",
]
sha256 = "abc123def456..."
size_bytes = 87654321

[distribution.targets.freebsd-amd64]
url = "..."
sha256 = "..."
size_bytes = ...

# Sigstore reference. The Rekor entry attests
# (publisher-identity, blob-hash, build-provenance).
[signature]
sigstore_rekor_uuid = "24296fb2429f7c8b0e3f4ad5d4b5a8c6..."
publisher_identity = "https://github.com/mozilla/firefox/.github/workflows/release.yml@refs/tags/125.0"

# Optional source attestation (binary distributions only).
# If present, asserts the binary is reproducible from this source.
[provenance]
source_url = "https://github.com/mozilla/firefox/archive/refs/tags/125.0.tar.gz"
source_sha256 = "..."
toolchain = "rustc-1.86.0+atrium-1"
build_workflow = "https://github.com/mozilla/firefox/.github/workflows/release.yml"

# Capability declarations — surfaced to user at install time.
# Schema is the same as `docs/spec/portcullis.md` `[capabilities]`.
[capabilities]
network.outbound = ["*"]
device.gpu = true
filesystem.user-downloads = "rw"
filesystem.user-documents = "ro"

# Runtime dependencies (other atrium-pkg names).
[dependencies]
"libc-runtime" = ">= 1.0"
```

The `pkgs/<shard>/<name>/manifest.toml` (no version suffix) is a
much smaller pointer file:

```toml
schema = "atrium-pkg/1"
latest = "125.0"
versions = ["123.0", "124.0", "124.1", "125.0"]
```

`atrium-pkg install firefox` resolves `latest` then fetches
`125.0.toml`. `atrium-pkg install firefox@124.0` fetches
`124.0.toml` directly.

## 5. Publishing flow

A publisher runs `atrium-pkg publish ./Atrium.toml` (which is
their *source-side* package definition — not the registry-side
manifest). The CLI:

1. Builds the binary or packages the source tarball per
   `Atrium.toml`.
2. Calls `cosign sign-blob` on the artifact. Cosign authenticates
   via OIDC against GitHub Actions's identity token (no secret
   keys, no key management). Fulcio issues a short-lived signing
   cert; the signature + provenance attestation lands in Rekor
   (Sigstore's public transparency log). **The publisher manages
   zero keys.**
3. Uploads the blob to the publisher's own GitHub Release via
   `gh release create`. (Or any other URL the publisher prefers
   — S3, B2, their own server. The blob URL is just a URL.)
4. Generates a registry-shape manifest (per §4) by combining
   `Atrium.toml` metadata + the Rekor UUID + the blob URL +
   blob hash.
5. Forks `atrium-os/atrium-pkg-index` (auto, via `gh`).
6. Adds the manifest file to the appropriate shard.
7. Opens a PR titled `add: firefox 125.0`.

A small FOSS publisher does this in ~30 seconds after their
build finishes. Their ongoing operational footprint: their
existing GitHub repo. No new infrastructure.

## 6. Bot check on submission

The index repo runs a GitHub Action on every PR (free for
open source). The bot:

1. **Schema check.** Manifest TOML parses; required fields
   present; `schema = "atrium-pkg/1"`.
2. **Sigstore check.** Queries Rekor for the declared UUID;
   verifies the entry exists, the blob hash in the entry matches
   the manifest's `sha256`, and the publisher identity in the
   entry matches the manifest's declared `publisher_identity`.
3. **Blob reachability.** HEADs the `url`; expects 200 + Content-
   Length matching `size_bytes`. (Does not download the blob —
   that would cost real bandwidth on the bot side. The hash
   check happens client-side at install time.)
4. **Capability declaration sanity.** Caps parse against
   `portcullis.md` schema; flag any cap on a maintainer-review
   list (e.g., `device.kvm`, `network.host`, `filesystem.system`).
5. **Typosquat check.** Levenshtein distance ≤ 2 against the
   curated top-1000 package names → flag for human review.
6. **Identity continuity.** For updates to existing packages,
   the new manifest's `publisher_identity` must match the prior
   version's. Identity changes (publisher hand-off) require
   manual review.
7. **Provenance check** (if `[provenance]` is present): the
   declared `source_url` is reachable and `source_sha256`
   matches.

All green → PR auto-merges. Any flag → label `needs-review`,
ping a CODEOWNERS-defined maintainer for the relevant package
namespace or capability scope.

## 7. Install flow (client-side verification)

`atrium-pkg install firefox`:

1. **Sync index** (sparse fetch of `pkgs/fi/firefox/manifest.toml`
   over HTTPS to `raw.githubusercontent.com`, or git pull if
   user has cloned). Cache locally.
2. **Resolve version.** `latest = "125.0"` → fetch `125.0.toml`.
3. **Verify Sigstore.** Query Rekor for the manifest's UUID;
   verify entry matches manifest's claimed `publisher_identity`
   + `sha256`. Cross-check against a second Rekor mirror if
   `--paranoid` set.
4. **Download blob** from the manifest's `url`. On 404, try
   `url_alternates` in order. On all-fail, try peer-cache
   (§9).
5. **Verify blob hash** against manifest's `sha256`. Mismatch =
   abort, do not retry (we don't trust the URL anymore).
6. **Show capability prompt** to user — declared caps, with
   per-cap human-readable explanation. User approves the set.
7. **Install** via the path documented in `atrium-pkg.md` —
   ingest content into Tessera CAS, write service manifest,
   etc.

Note that step 5 (hash check) means the blob URL is *just a
hint*. The actual integrity guarantee is content-addressing: the
manifest commits the publisher to a specific hash via Sigstore;
any byte-for-byte equivalent source serves equally well. This is
the foundation for §9 peer-cache.

## 8. Trust model

**Trust root**: the `atrium-os/atrium-pkg-index` repo's main
branch.

**Defenses:**

- All merges to main require a signed commit by an Atrium
  maintainer (GitHub branch protection enforces this).
- Sigstore's Rekor log is independent of GitHub. An attacker
  who compromises GitHub but not Sigstore cannot forge a
  signature. An attacker who compromises Sigstore but not
  GitHub cannot inject a manifest into the index.
- Cross-mirror verification: `atrium-pkg --paranoid` queries
  multiple Rekor mirrors. The Sigstore project operates
  several; we add at least one community-operated mirror as a
  redundancy.
- The CLI ships pinned to the index's expected commit hash from
  the last release. `atrium-pkg verify-index` walks the commit
  chain forward from that pin and refuses to run if signatures
  on the chain don't verify against the pinned maintainer keys
  in `registry.toml`. Stops a "GitHub force-push the whole
  history" attack.

**What the trust model does NOT defend against:**

- A compromised publisher signing a malicious release. Their
  Sigstore identity will sign whatever they tell it to. The
  capability sandbox limits damage; capability prompts let the
  user decline an install if a sudden new capability request is
  suspicious. Eventual mitigation: stable-version reputation
  (publisher-confirmed-stable for ≥30 days before the index
  surfaces it as `latest`).
- The user blindly accepting all capability prompts. UX must
  surface diff against prior version's caps and flag *new*
  caps prominently.

## 9. Peer cache (resilience, not infrastructure)

Once a user installs a package, they have its blob in local
Tessera CAS, content-addressed. `atrium-pkg` can fetch a missing
blob from a peer instead of (or alongside) the publisher's URL:

- **LAN discovery** via mDNS (atrium-discovery, `network.md`).
  "Anyone on the LAN have blob `sha256:abc…`?" Hash check on
  arrival means the peer can't lie.
- **Optional opt-in DHT** — Atrium machines that opt in
  participate in a Kademlia-style DHT keyed on blob hash. Off
  by default for privacy.

This is BitTorrent semantics without BitTorrent's protocol or
trust complexity, because content-addressing makes peer trust
unnecessary. The publisher's CDN failing or going away does not
mean the package becomes uninstallable — every existing user is
a potential mirror.

The peer cache also protects against the long-tail blob-rot
problem (publishers delete repos). For abandoned-but-popular
packages, the foundation can run an opt-in **preservation
mirror** that participates in the DHT and re-hosts blobs whose
publisher URLs have gone 404. Cheap because storage is bounded
by *what people actually still install*, not the entire
historical universe.

## 10. Costs

| Party | Cost | Notes |
|---|---|---|
| Small FOSS publisher | $0 | Existing GitHub repo + Actions; no new infra |
| Commercial publisher | $0 for registry | They already pay their own CDN; registry just indexes URL |
| Atrium project | $0 | GitHub org is free for open source; Actions is free; no blob hosting |
| End user | $0 | Fetches from GitHub's CDN |

The only optional line item: a domain like `atrium-pkg.org`
(redirect to GitHub), ~$12/year. Not required for v1; the client
can hardcode the GitHub URL.

## 11. Limits and acceptable trade-offs

- **GitHub release blob size cap**: 2 GB per file. Outliers
  (game datasets, ML weights) split blobs or self-host on B2/R2.
  Manifest's `url` field accepts any HTTPS URL.
- **GitHub bandwidth policy**: "for software distribution, not
  abuse." Atrium installs are exactly that. If we ever get rate-
  limited at scale, that's a fundraising opportunity.
- **Publisher repo deletion**: blob disappears. Mitigated by
  peer cache (§9) + optional preservation mirror.
- **Index size**: addressed in §3 (shard at 1 GB threshold).
- **No payments**: commercial publishers handle commerce on
  their own site; manifest may point at a license-key endpoint.
  Atrium-pkg never touches money. (Critical: keeps us out of
  payment-processor regulatory scope.)

## 12. Federation

The registry URL is configurable per-user:

```toml
# ~/.config/atrium-pkg/registries.toml
[[registry]]
name = "default"
url = "https://raw.githubusercontent.com/atrium-os/atrium-pkg-index/main"
priority = 0

[[registry]]
name = "experimental"
url = "https://raw.githubusercontent.com/atrium-experimental/index/main"
priority = 10                # higher number = lower priority; default registry wins on name conflict
```

Use cases:

- **Corporate**: internal-only registry for company packages,
  default registry for everything else.
- **Experimental software**: curated separately from stable.
- **Distribution variants**: an alt-licensing registry, a
  region-specific registry, etc.

Registry-trust is per-registry: the client's pinned-commit /
maintainer-key check (§8) applies independently to each
registry. Adding a registry is an explicit user action with a
"this registry has full install authority on your machine"
warning.

## 13. Migration path if we outgrow GitHub

The whole design is portable:

- **Index** is plain TOML in a git repo → move to self-hosted
  Forgejo / GitLab / Gitea instance, update hardcoded URL in
  next `atrium-pkg` release, old clients keep working via raw
  HTTPS until they upgrade.
- **Blob URLs** are just URLs → publishers can move them
  anywhere; `url_alternates` (already in schema today) lets
  clients try fallbacks.
- **Sigstore** is independent of GitHub already.

We are renting GitHub's infrastructure for free until we have a
specific reason not to. No semantics change at migration.

## 14. Open questions / future work

- **Reputation / "stable" promotion.** Today `latest` is just
  "the most recently published version." Should we have a
  separate `stable` channel that requires N days without a
  reported security issue? Probably, post-v1.
- **Capability diff visibility.** Need UX work on showing
  "version 124 → 125 added capability X" prominently in the
  install card.
- **Required-attestation-for-source-paired-binary.** If a
  publisher offers both source and binary distributions for the
  same version, should the binary be required to have a
  reproducible-from-source attestation, to prevent "source
  theater"? Strong yes. Implementation: bot check enforces it
  during PR validation.
- **Snapshot import** (`docs/spec/atrium-jail.md`) coexistence:
  document how a registry-installed package + a
  snapshot-imported jail can refer to the same Tessera CAS
  blobs; both paths land bytes in the same place.
- **Long-tail preservation tier.** Concrete trigger: if a
  package's publisher URL has been 404 for ≥30 days and the
  package has ≥100 active installs (telemetry needed for this
  metric), foundation auto-mirrors it. Telemetry collection is
  itself a privacy concern; defer until we have a real signal
  this matters.
