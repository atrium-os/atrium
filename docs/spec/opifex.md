# Opifex: the package catalog

> *Opifex* (Latin): a craftsman, one who makes. Atrium's package system: the catalog of
> published packages, and the fetch, check, install, update and rollback of them.

**Status:** accepted 2026-11-17 (Shankar: all §15 decisions as recommended; decision 1 with
the Atrium catalog holding only what Atrium builds; GitHub Pages first, a domain later).
C0 built 2026-11-17 (§14.1).
**Scope:** what a package is, who may publish one, how the catalog is signed, hosted and
downloaded, how a package is fetched, checked and installed, what the person sees, updates and
rollback, and how fonts, drivers, firmware, toolchain layers and the SDK crate registry ride on
the same machinery.
**Background:** Shankar 2026-11-17 asked who owns the package catalog; nobody did. The pieces
were design only (atrium-pkg.md, atrium-pkg-registry.md, atrium-pkg-install-ux.md from May, and
the nomenclator.md sketch). They disagree with each other and with the code (§2). Several
pieces of work wait on this one (§3).

**Supersedes** (once accepted): [atrium-pkg.md](atrium-pkg.md),
[atrium-pkg-registry.md](atrium-pkg-registry.md),
[atrium-pkg-install-ux.md](atrium-pkg-install-ux.md). Narrows
[nomenclator.md](nomenclator.md) (§9).

Related: [atrium-bundle-format.md](atrium-bundle-format.md) (the bundle),
[portcullis.md](portcullis.md) (manifests and the launch gate),
[insula.md](insula.md) §3, §14 (install and updates), [limen.md](limen.md) §2 (system roles),
[nuntius.md](nuntius.md) (the fetcher), [arca.md](arca.md) (app identity, publisher keys),
[sigillum.md](sigillum.md) (presses, WebAuthn RP ids), [device-assignment.md](device-assignment.md)
§4.4, §4.7, [fonts.md](fonts.md), [dev-tools.md](dev-tools.md) and
[dev-tools-plan.md](dev-tools-plan.md) (layers, SDK, bundles from workspaces),
[app-composition.md](app-composition.md) (workers), [opening-links.md](opening-links.md) L5,
[navigator-ui.md](navigator-ui.md) (the app prompt), [personas.md](personas.md),
[homes.md](homes.md) (dev bundles), [atrium-ax.md](atrium-ax.md) (coverage check).

## 1. Short answer

- **The Atrium catalog vouches only for what Atrium builds** (Shankar 2026-11-17). It holds
  `org.atrium.*` apps, fonts, drivers, firmware, layers and the SDK. Atrium has no review
  team, and a signature proves who published something, not that it is honest, so the
  project does not put third-party apps in front of people under its name. Anyone else runs
  their own catalog with the same format and tools (§6.5); a person adds it in Curia with a
  press, and every install sheet names the catalog and the publisher.
- **Each catalog downloaded whole.** The catalog is a signed, append-only list of every
  published package version: id, publisher, kind, version, per-architecture hash and size,
  where to fetch it, the capabilities it asks for, the roles it implements, the device ids
  it drives, the scripts a font covers. Every Atrium machine downloads all of it and
  answers every question locally: "is there a driver for this USB id", "which package has a
  Japanese font", "who publishes `com.example.weather` and what does it ask for". Nothing
  about what the person looks at, plugs in or reads leaves the machine until they choose to
  install something.
- **Static files, no servers.** The catalog is a set of static files (a Merkle log in
  tiles, like Go's checksum database and Certificate Transparency) and the packages are
  `.insula` archives at plain HTTPS URLs. Hosting is GitHub Pages and GitHub Releases at
  first; any mirror works because every byte is checked by hash. Atrium runs no service
  and pays nothing.
- **One signing scheme, the one in the code.** P-256 ECDSA as `portcullis-sig` does today,
  for bundles, layers, fonts, drivers and the catalog itself. No Sigstore keyless signing,
  no minisign, no ed25519 second scheme. The publisher signs `atrium.toml`; `atrium.toml`
  now carries the hash of a content index listing every file, so the one signature covers
  every byte (today it covers only the manifest).
- **Two kinds of signature, checked separately.** The publisher signs the package; the
  catalog signs the list. A stolen catalog key can hide or delay a release but cannot make
  one, because each entry carries the publisher's own signature, and a publisher's key can
  only change by the old key's signature or Atrium's offline root keys.
- **Binary only.** The catalog carries built bundles. Building from source happens in a
  workspace ([dev-tools.md](dev-tools.md)), whose output is a bundle like any other. The
  install-on-the-user's-machine cargo build in atrium-pkg-install-ux.md is dropped.
- **Jailed, split by risk.** `opifex-fetch` talks to the network through Nuntius and
  trusts nothing it gets. `opifex-check`, a worker with no capabilities, parses the catalog
  and unpacks archives. `opifexd`, the only privileged part, never parses an archive or
  touches the network; it checks the signature and re-hashes every file as it copies the
  checked tree into place. `opifex-sheet` draws the install sheet (its own window for now; a Limen system role for windowed callers later, §14.4).
- **Install is one sheet, drawn by the system.** Any app, the Navigator, Monstrator,
  Artifex, a Praeco driver notice or Curia can ask for it; none of them draws it. It shows
  the publisher, what the package may do, its size and licence, and on an update only what
  changed.
- **Updates in the background, consent only when needed.** Strict capability subsets
  update quietly; additions ask; a change of publisher is refused. The old version stays on
  disk for one-step rollback.
- **Fonts, drivers, firmware, layers and the SDK registry use the same machinery** with
  their own `kind`, each signed only by keys allowed that kind (§5.3).

## 2. What exists, and what this settles

The code: `portcullis/opifex` (448 lines) installs a local bundle folder into
`/var/lib/atrium/apps/<id>/`, copies the entry binary's `DT_NEEDED` closure from a sysroot,
lists and uninstalls. It checks `atrium.toml.sig` (P-256, base64 DER) against the PEM keys in
`/etc/atrium/publishers`, but with its own copy of the check, not `portcullis-trust`'s gate, and
it allows unsigned installs when no publisher key is configured. portcullisd checks the same
signature again at every launch. There is no network fetch, no index, no update, no rollback.
Tessera has no ingest or diff-stream API yet; `/var/lib/atrium` is on the Tessera root, so
files placed there are deduplicated by Tessera anyway.

The specs disagree. This spec picks:

| Question | Said before | This spec |
|---|---|---|
| Signature algorithm | ed25519 (bundle-format, insula §12), P-256 (code) | P-256, as built |
| Keyed or keyless | Sigstore keyless with Rekor (registry, navigator-backend) | keyed, pinned; our own append-only log gives the transparency Rekor would (§6.4) |
| What the signature covers | "manifest and contents" (specs); manifest only (code) | manifest, which names the content index hash (§4.2) |
| How names resolve | DNS TXT then per-publisher manifest (insula §12, nomenclator) | the downloaded catalog; DNS is checked once, when a publisher registers (§9) |
| Index shape | git repo of TOML per version, fetched per package (registry) | Merkle log of signed entries, fetched whole (§6) |
| Source or binary | both, with on-machine cargo builds (install-ux) | binary only (§1) |
| Library closure | in the bundle (bundle-format); resolved at install (code) | in the bundle for catalog packages; install-time resolving stays for local side-loads only (§7.5) |
| "Catalog" | portcullisd's installed list (forum, nuntius); Opifex's index (device-assignment, fonts); the font pin table (fonts) | *the catalog* is Opifex's list only; portcullisd's is *the installed list* (§13) |

## 3. Who is waiting, and what each needs

| Waiting | Needs from Opifex | Where |
|---|---|---|
| CJK fonts on demand, colour emoji (fonts thread) | `kind = "font"` packages, a coverage field to find the right one, a pin list the font library trusts | §8.1 |
| `atrium-app://` for apps not installed (Monstrator L5) | look up an app id, then the install sheet | §7.1, §9 |
| Navigator's app prompt | a pre-install query: publisher and capabilities for an id, no network (built 2026-11-18 on `Describe`, navigator-ui.md §6.4) | §7.1 |
| Driver lookup (device-assignment §4.7, decision 16) | every driver package's `devices.d` matches in the local catalog; install through the trusted sheet | §8.2 |
| Firmware (device-assignment §4.4, wifi) | `kind = "firmware"`, platform-signed | §8.2 |
| Toolchain layers (toolchains thread) | `kind = "layer"`, platform key only, the layer contract that thread defines | §8.3 |
| SDK crate registry (toolchains thread, M12) | the sparse index signed with the same keys, hosted next to the catalog | §8.4 |
| Bundles from workspaces (platform thread, M4) | install of a local bundle signed by a publisher key held in Arca | §7.4 |
| Workers, roles, ipc (app-composition) | carry them; read them from portcullis-toml; show the composition facts on the details page | §7.2 |
| Sigillum WebAuthn RP ids | verify `/.well-known/atrium-apps` at install, record the result | §7.3 |
| atrium-ax coverage | a check in publishing, a flag in the sheet when overridden | §10.2 |
| Artifex extensions and language servers | search by role in the local catalog | §6.6 |

## 4. Packages

### 4.1 Kinds

A package is a bundle as in [atrium-bundle-format.md](atrium-bundle-format.md): a tree with
`atrium.toml` at the top and a detached `atrium.toml.sig`, shipped as a deterministic
`.insula` archive. The manifest's top-level `kind` says what it is:

| `kind` | Installs to | Who may sign it (v1) | Who may install |
|---|---|---|---|
| `app` (default) | `/var/lib/atrium/apps/<id>/` | platform in the Atrium catalog; a third-party catalog's publishers in their verified prefixes (§6.5) | any person, for themselves (§7.6) |
| `service` | `/var/lib/atrium/apps/<id>/` plus `/etc/atrium/services.d/` | platform | admin, with a press |
| `layer` | `/var/lib/atrium/layers/<name>@<version>/` (toolchains thread's contract) | platform | admin, with a press |
| `font` | `/usr/share/fonts/atrium-extra/<id>/` | platform | any person (fonts are shared; §8.1) |
| `driver` | `/var/lib/atrium/apps/<id>/` plus its `devices.d` entry | platform (device-claim scope) | admin, with a press |
| `firmware` | `/boot/firmware/<vendor>/` or `/usr/local/share/atrium/firmware/<driver>/` | platform | admin, with a press; usually pulled in by a driver |
| `command` | later (dev-tools §4.7: command bundles) | — | — |

"Admin, with a press" means a person in the machine's admin group confirming on the sheet
with a Sigillum press, as for other system changes. Kinds other than `app` are platform-only in
v1 because each of them reaches beyond one jail: services run at boot, drivers get devices,
layers enter every workspace that names them, fonts are parsed by every app.

### 4.2 The content index: the signature covers every byte

Today the signature covers `atrium.toml` and nothing ties `bin/` to it. The fix is small and
keeps the scheme:

- The packer (`opifex pack`, `opifex layer pack`) writes `content.index`: one line per
  entry in sorted path order, tab-separated `kind mode size sha256 path target` (kinds `f`,
  `d`, `l` for in-tree symlinks only; `opifex/src/content.rs`). No mtimes, owners, devices,
  hard links, setuid or setgid bits; the checker refuses all of them.
- `atrium.toml` gains `[bundle] content = "sha256:<hash of content.index>"`.
- The publisher signs `atrium.toml` as today. One signature now covers the whole tree.
- `opifexd` and portcullisd both require `content` for anything not `dev.` (portcullisd's
  launch check reads the stored index hash from the installed record, §7.5, so launch stays
  cheap).

This is the same shape the toolchains thread is using for layers today, so layers need no
change later.

### 4.3 Ids and versions

- Ids are reverse-DNS (`org.atrium.prelum`, `com.example.weather`). A publisher may publish
  only ids under the prefixes registered for it (§5.2). `org.atrium.` is the platform's.
  `dev.` ids are the machine's dev publisher's (homes.md) and are refused by the catalog and
  by `opifexd` for catalog installs.
- Versions are semver. Within one id, a version can be published once; the archive for a
  (id, version, arch) never changes (a fix is a new version). Yanking marks a version as not
  for new installs; it stays in the log.
- Architectures: `aarch64`, `amd64`. A package names the ones it ships; IR bundles
  (`form = "ir"`) are one entry for both.

## 5. Keys and publishers

### 5.1 Key roles

| Key | Held by | Signs | Lifetime |
|---|---|---|---|
| **root** (3 keys, any 2 sign) | Shankar, offline, on hardware tokens (YubiKey PIV, P-256) | `root.toml`: the other keys and their scopes; publisher recovery | years; rotated by 2-of-3 of the old set |
| **catalog** | the publishing CI (§10.1) | tree heads of the log, with an expiry | months; replaced by a new `root.toml` |
| **platform** | Shankar, on its own hardware token (§10.4) | `org.atrium.*` packages, all non-app kinds, the SDK registry | years; retired by a new `root.toml` (§5.1.1) |
| **publisher** | each publisher, in their own Arca (or any P-256 signer, e.g. cosign keyed) | their own packages | until they rotate |

`root.toml` ships in the base system at `/etc/atrium/opifex/root.toml` and is the one trust
anchor. A new `root.toml` is accepted only if 2 of the current root keys signed it.

#### 5.1.1 Replacing and revoking keys

Every key is replaced or revoked by a new `root.toml`, signed as above; machines take it at
their next sync (daily, and when Updates is opened). Until then a machine still trusts the
old key, and a catalog head older than 14 days is refused anyway.

- **Root key** lost or stolen: the next `root.toml` names a new one in its place, signed by
  two of the remaining keys (and two of the new set). Losing two before that leaves no way to
  sign (a reinstall with a new `root.toml`).
- **Catalog key**: dropped and replaced. It only signs heads, and a new head under the new
  key covers the whole log, so nothing it signed before needs it.
- **Platform key**: never dropped, because the log keeps everything it signed and a new
  machine checks the whole log. It is **retired** instead: kept with `until = <log index>`,
  next to the new platform key. Its releases and yanks before that entry still count; from
  that entry on they are skipped, listed as revoked, and never offered. Lost: `until` is the
  log's size when the new root is written (the default of `opifex catalog root-new --next yes`,
  which keeps the replaced key retired by itself). Stolen: `--retire-at` the first entry not
  trusted. Only a platform key can be retired, a retired key never comes back, and at least
  one platform key stays current.
- A machine taking a new `root.toml` checks the whole log again under it, so a machine that
  synced the thief's releases drops them as a new machine would. An installed version that
  is revoked gets a high-urgency Praeco notice once, and Updates offers the newest release
  that still counts in its place, even an older version, for a person to approve.
- **Publisher keys**: rotated or recovered through publisher records (§5.2).

### 5.2 Publishers

In the Atrium catalog the only publisher is Atrium (the platform key). Publisher records exist
so third-party catalogs (§6.5) use the same machinery, and so one catalog can hold several
publishers. A publisher is a record in a catalog: a **stable publisher id**, display name, verified
domains, id prefixes, allowed kinds, and its current and past keys. The id is assigned once
(`pub:` plus a short hash of its first key) and never changes, so key rotation does not
change who the publisher is.

- **Registering:** the publisher proves control of each domain once, when it registers, by a
  DNS TXT record `_atrium-publisher.<domain>` or `https://<domain>/.well-known/atrium-publisher`
  containing its key's fingerprint. The catalog's operator checks it when adding the
  publisher, and every machine checks it again when it first sees the publisher (§6.5). Its prefixes are the reversed domains (`example.com` gives `com.example.`).
- **Rotating:** a key change is a catalog entry signed by the old key, naming the new one.
  A lost or stolen key is replaced only by an entry signed by 2 root keys, shown to every
  machine as a recovery (§7.7).
- **Identity elsewhere:** Arca keys an app's items on (person, publisher key, app id) and
  Sigillum's private RP is `app:<publisher>/<id>`. Both should key on the **publisher id**
  instead, so a normal rotation keeps the person's secrets and passkeys; a recovery does not
  carry them over automatically (the sheet asks). Side-loaded bundles from keys outside the
  catalog keep today's key-digest identity. (Decision 6.)

### 5.3 Scopes

Every key in `root.toml` and every publisher record carries scopes, checked by `opifexd` on
every install: id prefixes, allowed kinds, and `devices` (may ship `devices.d` entries). The
flat `/etc/atrium/publishers/*.pem` set becomes the **side-load** list (§7.4) and gets the same
scope fields, default `kinds = ["app"]`.

## 6. The catalog

### 6.1 Shape

The catalog is an append-only log of entries. Each entry is one small TOML document (the
same parser the manifests already use; C1 found CBOR bought nothing):

- `release`: id, version, arch, kind, publisher id, the signed `atrium.toml` bytes and its
  signature, archive SHA-256 and size, URLs (main plus alternates), and an extract for local
  search: name, description, icon hash, capabilities, roles implemented, `[ipc] provides`,
  device matches (§8.2), font coverage (§8.1), licence (SPDX), minimum Atrium release,
  update timing (insula §14.3).
- `publisher`: new publisher, key rotation, key recovery, prefix change.
- `yank`: id, version, reason.

Entries are leaves of a Merkle tree (SHA-256, RFC 6962 hashing). The **tree head**
(size, root hash, time, expiry) is signed by the catalog key. Entries are served in bundles of
256 (`entries/000000`, the last one partial as `entries/000001.p17`), each entry a 4-byte
length and its text. Because every machine downloads every entry, it rebuilds the tree itself
and needs no inclusion or consistency proofs: it keeps the leaf hashes it accepted, and a new
head must extend them exactly. A machine that has head N fetches only the bundles from N on.

### 6.2 Why whole, why a log

- **Whole:** device ids, fonts wanted, apps looked at and links followed are all private.
  Device-assignment already decided the index is local (decision 16). With the whole catalog
  on disk, every lookup is local, and the network sees one periodic sync identical for every
  machine. Size: an entry is about 1 to 3 KB; ten thousand releases is under 30 MB, and the
  client keeps only the latest per id plus what is installed, after checking the rest.
- **Append-only log:** a catalog that could show different machines different lists could
  give one person a targeted bad update. A log forces everyone onto the same history, and
  machines can compare heads (later: with peers on the LAN, §12). This replaces what Rekor
  would have given, without depending on Sigstore's servers or sending them each install.
- **Growth:** the log only grows, so three things keep the download small. (1) After the
  first sync a machine fetches only new tiles, typically kilobytes a day. (2) A new machine
  does not replay history: it downloads a **snapshot** (the latest release of each id plus
  publisher records) signed together with a tree head, and checks the log forward from there.
  (3) An entry carries a compact extract (about 1 KB compressed); the full `atrium.toml` is
  fetched with the archive at install. For scale: 10,000 apps is about 10 MB for a new
  machine; 100,000 apps about 100 MB, a cost comparable to Debian's package lists. If the
  snapshot passes about 50 MB compressed, lookups switch to **buckets**: the catalog is
  split into 256 fixed-size parts by hash of id, a machine always holds the parts for what
  it has installed plus drivers and fonts, and fetches other parts whole, so a lookup reveals
  only one bucket among many ids, never the id.
- **Expiry:** tree heads expire after 14 days. A machine that can only fetch an old head
  (someone freezing its view to keep it on a vulnerable version) says so in Curia after the
  expiry instead of silently staying old.

### 6.3 Checks a client makes

On every sync, `opifex-check`:

1. checks the tree head's signature against the catalog key in `root.toml`, and that it
   has not expired;
2. checks the new tree extends the last one it accepted (consistency proof);
3. for each new entry: the publisher's signature over `atrium.toml`, that the publisher's
   key was current at that point in the log, that the id is in the publisher's prefixes and
   the kind in its scopes, that no (id, version, arch) appears twice;
4. writes the checked local catalog (`/var/db/atrium/opifex/catalog.db`) and the
   derived files other services read (§8.2).

Any failure keeps the previous local catalog and is reported in Curia.

### 6.4 Hosting

- Catalog: a public repo `atrium-os/catalog`, served by GitHub Pages. Releases of
  `org.atrium.*` packages: GitHub Releases on `atrium-os/catalog-blobs`. Third-party archives:
  wherever the publisher hosts them; the catalog only holds the URL and the hash.
- A domain of our own later only adds a URL; nothing is trusted by host name. The base URLs
  are a `locations` entry in the log, signed by the catalog key, so a move reaches every
  machine at its next sync; the old Pages address keeps serving, so nothing is stranded.
- Mirrors (a LAN cache, a company mirror) are additional base URLs in `opifex.toml`; any
  byte from anywhere is checked by hash.

### 6.5 Third-party catalogs

Third-party apps come from catalogs their publishers (or anyone) run. A catalog is a
`root.toml` with its own root keys, the same log and tiles, on any static host. `opifex
catalog init|add|sign|publish` and a ready GitHub Actions recipe make running one a matter of
a repository and a key.

- **Adding one:** a person adds a catalog in Curia by URL, with a Sigillum press and a plain
  sentence: "Apps from this catalog are checked by nobody but its publisher; Atrium has not
  reviewed them." Added catalogs are machine-wide (an admin) or for one person (themselves,
  apps only). Per-persona catalogs (a Work persona that installs only from the company's)
  follow personas.md, later.
- **Prefixes, checked by every machine:** a catalog may list only ids under domains its
  publishers prove (§5.2). When a machine adds the catalog, and when it first sees a new
  publisher in it, `opifex-fetch` fetches the DNS TXT or `.well-known` proof through Nuntius
  and `opifex-check` checks it. So a catalog can't publish `com.hdfcbank.` unless whoever
  controls hdfcbank.com put its key there. `org.atrium.` is never accepted from a third-party
  catalog, and an id already installed from one catalog is never replaced from another.
- **Kinds:** third-party catalogs carry `app` only in v1 (§4.1).
- **Same checks:** signed heads, consistency, publisher signatures, scopes, the install
  sheet, update rules. The sheet shows the catalog's name next to the publisher.
- **Removing one:** stops updates for its apps and says so in Curia; installed apps keep
  running until the person removes them.

### 6.6 Local queries

`opifexd` answers, from the local catalog only, with no network:

- `Describe(id)`: publisher (name, verified domain, id), latest version, capabilities with
  their plain descriptions, roles, size, licence, installed or not. The Navigator prompt and
  Monstrator use this before showing anything.
- `Search(text | role | capability | font script)`: Artifex's extension browser, the
  language-server prompt, the fonts lookup.
- `Updates()`, `Installed()`.

## 7. Install

### 7.1 Who asks, who decides

Anything may ask for an install by id (or by local bundle path, §7.4): Monstrator for an
`atrium-app://` link to an app not installed, the Navigator's app prompt, Artifex, Curia,
a Praeco driver notice, Scrinium for a double-clicked `.insula`. The asker gets back only
`installed`, `declined` or `failed`. The decision is always made on the **install sheet**, a
new Limen system role `install` provided by `opifex-sheet` (`org.atrium.opifex`), which no app
can declare (limen.md §2.0). The sheet shows:

- the app name and icon from the catalog entry, the publisher's display name and verified
  domain, and "new publisher" if the person has nothing from it yet;
- what it may do: the capabilities in plain words (portcullis's capability registry), risk
  marked, and the composition facts from app-composition §7 (which grants sit in a process
  that reads files);
- size to download and on disk, licence;
- on an update or reinstall: only what changed;
- *Install* · *Not now*. *Try it* appears only once `[trial]` exists in portcullis-toml
  (decision 10).

*Asking for another app, as built 2026-11-18 (opening-links.md L5):* `Ask::Install` has an
optional `for_uid`, the app the install is asked for. opifex-sheetd takes it only from the uids
in `OPIFEX_SHEET_FOR_OTHERS` (default `_monstrator`, 65038) and answers anyone else `Failed`;
the declines, the one-at-a-time rule and opifexd's `InstallApproved.for_uid` are then that
app's, not Monstrator's. A line without it reads as before.

### 7.2 The steps

```
asker ──Limen install──▶ opifex-sheet ──▶ person presses Install
                              │
opifexd ◀─────────────────────┘
  │ 1. look up (id, arch) in the local catalog
  │ 2. opifex-fetch: archive → /var/db/atrium/opifex/dl/<sha256> (via Nuntius)
  │ 3. opifex-check (worker, no caps): sha256 of archive = catalog; unpack into
  │    staging; refuse anything content.index can't describe; tree matches content.index
  │ 4. opifexd: verify atrium.toml.sig itself (portcullis-trust gate + scopes);
  │    content.index hash = atrium.toml; copy staging → apps/.<id>.new,
  │    hashing every file again as it copies
  │ 5. portcullis-toml validation (incl. [[workers]], [roles], [ipc]); kind-specific checks
  │ 6. atomic switch (§7.5); record; tell portcullisd (uids for workers, installed list)
  ▼
installed
```

`opifexd` treats `opifex-check`'s work as a filter, not a verdict: the security-relevant facts
(signature, scopes, per-file hashes) are checked again by `opifexd` on bytes it reads itself.
`opifex-check` exists so that the archive and CBOR parsers, the code most likely to have a
bug, run with nothing to steal and nothing to write but staging.

### 7.3 Install-time verifications

- **WebAuthn RP ids** (sigillum.md): for each `identity.webauthn` RP in the manifest,
  `opifex-fetch` fetches `https://<rp>/.well-known/atrium-apps` and `opifex-check` checks it
  names the publisher id (or key) and the app id. Verified RPs go into the installed record,
  which Sigillum reads. Unverified ones are dropped, and the sheet says so.
- **Kind checks:** drivers' `devices.d` entries need the `devices` scope; services and layers
  their own validators; fonts §8.1.

### 7.4 Local bundles (side-loading and workspaces)

A bundle folder or `.insula` file on disk installs through the same sheet and the same
`opifexd` path, without the catalog: its signature must verify against a key in the
side-load list (`/etc/atrium/publishers`, with scopes), which an admin edits in Curia with a
press. This is how a bundle built in a workspace and signed by its developer's Arca-held key
(`cargo atrium bundle --sign`, dev-tools §4.5) gets installed for real. `--allow-unsigned`
goes away; unsigned bundles run only as `dev.` ids through the dev publisher (homes.md),
which `opifexd` refuses.

Through the sheet (built 2026-11-18): the asker (Scrinium, for a double-clicked `.insula`)
sends `Ask::SideLoad` with the open file attached (`SCM_RIGHTS`, `opifex::fdpass`), never a
path; `opifex_sheet::side_load(&file)` does it, and Scrinium does the same for a double-clicked
bundle (`scrinium::install`; the file manager's `removable-files` mounts the sheet's socket). The sheet passes the descriptor to opifexd
(`PreviewLocal { for_uid }`, from the sheet's uid or root only), which copies it (at most 4 GB)
into `state/held/<token>.insula`, unpacks it with the worker, and answers `LocalPreview` only
for an app signed by a key on the side-load list and not installed from a catalog. The sheet
says it is from a file and won't update by itself; a counted press sends
`InstallLocalApproved { token, manifest_sha256, for_uid }`, which installs that held copy
only if its manifest is the one shown, for that person, and uses the token up. Unapproved
copies go after 15 minutes. VM gate `scripts/vm-opifex-c3-check.sh` 0 failed on the Mac VM
(2026-11-18, with real presses): a file signed by a key off the side-load list refused with no
sheet, the sheet for a listed key's file ("Not from a catalog: it won't update by itself"),
Install putting that copy in as a side-load for the asker, nothing left held.

### 7.5 On disk

```
/var/lib/atrium/apps/<id>/            the installed tree (what portcullisd reads today)
/var/lib/atrium/opifex/prev/<id>/     the previous version, kept for rollback
/var/lib/atrium/opifex/staging/       trees being checked (same file system, so placing is a rename)
/var/db/atrium/opifex/catalog.db      checked local catalog
/var/db/atrium/opifex/installed.toml  per id: version, publisher id, archive sha256,
                                      content.index hash, source (catalog | side-load),
                                      verified RPs, who installed, when
/var/db/atrium/opifex/dl/             downloaded archives (kept until the install settles)
```

Switching: stage under `opifex/staging/`, then rename the current tree to `opifex/prev/<id>`
(dropping the older one) and the staged tree to `apps/<id>`. Staging and the previous
version stay outside `apps/`, which portcullisd lists as the installed apps. Running instances keep the tree they started with
(their nullfs mount holds the old directory) until they restart under the update's timing
class. No change to portcullisd's layout. Deduplication between versions and between apps is
Tessera's, since `/var/lib/atrium` is on the Tessera root; when Tessera grows an ingest API,
placing becomes a tree pointer swap with no copy, without changing anything above.

The library closure: catalog packages must ship it (atrium-bundle-format §5, the packer and
`[bundle] extra-libs`); `opifexd` refuses a catalog package whose entry needs a library not in
its tree. The current install-time `DT_NEEDED` resolving stays for side-loaded bundles only, as
the bring-up exception already says.

### 7.6 People and personas

The installed tree is shared by everyone on the machine. Who may use it is per person: the
install sheet records the installing person's approval of the capabilities (portcullisd's
`ApproveAccess`, keyed by manifest hash, as the Curia Apps page does today); another person
sees the app in their grid and approves on first open. Per-persona instances, data and grants
are personas.md's and are unchanged. Removing an app a person installed removes it for them;
the tree is deleted when nobody uses it.

### 7.7 Uninstall and rollback

- Uninstall: removes the person's approval and, if nobody else uses it, the tree and
  `.prev`. The app's data in people's homes stays until they remove it on the Curia Apps page
  (as today).
- Rollback: swaps `apps/<id>` and `opifex/prev/<id>`. Offered in Curia for 30 days after an update.
  Rolling back across a capability change asks again.
- A publisher key recovery (§5.2) is shown to people who have that publisher's apps, and
  the next update of each app asks again even with unchanged capabilities.

## 8. The other kinds

### 8.1 Fonts

Today `atrium-fonts` trusts only faces whose BLAKE3 is compiled into `catalog.rs` (fonts.md).
Font packages keep that rule with a second, installed pin list:

- `kind = "font"`, platform-signed only, OFL or Apache-2.0, static TrueType (`glyf`), as
  fonts.md. The PDF writer subsets only `glyf`, so CJK ships as static TTF instances (Google
  Fonts' static Noto Sans/Serif SC, TC, JP, KR, OFL, about 10 MB a weight), not the CFF
  Noto CJK or Source Han `.otf` files.
- The catalog extract lists each face's scripts and family names, so "a Japanese serif" is a
  local search.
- `opifexd` installs into `/usr/share/fonts/atrium-extra/<id>/` and rewrites
  `/usr/share/fonts/atrium-extra/pins` atomically (temp file and rename), root-owned. One
  line per face in a format `atrium-fonts` parses itself: package id, family, file, weight,
  italic, scripts (Unicode blocks) and BLAKE3. The library matches faces from this table as
  it does from `catalog.rs`, never by parsing name tables at runtime, and maps a face only if
  its file's hash matches its line.
- Jails: the Portcullis `fonts` capability mounts `atrium-extra` read-only next to
  `/usr/share/fonts/atrium`, and the Fresco renderer bundle (`scripts/fresco-renderer-bundle.sh`)
  carries it the same way. After an install, opifexd signals frescod, which re-reads the
  pins; running apps see new faces at their next start.
- The Navigator reads neither folder (fingerprinting), so installed fonts never change what
  web pages see.
- On demand: when text needs a script no installed face covers, the app shows tofu and a
  Praeco notice offers the font package (one sheet, any person may approve, no capability
  prompt because fonts carry none).

The fonts thread owns the library side; this is the contract to agree with it.

### 8.2 Drivers and firmware

- A driver package's `devices.d` entries are in its catalog extract. After each sync
  `opifex-check` writes `/var/db/atrium/opifex/devices.idx` (match rows to package id), which
  `dispensatord` reads for a device with no installed driver (device-assignment §4.7.2).
  Nothing about the device leaves the machine.
- The Praeco notice opens the install sheet for that driver (§4.7.3), admin with a press.
- Firmware packages are platform-signed and installed into the paths device-assignment §4.4
  names; a driver lists the firmware ids it needs and they are installed with it.
- Only the platform key has the `devices` scope in v1 (device-assignment decision 3).

### 8.3 Toolchain layers

The toolchains thread defines `kind = "layer"` (portcullis-toml `layer.rs`), its install
path `/var/lib/atrium/layers/<name>@<version>/`, and is writing a first `opifex layer
install|list|remove` that checks a platform signature over `atrium.toml` naming a content index
hash. Under this spec that same command keeps working; Opifex adds catalog lookup by
`name@version` and arch, download, and update notices. Platform key only.

### 8.4 SDK crate registry

The SDK crates are a cargo sparse registry (static `config.json`, `index/`, `crates/`), served
next to the catalog. Cargo checks each crate against the index's checksum but checks nothing
about the index. So for each registry update the platform key signs `index.sig` over a file
listing the SHA-256 of every index file, and `cargo atrium` checks it before letting cargo
use the registry. The copy inside the Rust layer is covered by the layer's signature already.
Host: the same Pages site under `/sdk/` (decision 9).

## 9. Names: what is left of Nomenclator

`atrium-app://<id>` resolves by looking the id up in the local catalog (and the installed list);
that is all an app link needs. Publisher domains are proven once, at registration (§5.2),
instead of by every client on every lookup, which removes the DNS TXT lookup, the per-client
TOFU, and the privacy leak of a DNS query per app.

Nomenclator's other job, `atrium-doc://` names for documents published by domain, is not a
package question and stays in nomenclator.md, to be built when a document publisher exists.
When it is, it should reuse the publisher records and keys of §5.2 rather than its own.

## 10. Publishing

### 10.1 The pipeline

Atrium publishes by opening a pull request to `atrium-os/catalog` that adds a release file: the signed `atrium.toml`, its signature, the archive URLs, sha256 and
size. GitHub Actions runs the checks and, when they pass, appends the entry to the log, signs
the new tree head with the catalog key (a repository secret) and publishes the tiles to Pages.
`opifex publish` writes the pull request contents; `cargo atrium bundle --sign` produces the
signed bundle.

The catalog key in CI is the weakest key, and is deliberately the least powerful: it can
order and delay, not forge (§1). Root keys never touch CI.

### 10.2 Checks

The CI fetches each archive (it must, to check the hash and the tree) and checks: schema;
publisher signature; id in prefixes, kind in scopes; content.index matches the archive;
self-contained closure; portcullis-toml validation; capabilities on the review list (device
claims, `network.host`-class grants, `arca`, `embeds` of system roles) need Shankar's
approval on the pull request; atrium-ax coverage below threshold is refused unless overridden,
and an override is shown on the sheet. Third-party catalogs get the same checks as a CI recipe
to run themselves; Atrium runs nothing for them.

### 10.3 Licences

The catalog accepts any licence, shown on the sheet as SPDX. The base system's no-GPL rule is
about the base, which never depends on a catalog package. Platform-published packages follow
the base rule except dev layers, which dev-tools already allows to carry GPL tools.

### 10.4 Built elsewhere, signed here

The platform key never needs to be on the machine that builds. The signature covers only
`atrium.toml`, which names the archive's contents by the hash of `content.index`, so a remote
or cloud builder packs unsigned and the key holder signs the manifests alone, without the
archives:

- on the builder: `opifex sign --list <dir>…` prints `id version content-hash` per package;
- a second, independent build (another machine or CI runner) prints its own list;
- locally: `opifex sign <dir>… --key platform.key --expect list.txt` reads every manifest
  first, refuses all of them if any id, version or content hash differs from the list, then
  signs each `atrium.toml` (a token key file asks for the PIN once) and writes
  `atrium.toml.sig` next to it, checked against the key's public key;
- the `.sig` files go back to the builder, which publishes.

Only the few-KB manifests travel to the key, and the `--expect` list makes two builds agree
before anything is signed (a reproducible-build check). The platform key sits on its own
token, separate from the three root tokens.

## 11. Updates

- `opifex-fetch` syncs the catalog once a day, at a random time, and when the person opens
  the Updates part of Curia.
- For each installed package with a newer release, under the publisher's timing class
  (insula §14.3, the person may only make it more conservative):
  - same publisher, capabilities a subset or equal: fetched and installed in the background;
    the app picks it up at its next start;
  - capabilities added: a Praeco notice; the sheet shows only the additions;
  - different publisher id: refused, shown in Curia.
- Platform packages (`org.atrium.*`, layers, fonts, drivers, firmware) follow the same path
  with admin approval batched per sync.
- The base system itself (kernel, base userland, base daemons) is not updated through the
  catalog in v1 (decision 12).

## 12. Not in the first version

- Delta downloads. v1 downloads the whole archive. Next: `.insula` gets a per-file offset
  table so a client fetches only files whose hash it lacks, with HTTP range requests on the
  same static file. Later still, Tessera diff streams when Tessera has them.
- LAN peer cache and head comparison between machines.
- Third-party drivers, services, layers and fonts.
- Trial installs (needs `[trial]` in portcullis-toml).
- Payments: never in Opifex. A commercial app handles licences itself.
- Base system updates through the catalog.
- Per-persona catalogs.

## 13. Words

- **the catalog**: Opifex's signed log of published packages, and its local checked copy.
- **the installed list**: portcullisd's `Request::Catalog` (the apps the dock and grid show).
  Renaming the verb to `Installed` when next touched is suggested, not required.
- **the font pin table**: `atrium-fonts/src/catalog.rs`, plus the installed `pins` file.

## 14. Build order

| Step | What | Unblocks |
|---|---|---|
| **C0** | Today's `opifex`: use `portcullis-trust`'s gate instead of its own copy; `content.index` and `[bundle] content`; refuse `dev.` ids, setuid, devices, hard links, escaping symlinks; staging and rename; drop `--allow-unsigned`. Shared with the toolchains thread's `opifex layer`. | layers on the VM; signed bundles from workspaces |
| **C1** | Key roles: `root.toml` (2-of-3), platform and catalog keys, scopes; key ceremony for Shankar with hardware tokens. Catalog entry format, log, tiles; `opifex catalog append|sign|verify` tools; a local catalog directory works offline. | fonts and layers from a local catalog |
| **C2** | `opifexd`, `opifex-check` (worker), installed record, install, uninstall, rollback from a local catalog and local archives. `Describe`, `Search`. | |
| **C3** | `opifex-sheet` as the Limen `install` role; callers: Monstrator L5, the Navigator prompt, Scrinium, Curia. | L5, publisher prompt |
| **C4** | `opifex-fetch` through Nuntius (as an ordinary launched system bundle with `net-fetch`, no new Nuntius verb); `atrium-os/catalog` repo, Pages, CI pipeline and checks; first platform packages published. | everything online |
| **C5** | Updates, timing classes, Praeco notices, Curia Updates; key rotation and recovery (moved to C7, §14.6). | |
| **C6** | Kinds: fonts (with the fonts thread), drivers and `devices.idx` (with dispensatord), firmware, layers from the catalog, signed SDK registry. | CJK on demand, driver lookup, SDK |
| **C7** | Third-party catalogs: `opifex catalog init/add/sign/publish`, the Actions recipe, domain proofs checked on each machine, Curia's add-a-catalog with a press; WebAuthn RP verification; ax coverage in CI. | third-party apps |

Each step has a VM gate: C0 refuses a tampered file, a setuid file and a `dev.` id; C2
installs, updates and rolls back an app with a running instance kept on the old tree; C3 shows
the sheet from an `atrium-app://` link and a click-check that the sheet is the system's; C4
syncs from Pages through Nuntius and refuses a catalog with a forked history; C6 installs a CJK
font on demand and a driver from a `nobody` device's notice.

### 14.1 C0 as built (2026-11-17)

- `opifex/src/content.rs`: the content index (format, indexing, parsing, placing with
  per-file hashing), shared by apps and the toolchains thread's layers (`layer.rs`).
- `opifex pack <bundle> [--sign key.pem]`: writes `content.index`, sets `[bundle] content`
  (new optional field in portcullis-toml), signs `atrium.toml`.
- `opifex install`: the signature goes through `portcullis-trust`'s gate (opifex's own copy
  is gone, so `trust.toml require_signatures` now applies to installs too); `dev.` ids are
  refused; with `[bundle] content` every file is checked as it is copied into staging, and
  setuid, devices, hard links, unlisted types and escaping links are refused; the tree is
  swapped in by rename with the previous version kept; `opifex rollback <id>` swaps back.
- Not yet: bundles without `content` are still accepted with a warning, because the VM gate
  scripts sign bare `atrium.toml` files; `trust.toml require_content = true` refuses them.
  It becomes the default once the build scripts run `opifex pack`. portcullisd does not
  check the index at launch yet (C2, from the installed record). `--allow-unsigned` stays
  for `forum-on-tessera.sh` until C2.
- Checked: 10 unit tests (pack, install, update, rollback, and refusals of a changed file, a
  setuid file, an edited manifest, a missing index when required, a `dev.` id, an unsigned
  bundle). VM gate `scripts/vm-opifex-c0-check.sh` on the Mac VM (arm64) 2026-11-17: 22
  checks, 0 failed (also a file grown after signing, nothing left in staging, uninstall clears
  the kept version).

### 14.2 C1 as built (2026-11-17)

- `opifex-catalog` crate: `root.toml` (roles root, catalog, platform; scopes: prefixes,
  kinds, devices; threshold), root rotation accepted only when signed by the current
  threshold and the new one's; entries (release, yank, locations) as TOML; bundles of 256;
  RFC 6962 tree; heads signed by the catalog key with an expiry; releases checked against
  the signer's scopes and the publisher's own signature over `atrium.toml`, which must name a
  content index; the client rebuilds the tree from every entry, keeps accepted leaf hashes,
  and changes nothing on any refusal.
- `opifex catalog root-sign | add | yank | head | sync | list`, working on folders (network
  fetch is C4).
- Checked: 6 unit tests (sync across bundles, yank, publishing refusals, expired head, wrong
  head key, rewritten history, a forged release under a stolen catalog key, root rotation
  needing 2 of the old keys).
- Real keys: waits for Shankar's key ceremony (three hardware tokens for root).

### 14.3 C2 as built (2026-11-17)

- `opifexd` (`src/bin/opifexd.rs`, the work in `src/daemon.rs`): root, unjailed like
  portcullisd for now, no network; one JSON request per line on
  `/atrium/sockets/opifex/opifex.sock`. Anyone may ask `Installed`, `Describe` and `Search`
  (answered from the local catalog); `Install` (newest catalog release), `InstallLocal`
  (side-load of a folder or `.insula`), `Uninstall`, `Rollback` and `Sync` are root's until
  the install sheet (C3) carries a person's approval.
- `opifex-check`: started per archive with the bytes on stdin; chroots into an empty folder
  owned by `_opifex` (65041), drops to that uid, unpacks there. opifexd then checks the
  manifest is the catalog's, the signature (against the catalog signer's key for catalog
  installs, the side-load list otherwise) and every file against the signed index while
  copying into its own staging, and swaps it in.
- The `.insula` archive is version 2 (`src/archive.rs`): adds folders and in-tree links to
  INSB v1, which layers need; deterministic; `opifex archive <dir> <out>` writes one.
- Catalog installs must carry their own library closure (no install-time resolving); the
  archive must match the catalog's hash and size (a mismatch is deleted). Downloading is C4:
  until then archives are placed in `/var/db/atrium/opifex/dl/<sha256>.insula`.
- The installed record `/var/db/atrium/opifex/installed.toml` (id, version, kind, publisher
  key id, source, archive hash, content hash, who, when).
- CLI through opifexd: `opifex describe | search | installed | get | side-load | remove | sync`.
- Launch trust: portcullisd still checks `/etc/atrium/publishers`, so the platform key must
  be listed there too until launch reads the installed record (C5).
- Checked: host tests (archive round trip and refusals; daemon: sync, describe, search,
  person refused, catalog install, update, rollback, uninstall, missing and wrong archive,
  untrusted and trusted side-load, garbage archive). VM gate `scripts/vm-opifex-c2-check.sh` on the
  Mac VM (arm64) 2026-11-17: 0 failed.

### 14.4 C3 as built (2026-11-17)

- `opifex-sheet/` (own crate, like Monstrator): `opifex-sheetd`, jailed as `_opifexsheet`
  (65042, `etc/services.d/39-atrium-opifex-sheetd.toml`, `scripts/opifex-sheet-service.sh`),
  no network, no files. Callers connect to `/atrium/sockets/opifex-sheet/opifex-sheet.sock`
  and send one line, `Install { id }`; they hear `Installed`, `Declined` or `Failed` and
  nothing else (`opifex_sheet::install(id)` is the client).
- The sheet is opifex-sheetd's own dialog window on frescod (as Monstrator's), not yet a
  Limen slot in the caller's window: Monstrator, the first caller, has no window to put a
  slot in. It shows the name, description, id, version, catalog and signing key, what it
  may use in plain words with the ones that reach beyond its window marked, the download
  size, and *Install* (or *Update*) · *Not now*. *Not now* has the focus. *Install* counts
  only when frescod counts the press (`PROMPT`, the two rows the window's guard, a click or
  Enter). One sheet at a time; an asker who got *Not now* is answered `Declined` without a
  sheet for 30 seconds; an app already at the newest version is answered `Installed` with
  no sheet; anything not of kind `app` is refused.
- opifexd's `InstallApproved { id, manifest_sha256, for_uid }` is taken from the sheet's uid
  (`OPIFEX_SHEET_UID`, default 65042) and root only. It installs only the release whose
  manifest the sheet showed (`Describe` now carries `manifest_sha256`); a release synced
  after the sheet opened is refused. The record names the asker's uid as `installed_by`.
  The sheet can't remove, roll back, side-load or sync.
- Not yet: the Limen `install` role for windowed callers (Curia, Scrinium); "only what changed" on an update (needs the installed
  manifest's capabilities); recording the person's `ApproveAccess` with portcullisd; the
  callers themselves (Monstrator L5, the Navigator prompt), which their threads add.
- Checked: host tests (sheet state and keys, layout, plain words, opifex-sheetd against a
  stand-in opifexd, opifexd refusing a person, a forged sender and a changed release).
  VM gate `scripts/vm-opifex-c3-check.sh` (frescod headless with real presses) on the Mac VM
  (arm64) 2026-11-17: 0 failed (the sheet as `_opifexsheet`, Not now, quiet repeat, Install
  recorded for the asker, forged request refused).

### 14.5 C4 as built (2026-11-17)

- `opifex-fetch` (`src/bin/opifex-fetch.rs`), installed as the system bundle
  `org.atrium.opifex.fetch` (`scripts/opifex-fetch-bundle.sh`): one capability, `net-fetch`,
  with `net-fetch-sites` set to GitHub Pages and Releases (`atrium-os.github.io`, `github.com`,
  `objects.githubusercontent.com`, `release-assets.githubusercontent.com`), so it reaches the
  network only through a Nuntius of its own, in that fetcher's jail. opifexd starts it when it
  needs bytes with `portcullis launch --no-prompt org.atrium.opifex.fetch` (system grant),
  whose stdio portcullisd hands to the jailed app: opifexd's two pipes are its whole channel.
  The wire (`src/fetch.rs`): one JSON `get { url, max }` per line, answered by `ok { len }`
  and the bytes, or `err { why }`; https only, no credentials in URLs. No new Nuntius verb,
  no new capability, no socket of opifexd's in any jail.
- `opifex sync` with no argument syncs over the network from where the catalog says it lives
  (a signed `locations` entry), else `https://atrium-os.github.io/catalog/`
  (`OPIFEX_CATALOG_URL`); `opifex sync https://…` from a given base; a folder as before.
  Everything goes through the C1 client checks. `opifex get` (and the sheet's
  `InstallApproved`) download a missing archive from the release's URLs, keeping it only if
  its size and sha256 are the catalog's.
- The catalog repository: `portcullis/opifex/catalog-repo/` is the template for
  `atrium-os/catalog`: `publish.sh` (a pull request adding `incoming/*.toml` is checked: the
  archive downloaded, its hash checked, unpacked, and added to a copy of the log, which checks
  the signature and scopes; on main it is appended and the head signed with the catalog key,
  the only key in CI), the SDK registry under `sdk/` checked with
  `cargo atrium registry verify` before it is published, and a workflow that publishes `site/`
  to Pages and re-signs the head weekly. `opifex unarchive` added for it.
- Not yet: the repositories themselves (`atrium-os/catalog`, `atrium-os/catalog-blobs`), the
  real keys (key ceremony), the first platform packages, a daily sync timer (C5).
- Checked: host tests (the wire; sync and download through `opifex-fetch --dir` standing in
  for the web; a server handing out other bytes refused and nothing kept; a 404; a URL with
  credentials refused); `publish.sh` against a local HTTPS server (check, publish, a wrong
  hash refused). VM gate `scripts/vm-opifex-c4-check.sh` (the hermetic HTTPS fixture of
  nuntius-vm-check.sh serving the catalog; `run-vm.sh` takes `NETDEV_EXTRA` for it) on the Mac
  VM (arm64) 2026-11-17: 0 failed (sync and download through the fetcher's own Nuntius, other
  bytes refused, a host outside its sites refused, a forked history refused, no jail left). The
  Mac VM now boots with `kern.racct.enable=1`, which every Nuntius fetcher needs.

### 14.6 C5 as built (2026-11-18)

- The timing class is the manifest's `[app] updates = "auto" | "notify" | "required"`
  (portcullis-toml; anything else fails validation; none means `auto`). A person may ask for
  more care for one app (`notify`), never less; root or whoever installed the app may set it.
- `opifex::updates::decide` (`src/updates.rs`) for each installed app with a newer release
  in the checked catalog:
  - another publisher (another catalog, or another key unless both keys hold the platform
    role), or a side-loaded app: **refused**, and `install_catalog` refuses it too, so
    `opifex get` can't apply it either;
  - capabilities added (by key, with `network` compared by mode): **ask**, whatever the
    class; `Described.added` lists them and the install sheet shows only those ("It now also
    asks to: …", or "It asks for nothing new.");
  - `notify` (by the publisher or the person): **waits** for the person;
  - otherwise (`auto`, `required`): installed **in the background**, as the person who
    installed it; a running instance keeps its tree until its next start (C2).
- opifexd: `Updates` (anyone: the list, the last sync time and error; what Curia's Updates
  page reads), `SetUpdates { id, care }`, `Tick` (root). The daily round runs in opifexd
  10 to 70 minutes after start and then about every 24 hours at a time picked at random
  (`OPIFEX_DAILY=0` turns it off): sync over the network, install what may go in the
  background, then one Praeco notice naming what waits (asks or notify), each version
  announced once. Praeco: opifexd posts on `PRAECO_SOCKET` as a system poster, so praecod
  needs `PRAECO_SYSTEM_POSTERS=0:Updates` (opifexd is root).
- `opifex updates`, `opifex update-care <id> notify|auto`, `opifex tick`.
- Not yet: Curia's Updates page (Curia's owner: it reads `Updates` and sends the person to the
  sheet for an ask); batched admin approval for
  platform kinds (C6, with the kinds); **key rotation and recovery move to C7**, where
  publisher keys other than the platform's first appear: until then the only publisher is
  the platform key, which rotates through `root.toml` (2-of-3).
- Checked: host tests (unit tests of `decide` and the notice; `updates_and_the_daily_round`:
  a background update, a `notify` release waiting and announced once, added capabilities
  asked with `added` on `Describe`, more care set and cleared, only root or the installer may
  set it, only root may tick); the sheet's update lines. VM gate
  `scripts/vm-opifex-c5-check.sh` 0 failed on the Mac VM (2026-11-18, 21327c4): the daily round
  through the fetcher's file mode, one notice taken by a real praecod as "Updates", none on a
  second round, and praecod refusing opifexd when its uid isn't a listed poster.

### 14.7 C6 as built (2026-11-18)

- **Fonts** (`opifex-catalog/src/font.rs`, `opifex/src/font.rs`): `kind = "font"`, a
  `[font]` table (id, name, version, licence OFL-1.1 or Apache-2.0, description,
  `languages`, content) and one `[[face]]` per file (family, `<name>.ttf` at the package's
  top, weight, italic, code point ranges). Releases are for arch `any` (the catalog's
  `latest` takes them on every machine). `opifex font pack` refuses CFF (`OTTO`), folders and
  a face the folder lacks. opifexd copies the package against its signed index into
  `/usr/share/fonts/atrium-extra/<id>/` (`OPIFEX_FONTS`), swaps it in, and rewrites `pins`
  atomically with each face's BLAKE3 and size as placed; an update replaces the package's
  lines where they were, so install order (and Han fallback order) stays; the list must read
  back whole in `atrium_fonts::extra::parse` or nothing is written. Any person installs a
  font, through the sheet ("Fonts run nothing and see nothing; every app can draw with
  them."); updates go in the background. frescod picks the list up by itself (fonts.md §5).
- **Layers**: opifexd installs a catalog layer with the toolchains thread's
  `layer::install` (platform signature, index, `layers/<name>@<version>`); an update installs
  the new version beside the old, which stays for workspaces that name it; uninstall removes
  the recorded version unless a workspace has it mounted. Administrators only.
- **Drivers and firmware** (`opifex-catalog/src/driver.rs`, `opifex/src/driver.rs`): a driver
  manifest carries `[[match]]` tables (`usb`: `usb_ids` and/or `usb_interface`; `hid`:
  `any_page`), so the catalog itself says which devices each driver is for. The package holds
  `devices.toml` (its `devices.d` file) and its files; the installer refuses rules that claim
  devices the matches don't show, classes other than usb and hid, owners other than the
  file's own driver jails, and a driver `exec` outside
  `/usr/local/libexec/atrium/drivers/<id>/`, where its files go; the rules go to
  `/usr/local/etc/atrium/devices.d/60-<id>.toml` (root, 0644). A driver's `firmware` ids are
  installed first, from the catalog. Firmware (`place = "jailed"` or `"kernel"`, `dir`) goes
  to `/usr/local/share/atrium/firmware/<dir>/` or `/boot/firmware/<dir>/` and never replaces
  a folder it didn't place. Administrators only (`opifex get`), not through the sheet.
  dispensatord takes a new driver's devices from its next start.
- **devices.idx**: opifexd writes `/var/db/atrium/opifex/devices.idx` after every sync (not
  opifex-check: it is derived from the checked catalog). dispensatord looks a USB interface
  or HID device nobody drives up in it (`dispensator/src/catalog.rs`, nothing leaves the
  machine), adds "the catalog has <id>" to the Devices list's why, and posts one Praeco
  notice per device and package ("A driver is available"). `opifex driver-for usb
  <vvvv:pppp> [<cc/ss/pp>] | hid <page>` does the same lookup by hand.
- Updates: fonts in the background; layers, drivers and firmware wait for an administrator;
  `opifex update-all` (root) applies every waiting update that isn't refused (§11's batched
  approval).
- SDK registry: as the toolchains thread built it; the catalog repo's publish step runs
  `cargo atrium registry verify sdk --keys keys/platform` before copying it to Pages (§14.5).
- VM gate `scripts/vm-opifex-c6-check.sh` (boot with `QEMU_EXTRA="-device qemu-xhci"`) on
  the Mac VM (arm64) 2026-11-18: **0 failed** (31 checks): a font package's pin line, the
  library's kana fallback to it and its mapping, a changed file refused, removal; a catalog
  layer; devices.idx, dispensatord offering the catalog driver for a hot-plugged QEMU MTP
  device (46f4:0004); the driver installed with its firmware, its rule loaded and taking the
  device after a restart, the offer gone; uninstall. The driver's jail itself was refused by
  jaild (`devfs_ruleset 30 not in policy`): jaild pins each ruleset to named jails and
  binaries, and a catalog driver can't add to jaild's policy. **Decided (Shankar 2026-11-18): one
  shared ruleset**, packages never add to jaild's policy. Built: ruleset 33
  (`atrium_driver`, the basics only) pinned in `etc/jaild.policy.toml` to jails named
  `atrium-driver-…`, running a program in a package folder directly under
  `/usr/local/libexec/atrium/drivers/` (`exec_dir`), as a uid in 61000-61499 (`uid_last`),
  shown only `ugen#.#`, `usb/#.#.0` or `hidraw#` through devfs_grant (jaild now grants the
  node to the request's uid). opifex refuses a driver file with another ruleset, a uid
  outside the range or not equal to its gid, another jail name, or a uid another installed
  driver has. The VM gate checks the driver's jail starts and runs as its uid once this
  tree's jaild, policy and devfs rules are deployed. Rerun 2026-11-18 with ruleset 33
  loaded: jaild then refused dispensatord's read-only mount of the driver's package folder
  (read-only sources were exact paths only). Fixed in jaild: `[mount_sources] ro_subtrees`
  takes a folder directly inside a listed one, never the listed folder itself, so a driver
  sees only its own package (`/usr/local/libexec/atrium/drivers/`). Rerun the same day with
  that jaild: **0 failed** (31 checks); jaild started the driver's jail
  (`atrium-driver-c6mtp-0-2-0` via `ugen0.2,usb/0.2.0`) and it runs as its uid. (The
  `jid 0` in dispensatord's line is jaild's sentinel for exec'd jails, not a failure.)
- opifex checks a driver's `devices.d` file together with the installed ones through
  `dispensator::rules`, so one bad package can't stop dispensatord from loading every rule.
- CJK packages (2026-11-18): `scripts/opifex-cjk-fonts.sh <out> --sign <platform key>` builds
  Noto Sans JP, SC, TC, HK and KR 2.004 (regular and bold, OFL) from the
  `@expo-google-fonts/noto-sans-*` 0.4.4 tarballs, each pinned by its npm sha512, with
  `incoming/<id>.toml` requests for the catalog-blobs release `fonts-noto-cjk-2.004`.
  `opifex/tests/cjk_fonts.rs` installs all five and checks atrium-fonts picks each one's forms
  by hint; a scratch catalog took all five (`catalog add`, sync). Published once the platform key
  and the repositories exist.
- Not yet: frescod drawing an installed CJK font from the catalog (the fonts thread checked
  frescod with an installed package; the gate checks the library); the notice opening Curia's install page for a driver
  (it names the command for now); PCI devices in the lookup (dispensatord sees USB and HID
  only); the driver jails' uids in jaild's policy are the package's to have accepted at review
  (§10.2), not checked by the installer.
- Checked: host tests (font manifest and pin list round trip, update in place, refusals;
  layers from the catalog with an update beside the old; a driver installed with its firmware
  from the catalog, devices.idx after sync, the sheet refused for it, uninstall; rule
  coverage; firmware placement; lookups; dispensator's planner offering a catalog driver for
  a nobody device).

### 14.8 C7 as built (2026-11-18)

- **Publisher records** (`opifex-catalog/src/publisher.rs`): log entries of type
  `publisher` with an action. `register` gives a stable id (`pub:` + the first 16 hex of
  its key's id), a name, domains (lowercase host names; prefixes are the reversed domains),
  kinds and a key; it is signed by that key. `rotate` names a new key, signed by the current
  and the new key; a past key never comes back. `scope` changes name, domains or kinds,
  signed by the current key. `recover` replaces a lost key, signed by the catalog's
  root-key threshold and the new key, and counts a recovery. No two publishers share a key
  or a domain. A release may name a `publisher`; its signer must be that publisher's
  current key, the id in its prefixes, the kind in its kinds. A publisher's current key
  yanks any of its releases. Publishers keep past keys' PEMs (`past_keys`), so a tree
  signed before a rotation still checks.
- **Third-party catalogs**: the machine decides by location (the system's catalog in
  `catalog/`, the others in `catalogs/<slug>/`), never by what a catalog says. A
  third-party catalog carries apps only, never `org.atrium.`, never `devices`, and
  releases only through publisher records (`Scope`, `Client::third_party`,
  `Log::third_party`). `Log::append` replays the log exactly as a machine does
  (`Log::replay`).
- **Following catalogs** (`opifex/src/catalogs.rs`): `catalogs/<slug>/` holds the client
  state, `source.toml` (URL, the approved root.toml's sha256, for everyone or for the
  person who added it, who, when) and `proofs.toml`. The slug is the host name (`-2`,
  `-3` for more on one host), and the installed record's source is `catalog:<slug>`, which
  has a dot, so it never meets the Atrium catalog's `catalog:Atrium`. `Describe`, `Search`
  and installs look in the catalog an id came from if it is installed (never replaced from
  another), else the Atrium catalog, then the person's other catalogs in order. A
  catalog someone adds for themselves is seen by them only.
- **Domain proofs, per machine**: after each sync of an added catalog, opifexd checks every
  publisher domain it hasn't checked yet: the TXT record `_atrium-publisher.<domain>`
  through Nuntius (opifex-fetch's new `Txt` ask, `nuntius::Lookup`), else
  `https://<domain>/.well-known/atrium-publisher`, either holding the line
  `atrium-publisher pub:…`. The proof names the publisher id, so a rotation needs no new
  proof; a domain added by `scope` does. A release is offered only once the domain whose
  prefix holds its id is proven; proofs of domains a publisher dropped are forgotten.
- **Two fetchers**: the Atrium catalog keeps `org.atrium.opifex.fetch`, narrowed to its
  hosts. Everything else (added catalogs, their archives, proofs, WebAuthn files) goes
  through `org.atrium.opifex.fetch-any`, the same binary with `net-fetch` to any https host
  (`scripts/opifex-fetch-bundle.sh --any`; `OPIFEX_FETCH_ANY`). Neither is trusted for
  anything it brings.
- **opifexd requests**: `Catalogs`; `PreviewCatalog { url }` (anyone: name, root.toml
  sha256, the warning); `AddCatalog { url, root_sha256, machine, for_uid }` (from the sheet
  for one person, `machine` only from root), refused if root.toml changed since the
  preview, undone if the first sync fails; `RemoveCatalog { slug }` (root, or the person
  who added it): the catalog's apps stay installed and their updates stop. A full `Sync`
  syncs every followed catalog and reports each in `others`; an Atrium failure no longer
  stops the others. `Describe { id, for_uid }`: the sheet describes as the person sees it.
  Described gains `publisher_name`, `publisher_domains`, `third_party`, `recovered`.
- **Updates**: the installed record keeps the publisher id, so a rotated key updates as the
  same publisher; after a recovery (record's `recoveries` behind the publisher's) the next
  update asks on the sheet even with nothing new asked for (§7.7), with "its publisher's
  key was replaced by a recovery".
- **The sheet** (`opifex-sheet`): `Ask::AddCatalog { url }` (`opifex_sheet::add_catalog`)
  shows "Add the catalog <name>?", its address, the warning (marked), "Its apps will be
  offered to you only", and its keys' hash; a counted press on Add follows exactly that
  root.toml for the asker (`Answer::Added`). An app from an added catalog names its
  publisher and proven domains and says "Atrium has not reviewed this catalog's apps."; an
  update after a recovery is marked.
- **Running a catalog** (`opifex catalog …`): `init` (root keys, a catalog key, a root.toml
  signed by the threshold, a first head), `register` (prints the TXT and `.well-known`
  lines), `publisher rotate|scope|recover`, `publishers`, `publish` (= `head`); `add` finds
  a publisher by its signature; a catalog with no platform key is handled as a third
  party's by the tools (`Log::for_dir`). The `catalog-repo` template, its workflow and
  `publish.sh` are the recipe for anyone (README, "Running a catalog of your own").
  CLI for machines: `opifex catalogs | follow <url> [<sha> [--everyone]] | unfollow <slug>`.
- **WebAuthn RP ids** (`opifex/src/rp.rs`, §7.3): an app's `identity.webauthn` RP ids are
  each checked against `https://<rp>/.well-known/atrium-apps` (lines `<publisher> <app id>`:
  the publisher id, or the platform key's id), at most 8; the ones that name the app go
  into the record's `verified_rps`, the rest are dropped and logged.
- Checked: host tests (publisher lifecycle and refusals in `opifex-catalog`; opifexd end to
  end with an added catalog served through the fetcher: preview, add from the sheet,
  visibility per person, an unproven publisher's app never offered, install for the person,
  a WebAuthn RP verified and one dropped, a rotation updating in the background, a recovery
  asking, removal keeping the app; the sheet's lines and its add-a-catalog decision; TXT
  answers in the fetcher; jaild's `ro_subtrees` refusals); the publishing tools run by hand
  (init, register, add, publish, rotate, a past key refused, sync). VM gate
  `scripts/vm-opifex-c7-check.sh` 0 failed on the Mac VM (2026-11-18, 56c424a): a third-party
  catalog made with `opifex catalog init`, followed after its preview, a proven publisher's app
  offered and installed for one person, an unproven publisher's never offered, WebAuthn RPs
  verified, rotation and recovery, a forged entry refused with the copy unchanged.
- Launch trust for apps from added catalogs (2026-11-18): when `installed.toml` says an app
  came from `catalog:<slug>` and `catalogs/<slug>/` exists, portcullis-trust's gate verifies
  its `atrium.toml.sig` against that record's publisher in the catalog's `catalog.toml`,
  its current key or a past one, and only if one of the publisher's prefixes covers the app
  id. Never unsigned, and `/etc/atrium/publishers` doesn't sign for it. A publisher the
  catalog no longer lists, or a malformed `installed.toml`, refuses the launch. Every
  launch vector shares the gate (Launch, the session path, one-shots, atrium-launch).
  Host test `third_party_catalog_apps_take_their_publishers_keys`.
- Not yet: Sigillum reading `verified_rps` and keying private
  RPs on the publisher id; Curia's add-a-catalog page and a link that opens the sheet
  (both call `opifex_sheet::add_catalog`); per-persona catalogs (personas.md, later);
  atrium-ax coverage in the publishing check, which needs an ax measurement tool that
  doesn't exist yet (atrium-ax.md §5.3).

### 14.9 Root keys on hardware tokens (2026-11-18)

(Side-loading through the sheet, built the same day, is described in §7.4; host tests
`a_file_opened_through_the_sheet` in opifexd's tests, the sheet's `decide_local` with a stand-in
opifexd, and `fdpass`.)


- `portcullis-token`: a key on a PKCS#11 token (YubiKey PIV through Yubico's `libykcs11`,
  Nitrokey or any module, loaded at run time) signs with ECDSA P-256 over the SHA-256 digest;
  the signature is checked against the token's public key before use and comes out in the
  same DER form as a PEM key's. A small **key file** names the key (`atrium-token-key = 1`,
  `module`, `serial`, `id`); it holds nothing secret.
- `portcullis_sig::set_external_signer`: `sign` and `public_pem` hand a non-PEM key file to
  the signer a tool installed. Only the `opifex` CLI installs it, so every `--key`/`--sign`
  (`root-sign`, `yank`, `register`, `publisher`, `pack`, `head`) takes a token key file;
  daemons never install it.
- The PIN is typed at the terminal with echo off, once per token per run, never an argument,
  file or environment variable. A key that wants the PIN for every signature (YubiKey slot
  9c) gets a context-specific login; slot 9a needs only the one.
- `opifex token list|ref|pub`; `opifex catalog root-new` writes an unsigned `root.toml` from
  public keys or key files (three root keys, a catalog key, the platform key with every kind
  under `org.atrium.`), `--next yes` the one after it (version + 1, signatures cleared).
  `root-sign`'s `--pub` is now optional.
- Handover (a new owner, or admins holding root slots): a next `root.toml` naming the new
  keys, signed by the threshold of the old root keys and of its own; machines adopt it at
  their next sync. After that the old keys alone can change nothing. Losing more root keys
  than the threshold allows before a handover leaves only a reinstall with a new `root.toml`.
- Checked: unit tests on SoftHSM (listing, public key, signing, a wrong PIN, a missing
  token, a catalog root through `portcullis_sig`), and `scripts/opifex-token-check.sh`
  (host, SoftHSM + pkcs11-tool, PINs through a pty): the ceremony with three token keys, a
  handover to three new tokens adopted by a machine, a root signed by the old keys alone
  refused, a wrong PIN signing nothing. 0 failed. Not yet tried on a real YubiKey.

### 14.10 Retired and revoked keys (2026-11-18)

- `opifex-catalog`: `Key::until`, `Local::revoked`; entries signed by a retired key at or
  after `until` are skipped on machines and refused by `Log::append`; a new `root.toml`
  replays the log from entry 0 (against the leaves already seen).
- `opifex catalog root-new --next yes [--retire-at N]` keeps a replaced platform key, retired.
- opifexd: `revoked_installed`, the round's high-urgency notice, Updates offering the newest
  release that counts.
- Checked: unit test `retired_platform_key` (a stolen key's release and yank skipped, the same
  catalog on a synced machine, a fresh machine and the publishing side), opifexd test
  `a_revoked_install`, `scripts/opifex-token-check.sh` (root-new keeps the old platform key
  retired; a retired key can't come back): 0 failed.

## 15. Decisions, with recommendations

1. **Each catalog downloaded whole** for all lookups, not per-publisher DNS manifests or
   per-package queries. *Recommend yes.* **Decided (Shankar 2026-11-17):** the Atrium catalog
   holds only what Atrium builds; third parties run their own catalogs (§6.5).
2. **Keyed P-256 signatures as built**, no Sigstore keyless, no ed25519 or minisign for
   packages. Fix bundle-format.md and insula §12 to match. *Recommend yes.*
3. **Content index named in the signed manifest** (§4.2), required for every non-`dev.`
   install and launch. *Recommend yes.* This touches portcullis-trust; the platform and
   toolchains threads have been told.
4. **Catalog as an append-only Merkle log in static tiles**, heads expiring after 14 days.
   *Recommend yes.* Alternative: a signed TOML-per-version git repo (simpler, but no
   protection against showing different machines different histories).
5. **Key roles**: 2-of-3 offline root keys on Shankar's hardware tokens, platform key,
   catalog key in CI, publisher keys with scopes. *Recommend yes.* This needs three P-256
   capable tokens (YubiKey PIV works; Shankar has one YubiKey 5).
6. **Stable publisher id**, and Arca and Sigillum key app identity on it instead of the raw
   key. *Recommend yes.* Arca and Sigillum change one key field each.
7. **Binary only**; source builds belong to workspaces. *Recommend yes.*
8. **Process split**: `opifex-fetch` (Nuntius), `opifex-check` (no-capability worker),
   `opifexd` (privileged, re-checks), `opifex-sheet` (Limen system role `install`).
   *Recommend yes.*
9. **Hosting on GitHub**: catalog repo with Pages, platform archives on Releases, SDK
   registry under the same Pages site; a domain later only adds a mirror URL. *Recommend
   yes.* Question for Shankar: is there a domain you want used (`atrium.dev`,
   `atrium-os.org`), or GitHub Pages for now?
10. **No *Try it* until `[trial]` exists**; the sheet offers Install and Not now.
    *Recommend yes.*
11. **Non-app kinds platform-only in v1**, installed by an admin with a press (fonts by any
    person). *Recommend yes.*
12. **Base system updates stay out of the catalog in v1** (they need their own A/B and boot
    rollback design). *Recommend yes.* Alternative: carry them as a `kind = "system"` from
    the start.
13. **Any licence in the catalog**, shown on the sheet; base rule unchanged. *Recommend yes.*
14. **Retire atrium-pkg.md, atrium-pkg-registry.md and atrium-pkg-install-ux.md** (marked
    superseded, kept for history) and narrow nomenclator.md to `atrium-doc://`. *Recommend
    yes.*
15. **Build C0 now** (it is shared with the layer work and fixes a real gap: files are not
    covered by any signature today), the rest after acceptance. *Recommend yes.*
