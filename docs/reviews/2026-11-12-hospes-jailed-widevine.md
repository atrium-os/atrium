# Hospes: real Widevine through the jailed system path (2026-11-12)

This closes the first item under "Not covered" in `docs/reviews/2026-11-12-hospes-cdm-jail.md`:
Widevine itself had not run on the `OpenCdm` path, because the player could not yet run as a
jailed app.

**Done.** Shaka's angel-one stream plays to the end in the FreeBSD arm64 VM with every part
running where the system puts it:

- **The player** runs as an installed app, in an app jail of its own, with only `net-fetch` and
  `protected-media`.
- **The CDM.** The player asks portcullisd for a CDM (`OpenCdm`). portcullisd starts
  `hospes-cdm` as `_hospes-cdm` in a one-shot jail, with the app's and origin's store and the
  system's Widevine.
- **The licence.** The player asks for a fetcher (`OpenFetcher`). portcullisd starts `nuntiusd`
  as `_nuntius` in a networked jail of its own, and the licence goes out through it.

| Clip | Frames | Last frame | Audio frames | Licence requests | Time |
|---|---|---|---|---|---|
| `v-0144p-0100k-libx264.mp4` | 1,500 (1,473 distinct) | 59.96 s | 2,880,648 | 1 (200) | 30 s |
| `v-0144p-0100k-vp9.webm` (with the manifest's PSSH) | 1,500 (1,473 distinct) | 59.96 s | 2,880,648 | 1 (200) | 33 s |

For each clip, the check also confirmed:

- The worker was sandboxed.
- The store `/var/db/atrium/cdm-store/<app>/com.widevine.alpha/https%3A%2F%2Fshaka-player-demo.appspot.com`
  is 0700 and owned by `_hospes-cdm`.
- portcullisd's log shows one CDM jail and one fetcher jail for the app's uid.

Afterwards no `hospes-cdm` or `nuntiusd` jail or process was left.

The same WebM run also passes with the Laminar scheduler (`kern.sched.name=Laminar`).

## What it took

### The player as an app

Commit `17433ac1` did this part.

- **The test bundle.** `JAILED=1 scripts/hospes-h4-vm.sh` builds one test bundle per clip:
  `org.atrium.test.widevine-mp4` and `-webm`.
- **A launcher entry.** `portcullis launch` gives an app no arguments or environment, so the
  bundle's entry is `atrium-player-bundle-launch`. It reads `play.conf` (`env K=V` and `arg X`
  lines) and execs the player.
- **Library paths.** opifex stages only the entry's library closure, so the harness copies the
  player's and decoder's libraries into the bundle. Libraries from packages go under `/usr/lib`,
  because a jail has no `ld-elf.so.hints` naming `/usr/local/lib`.
- **Fetcher diagnostics.** `NUNTIUS_FETCHER_STDERR=inherit` passes the fetcher's diagnostics to
  the caller. When the fetcher never ran, the player now says why.

### The fork kernel, rebased onto upstream main

**What blocked it.** The licence never went out. jaild refuses a networked jail unless the
firewall's isolation rules are loaded (`network.routed.isolation_not_loaded`). Under our fork
kernel, the guest's `pfctl` could not load them ("DIOCOSFPFLUSH: Operation not supported").
The stock kernel loaded them fine.

**Why.** The VM runs the newest arm64 snapshot (16.0-CURRENT, 2026-11-09). Its `pfctl` and
`libpfctl` speak pf's netlink interface, including the OS-fingerprint commands. Our fork was
based on upstream from about 2026-09-21. Its `pf` predates those commands, so they returned
`EOPNOTSUPP`.

**The fix.** Shankar chose to move the kernel up to the userland: "pull latest kernel and
userland and rebase". The userland was already the newest snapshot, so only the kernel moved.

- **The branch.** The fork's 150 commits were rebased onto upstream main `83c8ec72a`
  (2026-11-12). The result is `atrium-os/freebsd-src` branch `atrium-os-rebase`.
- **Conflicts.** Three needed resolving:
  - `sys/conf/files` and `options`: upstream moved the scheduler entries.
  - Two in `vt_core.c`, from the boot splash. Upstream now checks `VDF_SPLASH` under the
    flush lock, and the splash animation runs from that check.
- **One commit dropped.** Our jailmeta empty-key fix is upstream now, with its own test, so our
  copy of the fix and its test were dropped.
- **One commit added: the Laminar scheduler on upstream's new scheduler interface.**
  - What changed upstream: each scheduler's `struct sched_instance` is now generated from
    `SCHED_IMPLEMENTATION` and a generated `sched_<name>_ns.h`, and `active_sched` is private
    to `sched_shim.c`.
  - Laminar now follows ULE: `DECLARE_SCHEDULER("Laminar")`, and `sswitch` renamed to `switch`.
  - It asks the new `sched_active_instance()` whether it is the running scheduler.
  - The fork's `sched_sizeof_thread_max()` calls the renamed field.
- **Build and boot.** The kernel (`ATRIUM-NODEBUG`, arm64) builds clean. `hospes_trap.ko` was
  rebuilt against it. Under it, `pfctl -f /usr/local/etc/atrium/pf.conf` loads the rules.

**Fork commit ids changed.** The rebase gives every fork commit a new id. Older notes cite
the ids on branch `atrium-os`, which is unchanged on GitHub. The two Hospes commits are:

| Commit | On `atrium-os` | On `atrium-os-rebase` |
|---|---|---|
| The systrap hook | `c12a0dadc` | `abbb1e10f` |
| `p_systrap` move | `501a0bb0a` | `27b310d5f` |

**Pushing.** This checkout is shallow, so it could not push to GitHub. The branch was pushed from
a second clone that holds upstream main deep enough to reach the fork's old base.

**Not done:** `atrium-os` itself still points at the old base. Replacing it with
`atrium-os-rebase` is a force-push, and it waits for Shankar.

## The check

```
JAILED=1 SRC=/home/user/freebsd-src ARCH=aarch64 sh scripts/hospes-h4-vm.sh   # TRACE=1, VIDEOS=…
```

The fork kernel must be the rebased one. `hospes-k1-vm.sh kernel` installs it.

## Not covered

- **The amd64 VM.** It was not rerun. Its kernel would need the same rebase build (`ARCH=amd64`).
- **Other ATRIUM kernel configs.** Only `ATRIUM-NODEBUG` (arm64) was built after the rebase.
  The KASAN and SYZKALLER configs, and the sysfil ATF tests, were not rerun on the rebased kernel.
- **The real CDM is not in CI.** This is unchanged: it stays manual until the licence.
