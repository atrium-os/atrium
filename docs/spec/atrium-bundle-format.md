# Atrium bundle format

Status: spec settling a divergence. Supersedes the scattered bundle notes in
`insula.md` §3 and reconciles the two manifest schemas in tree
(`insula-manifest`/`manifest.toml` and `portcullis-toml`/`atrium.toml`).
Last updated: 2026-06-16.

> **One sentence.** An Atrium app ships as a single, signed, content-addressed
> **Insula bundle** — a self-contained tree carrying its manifest, its entry
> binary, its *runtime closure*, and its assets — because a Portcullis jail's
> rootfs **is** that tree.

## 1. Why a bundle at all (the jailing argument)

On Linux/FreeBSD an app is scattered: `/usr/bin/app`, `/usr/lib/*.so`,
`/usr/share/app/…`, resolved at runtime against a shared system. Atrium can't work
that way: a Portcullis jail's rootfs is **only the app's own tree** (a nullfs+
unionfs of `apps/<id>` + an overlay; see `portcullis.md` §4). There is no shared
`/usr/lib` mounted in, no `ldconfig`, no FHS. So an app **must** be self-contained.

That single constraint makes the bundle the unit of *distribution, execution,
addressing, and trust at once* — closer to a macOS `.app` or a container image than
to a `pkg`/`apt` package. It is a strength: bundles are reproducible (byte-identical
everywhere), dedup'd (Tessera CAS), verifiable (one signature over the whole tree),
and capability-declared (the manifest is the contract).

## 2. The canonical decision

| aspect | canonical | notes |
|--------|-----------|-------|
| **the unit** | an **Insula bundle** | the term; one app = one bundle |
| **manifest file** | **`atrium.toml`** | the NAMING-canonical name (NAMING.md, Portcullis, Opifex). `manifest.toml` is the legacy insula-manifest name and is **deprecated** → migrate to `atrium.toml`. |
| **archive (wire form)** | **`.insula`**, magic `INSB`, v1 (§8) | the single-file transport form before a bundle is resolved into CAS |
| **at-rest store** | **Tessera CAS**, content-addressed by root hash | byte-identical on every device |
| **naming / resolution** | `atrium-app://…` → root hash via **Nomenclator** | publisher manifests map names → hashes |
| **trust** | one publisher signature over manifest+contents | `portcullis-sig` (ed25519) / Sigstore root; `/etc/atrium/publishers` |
| **lifecycle — binary** | **Opifex** (`opifex install/list/uninstall`, rollback) | the pkg-style installer |
| **lifecycle — from source** | **insula** (ports/Homebrew-style, jail-aware) | compiles, then hands Opifex a bundle |

There is exactly **one manifest filename (`atrium.toml`) and one schema** (§4).
Two host adapters *translate* that one manifest to their sandbox mechanism — they
do not define their own manifest:

- `portcullis-jail` → `jail.conf` (FreeBSD jail; the Atrium-canonical host).
- `insula-host-macos` → SBPL profile + entitlements (the macOS host adapter).

## 3. Bundle layout

```
<bundle-root>/                 # content-addressed by its root hash in Tessera
  atrium.toml                  # the manifest (§4) — the capability contract
  atrium.toml.sig              # detached publisher signature (or embedded in .insula)
  bin/<entry>                  # native ELF per arch (or a fat binary), or an IR artifact
  lib/  libexec/               # the runtime closure (§5) — shipped, not resolved at install
  share/  fonts/  assets/ …    # resources
```

Install = **verify signature → resolve through Tessera CAS → register with
Portcullis**. No compilation, no dependency hunt, no host-lib pull. The same bytes
run on every device that has the bundle.

## 4. The unified manifest schema (`atrium.toml`)

The canonical schema is the **union** of today's two, with one capability model. It
is host-neutral: it declares *what the app is and what it may do*, never *how a
given host enforces it*.

```toml
[app]
id          = "org.atrium.edit"     # reverse-DNS; REQUIRED (was implicit in insula-manifest)
name        = "Atrium Edit"
version     = "1.2.3"
sdk-version = "1.x"                  # from insula-manifest; optional, default current
description = "A text editor."
icon        = "editor"

[bundle]
form   = "native"                   # native | ir   (insula-manifest §3.2/3.3)
arches = ["aarch64-freebsd", "aarch64-darwin"]
entry  = "bin/atrium-edit"          # one canonical home for entry (see migration note)

[capabilities]                      # the TYPED vocabulary the runtime enforces
graphics          = "fresco"
window-management = true            # restricted: the session shell only
forum-control     = true            # restricted: Forum chrome only
clipboard         = true
notify            = true
filesystem        = ["~/Documents"]
network           = "none"          # none | loopback | full
fonts             = { mode = "read-only", paths = ["/usr/share/fonts"] }
# … the full vocab from portcullis-toml::Capabilities …

# Richer app-platform sections (from insula-manifest), all OPTIONAL:
[storage]      data = "512M"
[ipc]          # declared service sockets
[peer]         # embed/peer roles (Limen)
[entry-points] # atrium-app:// scheme handlers
[background]   # resident / triggered
[setup]        # first-run script (+ override caps)
[resources]    # memory/cpu/files rlimits
[supervision]  # restart / keep-alive / instances
```

**Capability model — typed, not a free map.** The runtime (`portcullis-jail`) and
the consent diff both key off named capabilities, so the canonical `[capabilities]`
is the **typed** `portcullis-toml::Capabilities` vocabulary (extended over time),
NOT insula-manifest's generic `BTreeMap`. Unknown keys are still captured (forward
compat) and surfaced by the consent diff, but the known surface is explicit.

**Entry location.** insula-manifest put `entry` under `[bundle]`; portcullis-toml
put it under `[app]`. Canonical home is **`[bundle].entry`** (it's a bundle-layout
fact). The loader accepts `[app].entry` as a deprecated alias during migration.

## 5. The runtime-closure rule (reproducibility)

The bundle **ships its own runtime closure** — the shared libraries the entry
binary needs, plus the rtld (`/libexec/ld-elf.so.1` on FreeBSD) — under `lib/` and
`libexec/`. This is what makes install a pure *verify + register* and keeps the
bundle byte-identical everywhere. Closure assembly is a deterministic **build/pack**
step (the missing bundle-assembly tool, the counterpart to Opifex-the-installer),
NOT an install-time action.

> **Bring-up exception (explicit, temporary).** `opifex install` today resolves the
> closure at install time via `ldd` on the *host's* libraries (see
> `opifex/src/main.rs::resolve_runtime`). That is convenient for VM bring-up but
> **violates byte-identical reproducibility** (it pulls whatever lib versions the
> install host has). It is a fallback for thin bundles, not the norm. The norm is a
> bundle that already carries `lib/`+`libexec/`; the build tool produces those, and
> install copies them verbatim.

Statically-linked entries need no `lib/` at all (smallest bundle). IR (`form =
"ir"`, WASM) bundles AOT-compile at install and cache the native result in Tessera
keyed by `(bundle-hash, arch, sdk-version)` (`insula.md` §3.3–3.4).

## 6. The protocol stack

```
author → build → sign → pack(.insula / INSB) → publish
                                  │
                         atrium-app://name  ──(Nomenclator)──▶ root hash
                                  │
device: fetch .insula → verify sig (portcullis-sig + /etc/atrium/publishers)
        → resolve into Tessera CAS (dedup) → Opifex install
        → app tree at /var/lib/atrium/apps/<id>/
        → portcullisd launch: per-app uid + jail (portcullis-jail translates caps)
```

Updates are content-addressed + atomic (Opifex): a new root hash, verified, swapped
in, old one retained for rollback. No per-launch download.

## 7. How this got reconciled (the macOS experiment retired)

Originally two manifest schemas existed: `insula-manifest` (`manifest.toml`, the
macOS Insula-host experiment) and `portcullis-toml` (`atrium.toml`, the Atrium
runtime). The macOS host adapter was always **bootstrap scaffolding** — a way to
start Insula app development before Atrium's own subsystems existed. With Pergola,
the graphics stack, Portcullis jails, and Opifex now in place, that scaffolding was
**retired** (2026-06-16): the entire macOS cluster — `insula-host-macos`,
`insula-cli`, `insula-manifest`, `insula-bundle`, `atrium-mon`, `insula-hello`,
`atrium-fetch`, `atrium-netd-macos` — was removed. Nothing on the Atrium/FreeBSD
side depended on it.

So the reconciliation is not a migration — it's a deletion. **`atrium.toml`
(`portcullis-toml`) is now the sole manifest; there is no second schema.** The
`portcullis-toml` superset already absorbed the useful `manifest.toml` fields
(`[bundle]` form/arches/entry, `sdk-version`; see §4) so nothing of value was lost.

Remaining build-out (Atrium-native, no macOS coupling):
- **Bundle-assembly tool** — the closure-shipping packer + signer (the publisher
  side; the counterpart to Opifex-the-installer) that writes a self-contained,
  signed `.insula` (§8) with `lib/`+`libexec/` resolved deterministically at build
  time, so install is pure verify+register and `opifex`'s install-time `ldd`
  (`resolve_runtime`) reverts to the documented fallback (§5).
- **insula (the source/ports tool)** — the compile-from-source, jail-aware path
  (the Homebrew/ports counterpart to Opifex), to be (re)built Atrium-native.
- **Consent diff** — re-grown on `portcullis_toml::Capabilities` when the
  install-time consent UX lands (the retired `insula-manifest::diff` is the
  reference shape).

## 8. The `.insula` archive format (INSB v1)

The single-file transport form (deterministic, self-describing). Preserved here
from the retired `insula-bundle/archive.rs` so the Atrium-native packer can
reimplement it verbatim:

```text
  bytes [0..4)   magic "INSB"
  byte  [4]      version = 1
  bytes [5..8)   reserved (zero)
  bytes [8..16)  n_entries (u64 LE)
  per entry:
    [..2)  path_len (u16 LE)
    [..L)  path  (UTF-8, forward-slash, bundle-relative)
    [..4)  mode  (u32 LE, low 9 bits)
    [..8)  size  (u64 LE)
    [..N)  data
```

Properties: **deterministic** (entries sorted lexicographically by path; no
mtimes/uids — two packs of the same tree are byte-identical), **self-describing**
(the 4-byte magic suffices; no extension heuristics), single-pass pack/unpack (no
central directory). Extension: `.insula`. Out of scope for v1: per-entry
compression (zstd planned) and per-entry checksums (the bundle's detached publisher
signature + the Tessera CAS root hash cover integrity).
