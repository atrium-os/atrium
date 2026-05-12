# `atrium-pkg` — Atrium package format and install path

**Status:** spec, 2026-05-08
**Owner:** D2.5 packaging track

How Atrium-shape applications and services get from "an artifact
somewhere" to "a manifest in `/etc/atrium/services.d/` or
`/var/lib/atrium/apps/` plus content addressable in Tessera." The
operator-facing tool that bridges third-party software (mysqld,
postgres, browsers) into the Atrium-jailed-services world.

Companion specs:
- `docs/spec/storage.md` — volume kinds + backends
- `docs/spec/portcullis.md` — manifests + capability schema
- `docs/spec/atrium-volumes.md` — volume allocation
- `docs/spec/jaild-policy.md` — manifests' jail-creation
  parameters

> **Spec drift note (2026-05-08):** the original design used a
> `kind = "cas"` volume to expose package contents read-only into
> a jail. That kind was removed because Tessera CAS dedup is
> automatic for *every* persistent volume — there's no
> distinction at the volume API. The replacement mechanism is
> still TBD; current best guess is that `atrium-pkg install`
> writes package content under `/var/lib/atrium/apps/<id>/`
> (which is on Tessera, so dedup happens transparently), and
> service manifests reference that path either via `path =`
> (jail rootfs) or `[[mounts]] kind = "ro_nullfs"`. The §3.4 /
> §3.6 references to `cas_root` in this doc need a rewrite when
> atrium-pkg gets built; for now they describe the original
> intent, not the current code.

## 1. Principle

> **An Atrium package is a content-addressable bundle of
> binaries+config+manifest, addressable by hash, dedup'd across
> all installations on the system, installed via a single
> CLI, never requiring host shell beyond the `atrium-pkg`
> command itself.**

Contrast with FreeBSD `pkg`:

| Concern | FreeBSD `pkg` | `atrium-pkg` |
|---|---|---|
| Files land in | `/usr/local/...` (host scope) | Tessera CAS layer, addressable as `cas_root` |
| Service goes into | `/usr/local/etc/rc.d/<name>` | `/etc/atrium/services.d/<name>.toml` |
| Shared lib dedup | none (per-package) | yes (CAS chunk-level across all packages) |
| Service runs as | host process via `rc(8)` | jailed via portcullisd → jaild |
| Network access | full host stack | per-manifest `[network]` capability |
| Persistent data | wherever the package decides (`/var/db/<pkg>`) | atrium-volumes-allocated, declared in `[[volumes]]` |
| Uninstall removes data? | sometimes (`pkg delete` won't, `pkg autoremove` might) | never (operator explicit `atrium-volumes-cli destroy`) |

`atrium-pkg` doesn't *replace* FreeBSD pkg — it *imports from*
it (and other sources) into the Atrium shape.

## 2. Package format

An Atrium package is a tarball containing:

```
manifest.toml             # the service manifest (atrium.toml or services.d-shape)
content/                   # the bundle's filesystem tree (binaries, configs, etc.)
  bin/
  lib/
  share/
  ...
metadata.toml             # provenance: source pkg, version, build date, author
                          # signatures (future)
init.d/                   # optional first-run setup scripts
                          # referenced from manifest's [volumes.X.init]
```

The `content/` tree is what gets ingested into Tessera CAS. The
package's `cas_root` reference (e.g., `atrium-app:mysql80-server@8.0.36`)
resolves to the root hash of that ingested tree.

`metadata.toml`:

```toml
[package]
name        = "mysql80-server"
version     = "8.0.36"
authors     = ["MySQL upstream"]
license     = "GPL-2.0-with-FOSS-exception"
homepage    = "https://www.mysql.com/"

[provenance]
source         = "freebsd_pkg"          # or "atrium_native", "tarball", ...
source_pkg     = "mysql80-server-8.0.36"
imported_at    = "2026-05-08T12:34:56Z"
imported_from  = "https://pkg.freebsd.org/freebsd:14:amd64/quarterly/All/..."
sha256         = "abcd1234..."

# Future: detached signature
# [signature]
# minisign_pubkey = "..."
# signature       = "..."
```

## 3. Install workflow

```sh
atrium-pkg install mysql80-server@8.0.36
```

What happens, step by step:

### 3.1 Resolve

`atrium-pkg` consults the configured registries (in
`/etc/atrium/pkg.conf`) to find the package by name+version.
Registries can be:
- **Local**: a directory on the host with `.atrium-pkg` tarballs.
- **Remote**: HTTPS URL with a manifest of available packages
  (signed). V1 work.
- **Built locally**: from a source tree via `atrium-pkg build`
  (see §4).

For V0, only local registries.

### 3.2 Fetch

Download / read the `.atrium-pkg` tarball into a temporary
location (`/var/run/atrium/pkg-staging/<job-id>/`).

Verify the SHA-256 against the registry's claim. Verify the
signature if signing is enabled (V1).

### 3.3 Ingest into Tessera CAS

Walk the `content/` tree, hash each file's content (or each
chunk for large files), insert into Tessera's CAS layer. The
root of the tree becomes the package's `cas_root`.

Tessera's chunk-level dedup means:
- Two installations of `mysql80-server@8.0.36` share 100% of
  storage.
- `mysql80-server@8.0.36` and `mysql80-server@8.0.37` share
  whatever bytes haven't changed (typically 90%+).
- Different packages that include the same shared library
  (e.g., `libssl.so.30`) share that file's chunks.

### 3.4 Drop manifest

Copy `manifest.toml` from the package to either:
- `/etc/atrium/services.d/<name>.toml` for a system service
  (e.g., mysqld), OR
- `/var/lib/atrium/apps/<app-id>/atrium.toml` for a user app
  (e.g., a browser).

`atrium-pkg` knows which destination based on a manifest field
(`[meta] kind = "system_service"` vs `kind = "user_app"`).

The manifest's `cas_root` field is rewritten to the resolved
hash from §3.3 — so manifests-as-shipped reference packages
abstractly (`atrium-app:mysql80-server@8.0.36`), but
manifests-as-installed reference concrete CAS roots.

### 3.5 Validate

Run the manifest through portcullis-toml's validator + jaild
policy validator + atrium-volumes' backend validator.

If any of these fail (manifest references an unconfigured
backend, exec_path not in jaild policy's allow-list,
capability outside what this Atrium install permits), the
install is rolled back: CAS-ingested content stays (it's
content-addressed; harmless if unreferenced; eventually GC'd),
but the manifest is not dropped, registry doesn't record the
install.

### 3.6 Register

Add an entry to `/var/db/atrium/pkg.installed.toml`:

```toml
[[installed]]
name           = "mysql80-server"
version        = "8.0.36"
cas_root       = "<sha256-of-tree>"
manifest_path  = "/etc/atrium/services.d/50-mysqld.toml"
installed_at   = "2026-05-08T12:34:56Z"
provenance     = "freebsd_pkg"
```

### 3.6.5 Shader precompile (V2+; aqueduct-gpu §9 Phase 2)

For packages shipping GPU shaders (any `.spv` files under
`content/`), `atrium-pkg install` runs a one-shot precompile pass
before §3.7. This is the warm-path side of the aqueduct-gpu
`OP_GPU_SHADER_RESOLVE` / `OP_GPU_SHADER_UPLOAD` two-phase wire
(see `docs/spec/aqueduct-gpu.md` §4.1–4.2): by validating shaders
and populating the cache at install time, app startup pays zero
SPIR-V upload round-trips.

Atrium-shipped packages author shaders in Slang (Khronos-stewarded,
Apache-2.0, multi-backend emit) — see
[`docs/LANGUAGE-POLICY.md`](../LANGUAGE-POLICY.md#shader-source-language).
The packaging step still ships SPIR-V (the wire format), so this
hook is source-language-agnostic: any compiler that emits valid
SPIR-V (slangc, glslang, dxc, naga) works, and the per-file validator
verdict is the same.

What runs (reference implementation:
`aqueduct-gpu-host/scripts/atrium-pkg-precompile-hook.sh`):

```sh
# 1. Compile  — run bundle's build.sh if present (slangc + annotate)
# 2. Verify   — manifest schema + every referenced shader
aqueduct-shader-tool verify-bundle <pkg-staging>/content/

# 3. Cache populate (per detected backend)
aqueduct-shader-tool precompile \
    --cache  /var/db/atrium/shaders/    \
    --backend <detected-host-vendor>    \
    --generation <detected-generation>  \
    --compiler-version <current>        \
    <pkg-staging>/content/
```

Atomicity: verify must succeed before precompile runs. A validator
rejection from verify aborts the install before the cache is
touched.

Per-file outcomes:
- **OK** — validator (`aqueduct_gpu_host::shader_validator`) accepted
  the bytes. Inserted into the cache keyed by `(SHA-256(bytes),
  backend, generation, compiler_version, SpirV)`. A subsequent
  `OP_GPU_SHADER_RESOLVE` from the running app hits warm.
- **REJECTED** — validator rejected. The install **aborts** with a
  per-file diagnostic; CAS-ingested content stays (harmless), no
  manifest is dropped. Rejection reasons include: forbidden
  capability (e.g. `PhysicalStorageBufferAddresses`), unbounded
  loop, oversized module, ray-tracing / mesh-shader features the
  sandbox doesn't support yet, etc.

The precompile pass runs **inside a Portcullis jail** with:
- Capability `cap.fs:/var/db/atrium/shaders` (writable, for cache
  insert).
- Capability `cap.cpu_time:60s` (precompile is bounded; if it
  spins, jaild SIGKILLs).
- No network. No FS access outside the staging directory and
  cache directory.

On hosts where multiple backends are present (e.g. MoltenVK on
macOS-HVF for dev, plus Software fallback), precompile runs once
per `(vendor, generation)` pair the host advertises via
`IOC_GPU_LIST_BACKENDS`. Cache files are vendor-keyed so they
don't collide.

V2 limitations:
- Precompile keeps shaders in their original SPIR-V form. Phase 3
  swaps in **backend-bytecode** translation (SPIR-V → MTLLibrary
  for MoltenVK, SPIR-V → AMDGPU ISA via atrium-mesa for the AMD
  native path) and the cache stores the translated artifact. The
  cache key already includes `compiler_version`, so the schema
  doesn't change.
- Tessera-backed cache lives at `/var/db/atrium/shaders/`
  for V2. The aqueduct-gpu host daemon's `ShaderCache::open` path
  points at this directory; the daemon and the package installer
  share the cache layer. Phase 3 migrates this to a Tessera
  prefix, eliminating the bespoke disk layout.

### 3.7 First launch

`atrium-pkg install` does NOT start the service. The operator
either:
- Does nothing — next reboot, portcullisd's bootstrap reads the
  new manifest and starts it.
- Triggers manually: `service atrium-portcullisd reload`
  (graceful) or `service atrium-portcullisd restart` (full).

Or, for user apps (per-launch via Forum dock), the install
appears in Forum's app list immediately.

## 4. Building packages from source

`atrium-pkg build <source-dir>` produces a `.atrium-pkg`
tarball.

Two paths:

### 4.1 Atrium-native source

A directory tree containing an `atrium.toml` + the source. The
build is a CAS ingest of the tree (Tessera-native; no shell
needed).

### 4.2 Wrapping a FreeBSD pkg

For third-party software like mysqld, the source is a FreeBSD
`.txz` package or a `pkg install` invocation. The build runs in
a one-shot install jail:

```
atrium-pkg build --from-freebsd-pkg mysql80-server@8.0.36
```

Behind the scenes:
1. portcullisd asks jaild to launch a one-shot "pkg-import"
   jail with network capability + `pkg`-install permission.
2. Inside the jail: `pkg install mysql80-server-8.0.36`.
3. After install, walk `/usr/local/...` in the jail's view,
   copy out a manifest of files + their contents.
4. Generate a draft Atrium `manifest.toml` (operator edits as
   needed).
5. Tarball it up: `mysql80-server-8.0.36.atrium-pkg`.

The draft manifest includes everything `atrium-pkg` could infer:
- `[exec]` based on the FreeBSD package's `rc.d` script
- `[[volumes]]` based on `/var/db/<pkg>` directories the
  FreeBSD package creates
- `[network]` defaulting to `disable` (operator opts-in based
  on what the service actually does)
- `[capabilities]` defaulting to none

Operator inspects the draft, fills in details (which backend
for the data volume, which lo0 alias address, etc.), then runs
`atrium-pkg install` on the resulting `.atrium-pkg`.

This separation keeps `atrium-pkg install` deterministic (no
network, no third-party state) while `atrium-pkg build` does
the messy upstream-pkg integration in a jailed sandbox.

## 5. Update model

```sh
atrium-pkg update mysql80-server          # update to latest 8.0.x
atrium-pkg update mysql80-server@8.0.40   # update to specific version
```

Atomic version switch:

1. Resolve, fetch, ingest the new version (steps 3.1–3.3).
2. Validate the new manifest.
3. Stop the running service (if any): portcullisd `pdkill`s
   the procdesc fd.
4. Replace the manifest file atomically (write `.tmp` + rename).
5. Drop the new manifest's first-run-init sentinel from the
   data volume (so the new version's init runs if needed).
6. Start the service: portcullisd reads the new manifest,
   asks atrium-volumes to provision (existing volumes return
   AlreadyProvisioned with paths), asks jaild to launch.
7. Old version's `cas_root` ref-count drops; Tessera GC
   eventually removes unreferenced chunks.

If the new version's manifest fails validation (e.g.,
references a backend the operator hasn't configured), update
fails; old version stays running.

Roll-back: `atrium-pkg rollback mysql80-server` finds the
previous version in the install registry, repeats steps 3–6
with the older manifest. Persistent data is unchanged
(operator's responsibility to ensure compatibility).

## 6. Uninstall

```sh
atrium-pkg uninstall mysql80-server
```

1. Stop the running service (portcullisd `pdkill` + EOF).
2. Remove the manifest file.
3. Decrement the `cas_root`'s ref-count in Tessera; GC
   asynchronously drops unreferenced chunks.
4. Remove the entry from the install registry.

**Persistent volumes survive.** Per the storage spec's
"data should never accidentally vanish" principle. Operator
removes them explicitly:

```sh
atrium-volumes-cli destroy mysqld/data --really-yes
```

`atrium-pkg uninstall --remove-data <pkg>` is a future
convenience flag that walks the manifest's `[[volumes]]` and
runs the destroys. V1.

## 7. Dependency model

V0: **no Atrium-pkg dependency resolution**. Each package is
self-contained; if mysqld needs a specific OpenSSL version,
that lives in mysqld's own bundle (CAS dedup makes the
duplication free).

V1: declarative dependency capability:

```toml
[depends_on]
"libssl@30" = "system"
"atrium-app:postgres-client@16" = "atrium_pkg"
```

`atrium-pkg install` resolves transitively. Cycle detection,
version solving (semver-style ranges), all the package-manager
mechanics. Defer unless V0's "self-contained bundles" model
hits real friction.

## 8. Registry shape

V0: **local registry only**. `/etc/atrium/pkg.conf`:

```toml
[[registry]]
name = "local"
kind = "directory"
path = "/var/db/atrium/pkg-registry"

[[registry]]
name = "operator-builds"
kind = "directory"
path = "/home/admin/atrium-builds"
```

V1: **remote registries** with HTTPS + signing:

```toml
[[registry]]
name        = "atrium-official"
kind        = "https"
url         = "https://pkg.atrium-os.org/"
sign_pubkey = "minisign:RWQf6LRCGA9..."

[[registry]]
name        = "atrium-community"
kind        = "https"
url         = "https://pkg.community.atrium-os.org/"
sign_pubkey = "minisign:RWS7XbJ..."
```

Multiple registries: `atrium-pkg search <pkg>` returns matches
from all configured registries; install picks the first match
or operator selects.

## 9. Signing model (V1+)

Every `.atrium-pkg` carries a detached minisign signature in
the registry. `atrium-pkg install`:

1. Fetches both the tarball and the signature.
2. Verifies signature against the registry's `sign_pubkey`.
3. Refuses install on signature failure with no override flag
   (no `--force` for sig failures; operator must remove the
   pubkey from `pkg.conf` if they really want unsigned
   installs).

Pubkey distribution is operator-side. Atrium's official key
ships with the OS install; third-party keys are added per
deployment.

## 10. Discipline

`atrium-pkg` is a CLI tool, not a long-running daemon. It runs
as the calling user (with privilege escalation via aqueduct to
portcullisd / atrium-volumes for the privileged ops) — it does
NOT need its own jail.

Per `LANGUAGE-POLICY.md`:
- Rust crate at `portcullis/atrium-pkg/`.
- `#![deny(unsafe_code)]` at root; no FFI needed (the
  privileged work is in the daemons it talks to).
- Standard CLI deps (clap or argh, serde, toml, reqwest for
  remote registries V1, sha2, minisign for signing V1).
- Aqueduct client to portcullisd, atrium-volumes, and
  Tessera's ingest service.

## 11. Open questions

1. **Tessera ingest API.** atrium-pkg needs to push content
   into Tessera CAS. Is that a syscall? An aqueduct service?
   A library call? Depends on Tessera v2/v3 API maturity. V0
   assumption: a `tessera-import(8)` already exists per project
   memory `tessera_binsplit` (Phase 1 complete with
   per-arch PC-rel masking) — atrium-pkg shells out.
2. **Multi-arch packages.** A single `.atrium-pkg` containing
   amd64 + arm64 binaries? Or separate per-arch packages?
   FreeBSD's pkg goes per-arch; we should follow.
3. **System service vs user app installation site.** Should
   `atrium-pkg install` always know which one (from manifest
   metadata)? Or take an explicit `--system` / `--user` flag?
   V0: from manifest metadata.
4. **Rollback failures.** If rollback also fails (new version
   can't start AND old version was already destroyed by
   update), system has no working version. Worth an
   "atomic update" mode that keeps both versions installed
   until the new one has run successfully for some period?
   V1+ feature.
5. **Distroless-shape.** Atrium apps could be distroless (just
   the app + libc, no shell, no coreutils). Encouraged;
   operator's choice. Doesn't affect atrium-pkg directly.

## 12. Implementation order

1. CLI skeleton + arg parsing (½ day).
2. Install registry + state file (½ day).
3. Local registry kind (½ day).
4. Tessera ingest path (1 day, depends on Tessera API).
5. Manifest validation + drop (½ day; reuse portcullis-toml).
6. `install` end-to-end with local registry (1 day).
7. `uninstall` (½ day).
8. `update` with atomic version switch (1 day).
9. `build --from-freebsd-pkg` (2 days, gnarly: needs pkg-import
   jail, tree walk, draft manifest generation).
10. Remote registry kind + signing (V1, 2-3 days).

Total V0 (steps 1-9): ~7 days. V1 (registry/signing): +3 days.

## 13. References

- `docs/spec/storage.md` — Tessera CAS layer
- `docs/spec/portcullis.md` — manifest schema
- `docs/spec/atrium-volumes.md` — volume allocation post-install
- `docs/spec/jaild-policy.md` — manifest validation against
  privileged-broker policy
- Project memory `tessera_binsplit` — the per-package CAS
  chunking work this depends on
- Project memory `tessera_perf_session_2026-05-03` — Tessera
  performance baseline that makes this viable
