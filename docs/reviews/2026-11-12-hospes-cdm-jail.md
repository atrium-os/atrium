# Hospes: the CDM's own jail and user (2026-11-12)

The first open item after H4/H5. `docs/spec/hospes.md` §3 puts `hospes-cdm` in "its own
one-shot jail" and §8.4 has a broker pass "a directory fd limited with `cap_rights_limit`".
Until now the player started `hospes-cdm` itself, inside the player's own sandbox, with an
unrestricted store fd.

**Done**, and checked in the FreeBSD arm64 VM (`scripts/hospes-cdm-jail-vm.sh`, all checks
pass).

## How it works now

1. **The capability.** An app whose installed manifest holds `protected-media` can ask
   portcullisd for a CDM. The user sees this consent line: "Play protected (DRM) media through
   a separately jailed decryption module". The jail of such an app gets portcullisd's socket,
   as it does for `net-fetch`.
2. **The request.** The app sends `OpenCdm { key_system, origin }`. portcullisd identifies the
   app by its peer uid, through the launch registry, and reads its installed manifest. Nothing
   the app sends is trusted.
3. **What portcullisd chooses.** portcullisd picks the library and the store. The app names
   only the origin.
   - **The library** is `/var/db/atrium/cdm/<key system>/current/`, from a fixed table:
     Widevine, and the Clear Key test CDM. It is opened read-only.
   - **The store** is `/var/db/atrium/cdm-store/<app>/<key system>/<origin>/`. The origin is
     percent-encoded, so it is one path component and never `.` or `..`. The levels above it
     are 0711 and root's; the leaf is 0700 and the host user's. A store is keyed by the app,
     so one app can never reach another app's stores.
   - **Rights.** Both are limited with `cap_rights_limit`. The library may only be read and
     mapped. The store gets only the record operations: lookup, create, read, write, seek,
     fstat, ftruncate, fsync, rename and unlink.
4. **The host.** `org.atrium.hospes-cdm`, a bundle holding only the static `hospes-cdm`,
   runs in a one-shot jail as `_hospes-cdm` (uid 65027, added to jaild's allowed system
   uids). It runs under a system worker rule, never as the app. The jail has no network and
   no capabilities, and a memory limit is required.
   - portcullisd refuses an `ExecInstance` of the host.
   - The jail is removed when the app hangs up.
5. **The hand-over.** The app sends three descriptors: its control socket, its data socket and
   stderr. portcullisd gives the host a socket of its own as stdin. Over it, portcullisd sends
   the four descriptors (control, data, library, store) with `SCM_RIGHTS`, beside a line
   `KEY_SYSTEM MEM_MIB`.
6. **The host narrows again.** `hospes-cdm` with no arguments reads that hand-over. On every
   start path it narrows the library and the store with `cap_rights_limit` itself, before it
   loads anything and before `cap_enter`. The spec already listed `cap_rights_limit` among
   the runtime's calls. `hospes-rt` gains `recv_fds` and `cap_rights_limit`.
7. **The player.** `ATRIUM_PLAYER_CDM=system:com.widevine.alpha` (with
   `ATRIUM_PLAYER_CDM_ORIGIN`) asks portcullisd. A host started by portcullisd is not the
   player's child, so the player watches portcullisd's connection for its end. Hanging up ends
   the host. Starting `hospes-cdm` directly stays for development and the VM checks.

## Checks

On Linux, the H3 Clear Key transcript still equals the reference host's on x86_64 and arm64.
It also matches through the new no-argument start path. `hospes-build.sh cdm-check-linux`
now runs that path too.

In the FreeBSD arm64 VM (fork kernel, `hospes_trap.ko`, the real portcullisd and jaild):

| Check | Result |
|---|---|
| Clear Key transcript through `OpenCdm` equals the reference host's | PASS, 47 lines |
| Store `…/org.atrium.test.cdm-probe/org.w3.clearkey/https%3A%2F%2Fcheck.example`, 0700, owned by `_hospes-cdm` | PASS |
| The CDM's records in it were written by uid 65027, not the app's | PASS, 2 files |
| A second app, same origin: its own CDM and its own store | PASS |
| An app without `protected-media` is refused (`cap.protected_media.denied`) | PASS |
| `ExecInstance` of `org.atrium.hospes-cdm` is refused | PASS |
| No CDM jail or process left | PASS |

The first run found two setup gaps, both fixed:

- **The host's uid.** jaild refuses uids outside its allowlist, and 65024 (the first choice)
  is reserved for Vinculum. The host now uses 65027, and `etc/jaild.policy.toml` allows it.
- **The fork kernel.** `hospes-k1-vm.sh kernel` boots it only once (`nextboot`), so it has to
  be run again after the VM restarts.

## Not covered

- **Widevine on this path: done 2026-11-12.** The player now runs as a jailed test app, and
  real Widevine plays through `OpenCdm` and `OpenFetcher` (`2026-11-12-hospes-jailed-widevine.md`,
  `JAILED=1 scripts/hospes-h4-vm.sh`).
- **The bhyve wrapper** (optional) is not built.
- **Wiping and listing stores** in settings ("protected content licences for <origin>",
  §8.4) has no UI yet. The directory layout makes both one directory per origin.
