# Fresco-on-FreeBSD POC — Runbook

Operational reference for the Fresco scenegraph FreeBSD POC. Update this file whenever a new gotcha is found, a command line changes, or a directory moves.

**Naming.** *Fresco* is the protocol/architecture name (a retained-mode, content-addressed scenegraph protocol). The reference server lives at `~/src/fresco-server/` (Cargo crate `fresco-server`). It was renamed from `karythra-gpu-server` after this BSD port began; karythra-os was updated in lockstep. Components: `fresco.ko` (kernel module), `libfresco` (userspace), `slint-fresco` (Slint backend), `/dev/fresco0`.

Project context lives in claude-memory (`~/.claude/projects/-Users-girivs-src-bsd/memory/`); architectural decisions live in commit messages and the gpu-server source. This file is for **how to do things** — recipes, paths, quirks.

---

## 1. Tree layout

```
~/src/bsd/
  RUNBOOK.md              # this file
  vm/                     # VM disk + EFI firmware (NOT checked into git)
    vm.qcow2              # FreeBSD 16.0-CURRENT arm64 disk
    vm.qcow2.xz           # compressed original (kept for re-extract)
    edk2-aarch64-code.fd  # EFI code (padded to 64 MiB)
    edk2-arm-vars.fd      # EFI vars (64 MiB blank)
  tarballs/               # downloaded base/src/kernel .txz
  sysroot/                # extracted base.txz (headers+libs only) — cross-compile root
  freebsd-src/            # extracted src.txz — for kernel module headers + bsd.kmod.mk
  scripts/
    run-vm.sh             # boot VM (--gpu attaches ivshmem-doorbell)
    first-boot-setup.exp  # one-shot expect script (already run; kept for reference)
    vssh                  # ssh into running VM
  fresco-kmod/            # fresco.ko kernel module source (built IN-VM)
  libfresco/              # userspace C library — phases 1-4 done:
                          #   examples/smoke         transport round-trip
                          #   examples/upload_blob   CAS upload + dedup
                          #   examples/hello_rect    slot graph → orange square on host
                          #   examples/event_loop    kqueue mouse/key event stream
  fresco-rs/              # safe Rust bindings around libfresco
                          #   (formerly slint-fresco — Slint dropped over GPLv3)
  fresco-text/            # rustybuzz + swash → glyph atlas, no FreeType
  atrium-term/            # vertical app #1: terminal emulator
                          #   forkpty + vte parser + USB-HID keymap + kqueue loop
                          #   verified: interactive shell, ls, scrollback
  atrium-edit/            # vertical app #2: text editor
                          #   buffer + cursor + insert/delete/save
                          #   verified: load file, edit, Ctrl+S writes back to host
  test-assets/            # sample data — fonts, PNGs, scratch.txt — NOT in git
                          #   (multi-MB; bundled here so apps don't depend on pkg)
                          #   src/sys.rs            raw FFI to libfresco
                          #   src/lib.rs            safe Rust wrapper (Connection, blob:: builders)
                          #   src/platform.rs       slint::platform::Platform impl
                          #   examples/hello_rect   Rust port of libfresco's hello_rect
                          #   examples/hello_slint  HelloWorld Slint component runs through us
```

**Vendored external clones** (under `~/src/bsd/external/`, gitignored
— each is its own git working tree with its own remote):

- `external/qemu-build/` — Atrium-patched QEMU (HVF + ivshmem +
  async-fence + venus-hostmem patches). Origin
  `gitlab.com/qemu-project/qemu`; ahead by Atrium-specific patches.
- `external/virglrenderer/` — Atrium-patched virglrenderer. Origin
  `github.com/atrium-os/virglrenderer atrium/main`; key macOS-host
  fix is the socketpair-based eventfd emulation (commit `52903fdb`).
- `external/mesa/` — Atrium-mesa fork. Atrium remote is
  `github.com/atrium-os/mesa`; venus userspace ICD with
  `vn_renderer_atrium.c` backend.
- `external/MoltenVK/` — Atrium-patched MoltenVK. Origin
  `github.com/atrium-os/MoltenVK`. Used by venus's worker process
  to drive Apple GPUs through Metal.
- `external/slang-bin/` — slangc compiler binary distribution
  (atrium-core/atrium-text bundle SPIR-V build).

**Other Atrium-related siblings (not under `external/`):**
- `~/src/fresco-server/` — Fresco reference server (Rust crate
  `fresco-server`, Metal backend on macOS). Predates the BSD port;
  not vendored. Renamed from `karythra-gpu-server`.
- `~/src/karythra-os/` — the OS that originated the Fresco protocol.
  Wire-protocol changes stay in sync across this repo, `fresco-server`,
  and `libfresco`.

---

## 2. VM lifecycle

### Boot (no GPU server attached — smoke test)
```sh
~/src/bsd/scripts/run-vm.sh
```
- Boots with `-nographic`, serial on stdio.
- HVF accel, 4 vCPU, 4 GiB RAM, GIC v3.
- DHCP gives `10.0.2.15` (qemu user-net), gateway `10.0.2.2`.
- SSH host-fwd: `localhost:2222 → guest:22`.
- 9p share `bsd_share` exposes `~/src/bsd` to guest at `/mnt/host` (auto-mounts via fstab).

### Boot with GPU server attached
```sh
# Terminal 1 — start the GPU server first
cd ~/src/fresco-server && cargo run --release

# Terminal 2 — boot the VM with ivshmem-doorbell
~/src/bsd/scripts/run-vm.sh --gpu
```
The script aborts if `/tmp/fresco-shmem.sock` does not exist when `--gpu` is given.

### SSH into running VM
```sh
~/src/bsd/scripts/vssh                    # interactive
~/src/bsd/scripts/vssh "uname -a"         # one-shot
~/src/bsd/scripts/vssh "kldload p9fs; mount -t p9fs -o trans=virtio bsd_share /mnt/host"
```
Key: `~/.ssh/fresco_bsd_ed25519`. Root password (for serial console fallback): `fresco`.

### Copy files to/from VM
Two channels:
- **9p share** — host's `~/src/bsd/` is the guest's `/mnt/host/`. Cross-compile binaries into `~/src/bsd/<crate>/target/...` on host; they appear in the VM at `/mnt/host/<crate>/target/...`.
- **scp over the SSH forward** —
  ```sh
  scp -i ~/.ssh/fresco_bsd_ed25519 -P 2222 some-file root@localhost:/root/
  ```

> ⚠️ **Do NOT `execve` a binary directly off the 9p mount — it panics p9fs.**
> Running (especially several concurrent) binaries from `/mnt/host/.../target/...`
> demand-pages them over 9p; `kern_execve → namei → p9fs_lookup → p9fs_vget_common
> → vfs_hash_insert → vput_final → freevnode` hits a vnode double-free and the
> kernel panics into ddb (`freevnode: ...`). Recovery: `python3 scripts/ddb_session.py
> "reset"` (ZFS root reboots clean). **Copy binaries to local ZFS first, then run
> them there:** `cp /mnt/host/<crate>/target/.../<bin> /root/wmtest/ && /root/wmtest/<bin>`.
> Prefer `--release` cross-builds (≈5 MB vs ≈100 MB debug) — a single sequential
> `cp` is far gentler on p9fs than concurrent execve mmap faults.

### Shutdown
**Always use `~/src/bsd/scripts/vshutdown`.** It issues `shutdown -p now`, waits for QEMU to exit on its own (up to 60 s), and only escalates to SIGKILL if genuinely stuck.

The dev VM root is on **ZFS** (since the 2026-05-09 rebuild — see §11 *Dev VM rebuild milestone*). ZFS imports cleanly after any unclean shutdown — no fsck, no qcow2 corruption from mid-write `kill -9`. The historical "UFS softdep flush + SIGKILL = lost cargo caches" failure mode is gone. `pkill -f qemu-system-aarch64` is now a safe-ish recovery, though `vshutdown` is still preferred for in-flight write hygiene.

> **Never run `cargo build --release` (or any --release-profile cargo) inside the VM.** It hangs the guest hard every single time — SSH dies, only `kill -9 qemu` recovers, and that corrupts the qcow2. The 12 GB VM RAM is exhausted by rustc + LTO under HVF before swap reaches steady state. Even small crates trip this. Always cross-compile from the macOS host (§4); the host config in `~/src/bsd/.cargo/config.toml` already targets `aarch64-unknown-freebsd`. C builds in the VM are fine and stay there.

---

## 3. QEMU command-line cheat-sheet

Built into `scripts/run-vm.sh`, but reference here for one-offs:

```sh
~/src/bsd/external/qemu-build/build/qemu-system-aarch64 \
  -accel hvf -cpu host -machine virt,gic-version=3 \
  -smp 4 -m 12288 \
  -drive if=pflash,format=raw,unit=0,file=~/src/bsd/vm/edk2-aarch64-code.fd,readonly=on \
  -drive if=pflash,format=raw,unit=1,file=~/src/bsd/vm/edk2-arm-vars.fd \
  -drive if=virtio,file=~/src/bsd/vm/vm.qcow2,format=qcow2,cache=writeback \
  -device virtio-net-pci,netdev=net0 \
  -netdev user,id=net0,hostfwd=tcp::2222-:22 \
  -fsdev local,id=share,path=$HOME/src/bsd,security_model=none \
  -device virtio-9p-pci,fsdev=share,mount_tag=bsd_share \
  -nographic \
  -chardev socket,path=/tmp/fresco-shmem.sock,id=ivshmem \
  -device ivshmem-doorbell,vectors=2,chardev=ivshmem
```

Key bits:
- `gic-version=3` is required — ivshmem MSI-X delivery needs GIC v3.
- `pflash unit=0 readonly=on, unit=1 rw` — split EFI code/vars; both files must be exactly 64 MiB.
- `cache=writeback` on the qcow2 — fine for a dev VM, do not use for anything you care about.
- `security_model=none` on the 9p fsdev — required to avoid uid mapping headaches; fine for dev.

---

## 4. Cross-compile setup (host → FreeBSD)

### C (libfresco etc.)
Use macOS clang with the FreeBSD triple + sysroot.
```sh
SYSROOT=~/src/bsd/sysroot
clang --target=aarch64-unknown-freebsd16.0 \
      --sysroot=$SYSROOT \
      -fuse-ld=lld \
      -isystem $SYSROOT/usr/include \
      -L$SYSROOT/usr/lib -L$SYSROOT/lib \
      ...
```
For SHA-256 use `-lmd` (libmd is in base, not OpenSSL).

### Rust (slint-fresco etc.)
```sh
rustup target add aarch64-unknown-freebsd
```
Project-level `.cargo/config.toml`:
```toml
[target.aarch64-unknown-freebsd]
linker = "clang"
rustflags = [
  "-C", "link-arg=--target=aarch64-unknown-freebsd16.0",
  "-C", "link-arg=--sysroot=/Users/girivs/src/bsd/sysroot",
  "-C", "link-arg=-fuse-ld=lld",
]
```
Build: `cargo build --target aarch64-unknown-freebsd --release`. Output goes to `target/aarch64-unknown-freebsd/release/`, which is under the 9p share — run in the VM at `/mnt/host/<crate>/target/aarch64-unknown-freebsd/release/`.

### Kernel module (`fresco.ko`)
**Must be built in-VM** — kld modules are KBI-bound to the running kernel. Source lives in `~/src/bsd/fresco-kmod/` (visible in VM as `/mnt/host/fresco-kmod/`). Build inside the VM:

```sh
# one-time: extract kernel src to VM-local UFS (NOT 9p — see quirks)
vssh "tar -xJf /mnt/host/tarballs/src.txz -C /"

# every build:
vssh "cd /mnt/host/fresco-kmod && make"
vssh "kldload /mnt/host/fresco-kmod/fresco.ko"
```

Output `fresco.ko` lands in `~/src/bsd/fresco-kmod/` on the host (visible to VM via 9p). Module Makefile uses `bsd.kmod.mk` and picks up `SYSDIR=/usr/src/sys` automatically.

### Tessera kmod (`tessera_fs.ko`) — Phase-4 file-system driver

Source: `~/src/bsd/atrium-tessera/kmod/` and the linked subset of `~/src/bsd/atrium-tessera/core/src/` (compiled with `-DTESSERA_KERNEL=1` so the `tessera_compat.h` allocator macros expand to the kernel `M_TESSERA / M_WAITOK / M_ZERO` form).

```sh
# One-time per fresh qcow2: extract kernel source + ensure 9p is mounted.
vssh "kldload p9fs 2>/dev/null; mount | grep -q /mnt/host || \
      mount -t p9fs -o trans=virtio bsd_share /mnt/host"
vssh "[ -d /usr/src/sys ] || tar -xJf /mnt/host/tarballs/src.txz -C /"

# C library (in-tree because the .a must live under the 9p share so the
# host's cross-compiled Rust crates can link it; MK_AUTO_OBJ=no in the
# Makefile keeps it out of /usr/obj).
vssh "cd /mnt/host/atrium-tessera/core && make"

# Kernel module.
vssh "cd /mnt/host/atrium-tessera/kmod && make"
vssh "kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko"
```

Userspace tools (`mkfs-tessera`, `tessera-debug`, `tessera-property-tests`) cross-compile on the host; the binaries land in `~/src/bsd/atrium-tessera/rs/target/aarch64-unknown-freebsd/release/` and run in the VM via 9p.

```sh
# Host:
cd ~/src/bsd/atrium-tessera/rs && \
    TESSERA_CORE_LIB=$HOME/src/bsd/atrium-tessera/core \
    cargo build --release --target aarch64-unknown-freebsd
```

End-to-end mount cycle:

```sh
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
vssh "$BIN/mkfs-tessera --create -s 16 /tmp/test.img"
vssh "MD=\$(mdconfig -a -t vnode -f /tmp/test.img); \
      mkdir -p /mnt/tessera; mount -t tessera /dev/\$MD /mnt/tessera"
vssh "df -k /mnt/tessera; ls -la /mnt/tessera; stat /mnt/tessera"
vssh "umount /mnt/tessera; mdconfig -d -u md0; kldunload tessera_fs"
```

Gotchas (each one cost a kernel panic + qcow2 restore — preserved here so the next round doesn't repeat them):

- **`vflush(mp, rootrefs, …)` rootrefs MUST be 0** — vfs_root allocates the root vnode lazily on every call; rootrefs=1 is for filesystems that hold one for the lifetime of the mount, and tessera_fs panics at umount with `vflush: not busy`.
- **vfs_root vnode-construction order is strict**: getnewvnode → vn_lock(LK_EXCLUSIVE) → set v_data + v_type + v_vflag → VN_LOCK_ASHARE → insmntque1 → vn_set_state(VSTATE_CONSTRUCTED). Mirror tmpfs_alloc_vp verbatim.
- **dirent d_reclen** must be 8-byte aligned to `offsetof(struct dirent, d_name) + namlen + 1` (rounded up). Using `sizeof(struct dirent)` hangs `ls`.
- **`MODULE_DEPEND(geom_vfs, …)`** is wrong — geom_vfs is built into the kernel, not a separately loadable module; it makes kldload fail with "depends on geom_vfs - not available".
- **`g_vfs_open(devvp, &cp, "tessera", wr)`** — pass `wr=1` even for a logically read-only mount, so the kmod can issue maintenance writes (e.g. dual-SB self-heal). User writes are blocked at the VFS layer via MNT_RDONLY + vop_access EROFS, not at the GEOM layer.
- **B+tree key encoding**: inode_no is 4 bytes, encoded **big-endian** so the tree's memcmp ordering matches numeric ordering. Both mkfs (`tessera_volume_format`) and the kmod (`encode_inode_key`) use the same convention.

If the kernel panics during a kmod experiment: `pkill -9 qemu-system-aarch64`, then re-extract baseline if needed (`xz -dk vm.qcow2.xz`). With ZFS root the next boot imports cleanly without fsck; the `xz -dk` step is only needed if you want to roll back to the post-first-boot baseline (e.g. after testing destructive changes inside the VM). Don't `cargo build --release` in the VM — see `~/.claude/projects/.../memory/feedback_no_vm_cargo_release.md`.

### Custom kernel (`buildkernel`/`installkernel`) — the `/boot/laminar` gotcha

For changes to code compiled **into** the kernel (e.g. `vt(4)` in `sys/dev/vt/`, scheduler, any `options`), a loadable kmod is not enough — you must rebuild + install the whole kernel and reboot into it. The VM already has a full `/usr/src` + a warm `/usr/obj` (it built the running `LAMINAR-DEV`), so incremental rebuilds are fast (~90 s) once one file changes.

```sh
# In the VM (kernel builds are make-based C — fine in-VM, unlike `cargo --release`):
cd /usr/src
make -j$(sysctl -n hw.ncpu) buildkernel KERNCONF=LAMINAR-DEV
make installkernel KERNCONF=LAMINAR-DEV INSTKERNNAME=laminar     # <-- INSTKERNNAME!
```

**THE GOTCHA (cost most of a session): the loader boots `/boot/laminar/kernel`, not `/boot/kernel/kernel`.** `/boot/loader.conf` has `kernel="laminar"`, and `sysctl -n kern.bootfile` confirms `/boot/laminar/kernel`. A plain `make installkernel` writes to the default `/boot/kernel/` — which the loader never boots — so your rebuilt kernel is **silently ignored** and the VM keeps running the old one. Symptoms: `uname -v` build number/date doesn't change after install, and `strings -a /boot/laminar/kernel | grep <your-new-symbol>` returns 0. Always pass `INSTKERNNAME=laminar` so it installs to `/boot/laminar`, and verify before rebooting:

```sh
strings -a /boot/laminar/kernel | grep -c <a string from your change>   # expect > 0
stat -f "%Sm %N" /boot/laminar/kernel                                   # expect today
```

Other notes:
- **`buildkernel` aborts on the first `-Werror`** (e.g. a `printf("%d", x)` where `x` is `long` — `KERNEL_PANICKED()` returns `long`, cast to `(int)`). If `buildkernel` "finishes" suspiciously fast (~25 s, ~1 s CPU) it failed early; a real incremental build is ~90 s with tens of seconds of CPU. Guard the build: `make buildkernel … && make installkernel …` (don't let a failed build's stale obj get installed). Find the error with `grep -iE error /tmp/bk.log`.
- **Editing the VM's `/usr/src` vs the host `freebsd-src` tree:** they have **diverged** — the VM's `/usr/src` is newer (e.g. it has a `KERNEL_PANICKED()` guard in `vtterm_splash` the host tree lacks). The VM `/usr/src` is the real build tree. To patch safely, copy the VM's pristine file to the host (`cp <file> /mnt/host/scratch/`), edit it against *its* exact content, copy it back — don't blindly overwrite the VM file with the host copy (regresses newer changes).
- **Reboot reliability:** a guest `reboot` can **hang on shutdown** when the jailed desktop services (jaild/frescod/ostiarius hold procdescs + live jails) don't tear down cleanly, freezing the console and degrading sshd. If that happens, do a clean QEMU restart instead of `kill -9`: `echo quit | nc -U /tmp/qmp.sock` then relaunch `scripts/run-vm.sh …`. (`system_reset` via the monitor also resets the guest but may not re-init the ramfb GOP → black screen; prefer quit+relaunch for a clean firmware boot.) 9p does **not** auto-mount after a fresh boot: `kldload p9fs; mount -t p9fs -o trans=virtio bsd_share /mnt/host`.

### Boot splash (vt(4) `DEV_SPLASH`) — kernel-side, loader-fed

The Atrium boot splash is a **kernel** feature, not userland: a userland splash runs far too late (init/rc is hundreds of console lines in) and `vt(4)` owns the framebuffer + keyboard, so it would overdraw anything userland paints — and even after the GPU driver attaches, `vt` just moves its text onto the new fb. The one component that sees every console write and owns every fb backend is `vt(4)` itself, which already has the `DEV_SPLASH` framework:

- **Kernel:** `options DEV_SPLASH` in `arm64/conf/{LAMINAR-DEV,ATRIUM}`. `vtterm_splash()` draws a loader-preloaded image (or the built-in logo) and sets `VDF_SPLASH`, which makes `vt_flush()` skip all text drawing. Patched (`sys/dev/vt/vt_core.c`) so a **preloaded image enables it without `RB_MUTE`** — hiding only the *video* console (via `VDF_SPLASH`) while the serial console stays verbose for debugging.
- **Loader:** the EFI loader (`stand/common/gfx_fb.c:build_splash_module`) decodes a **32-bit RGBA PNG** named by `splash="..."` in `loader.conf` and preloads it as `MODINFOMD_SPLASH`. Serial shows `Loading splash ok` when it succeeds. (`png_get_bpp` returns 4 for 8-bit RGBA → passes vt's `si_depth != 4` guard.)
- **Asset:** `atrium-splash --gen-png /boot/atrium-splash.png 800x600` (the `atrium-splash` crate, run on host or in-VM) renders the wordmark to the PNG the loader wants.
- **Persistence + dismissal:** stock vt(4) dismisses the splash when any vt terminal is opened (`vtterm_opened`) — and `init` opens `/dev/console` early, so the stock splash would vanish before the boot is even hidden. The Atrium `vt_core.c` patch makes `vtterm_opened` NOT clear `VDF_SPLASH` (persistent splash) and adds a dismiss in `vt_processkey` (interactive keys flow there, not the polled `vtterm_cngetc`). Also set **all `ttyv*` to `off` in `/etc/ttys`** (a getty opening any vt would otherwise call `vtterm_opened`; video = GUI, CLI fallback stays on serial `ttyu0` + ssh). Net: splash holds through the whole boot; **any keypress reveals the console**; it's also dropped when a real fb/GUI driver takes over.
- **Animation:** `vt_splash_animate()` sweeps a rect indicator below the wordmark, driven by the vt flush timer; position keyed off `ticks/hz` (constant px/sec — a per-`vt_flush`-call counter races during the verbose early boot then slows). `kern.vt.splash_anim=0` disables.
- **Minimizing the pre-splash flash:** the splash appears once the *kernel* starts, so before it you briefly see (a) the UEFI/TianoCore firmware logo — pre-loader, not removable without reconfiguring EDK2; and (b) the loader's beastie menu — suppress with `beastie_disable="YES"` in `/boot/loader.conf` (the splash preload is independent of the menu). **Keep `autoboot_delay="1"`, not `"0"`:** the delay is the window to press a key and drop to the loader `OK` prompt (on video *and* serial) for recovery — booting an alternate kernel, setting tunables, single-user. `"0"` shaves ~1 s off the pre-splash flash but removes that escape hatch (anti-lockout cost); 1 s is worth it. A couple of console lines still flash before the splash; harmless.
- **Verify the patched kernel is actually running** before concluding the splash "doesn't work" — see the `/boot/laminar` gotcha above; the entire splash bring-up was chased on the wrong (unpatched) kernel for a while because `installkernel` had gone to `/boot/kernel`.

#### ddb over QEMU TCP serial (added 2026-05-01)

The historical claim "kdb isn't reachable" no longer holds. `scripts/run-vm.sh` now exposes the serial console on `tcp:127.0.0.1:4444` and the QEMU monitor on `unix:/tmp/qmp.sock`. The stock GENERIC kernel running in the VM has `KDB`, `DDB`, `INVARIANTS`, `WITNESS`, `GDB`, and `KDB_TRACE` compiled in — no rebuild needed.

```sh
# enable break-into-debugger via serial (also persists if set in /etc/sysctl.conf)
vssh "sysctl debug.kdb.break_to_debugger=1"
```

Two ways to enter ddb:

1. **From the running guest** (good for "force a known-state probe"): `vssh "sysctl debug.kdb.enter=1"`. The sysctl drops the kernel into ddb immediately; the SSH session that issued it stays blocked until you `c` out.

2. **When the VM is hung** (the actual reason this exists): `python3 scripts/ddb_session.py break`. Sends `\r~\x02` (the alt-break-to-debugger sequence) over the TCP serial. Requires `debug.kdb.alt_break_to_debugger=1` (the default).

Once in ddb, drive the debugger over the same TCP serial:

```sh
# read backtrace + lock state of the offending thread
python3 scripts/ddb_session.py "tr; show alllocks; show lockedvnods"

# pick a process and trace its kernel stack
python3 scripts/ddb_session.py "ps; tr 12345"

# resume execution
python3 scripts/ddb_session.py continue
```

`ddb_session.py` auto-responds to ddb's `--More--` paginator by sending space, so multi-page output (like `ps`) doesn't block. Pass a per-command timeout as the second argument if a command takes long (default 3 seconds).

Useful ddb commands for kmod hangs:

- `tr` — backtrace of the current thread.
- `tr <pid>` — backtrace of a specific process's kernel stack.
- `ps` — process table with wmesg (sleep channel) for each blocked thread.
- `show alllocks` — all currently-held locks across all threads. The thread spinning at 100% will be the one with no waiters but holding things others wait on.
- `show lockedvnods` — vnodes with someone holding their lock.
- `show pcpu` — per-CPU state, useful when several CPUs are pinned.
- `bt` — alternate spelling of trace.
- `c` — continue execution.
- `panic` — force a panic with stack trace; clean exit when you're done diagnosing.

Read the FreeBSD `ddb(4)` man page for the full vocabulary; the above covers ~95% of kmod-hang triage.

#### Debugging tessera_fs hangs (lessons from slice 4)

This section captures patterns that are still useful even now that ddb is reachable — proactive instrumentation lets you read state without first having to enter ddb.

**Pattern: trace ring + sysctl dump.** For the slice-4 work, two prior attempts hung the VM with no usable post-mortem. The third attempt added a 1024-entry static trace ring as the FIRST commit, before any of the actual fix code. Every meta-reserve event (`alloc-bump`, `alloc-reuse`, `free-push`, `drain-{begin,keep,release,end}`, `bitmap-built`) gets one entry with `(op, sector, gen, count)`. Static = kmod-lifetime, survives unmount. Reads via sysctl (no syscall plumbing into the hung VM).

Recipe:

```c
// Toggle (off by default in production builds).
SYSCTL_INT(_kern_tessera, OID_AUTO, metatrace_enabled, ...);

// Atomic-bump write index (only allocates a slot, no lock).
unsigned long i = atomic_fetchadd_long(&widx, 1);
ring[i % LEN] = (struct entry){ .seq = i + 1, .op = ..., ... };

// Dump-on-read sysctl. Reading the value triggers a printf
// of the ring's contents to dmesg. Awkward but minimal — no
// variable-length sysctl plumbing needed.
SYSCTL_PROC(_kern_tessera, OID_AUTO, metatrace_dump,
    CTLTYPE_ULONG | CTLFLAG_RD | CTLFLAG_MPSAFE,
    NULL, 0, sysctl_handler_that_printfs, "LU", "...");
```

Use:

```sh
vssh "sysctl kern.tessera.metatrace_enabled=1"
# ... reproduce the bug ...
vssh "sysctl kern.tessera.metatrace_dump"  # triggers the dmesg dump
vssh "dmesg | tail -200"                    # read the trace
```

If the VM hung BEFORE you could run `sysctl ... metatrace_dump`: the trace is still in memory, but you can't extract it without a working VM. Lesson: **dump the trace at known checkpoints during the test**, not just at the end. Pattern:

```sh
# Repro script that uses timeouts to bound any single operation,
# so a hang in step N can't take down the test for step N-1.
vssh "...preamble..."
vssh "sysctl kern.tessera.metatrace_dump"   # dump after preamble
vssh "...next operation..."
vssh "sysctl kern.tessera.metatrace_dump"   # dump again
```

**Pattern: bounded testing of forensic mounts.** Forensic-mount paths (`mount -o tessera.gen=N`) had pre-existing hangs from a double-free corrupting the malloc heap. Wrapping the user-space `mount` in `timeout 10 mount -t tessera ...` doesn't actually save you from a kernel-side hang — `timeout` only kills the user process; the kernel work continues. To distinguish "kmod hung" from "test misbehaved":

```sh
# Watch qemu CPU. If pinned at ~400% (== smp_cpus * 100), kernel
# is spinning in some loop that isn't yielding. Almost always the
# malloc allocator after heap corruption (double-free of a
# sector-sized block).
ps aux | grep qemu-system | awk '{print $3 "%"}'
```

If you see this pattern, the candidate causes are:
- Double-free or use-after-free of a heap-allocated buffer (most common).
- Infinite loop in a btree/manifest parser caused by corrupt on-disk data.
- Spinlock that never yields (rare in this codebase — we use mutexes).

For double-free specifically: search for any `free(p, M_TESSERA)` followed by a later `if (p) free(p, M_TESSERA)` where `p` isn't NULLed in between. The `if (p)` test is non-NULL because the local pointer wasn't cleared, even though the pointed-to memory was already returned to malloc. **Always NULL pointers after free if there are any error paths that might re-free them.**

**Pattern: diff the bug against baseline.** Before assuming a bug is caused by your in-progress changes, stash them and reproduce:

```sh
git stash
# rebuild kmod, re-run the failing test
git stash pop  # if your changes weren't to blame
```

For slice 4, this isolated the gen=999 hang as pre-existing (the sb_a/sb_b double-free) — independent of the slice-4 work itself. Without this check I would've spent hours hunting in the wrong place.

**Pattern: VM reboot recipe.** When the VM hangs:

```sh
pkill -f qemu-system 2>/dev/null
pkill -f "ssh.*2222" 2>/dev/null   # also kill stuck ssh sessions
sleep 3
~/src/bsd/scripts/run-vm.sh > /tmp/vm.log 2>&1 &

# Wait for boot:
until vssh "uname -r" 2>/dev/null | grep -qE "16\.0"; do sleep 5; done

# 9p mount doesn't auto-mount on every boot; check + reattach:
vssh "kldload virtio_p9fs 2>/dev/null; \
      mount | grep -q /mnt/host || \
      mount -t p9fs -o trans=virtio bsd_share /mnt/host"
```

The qcow2 is durable across hangs (qemu uses `cache=writeback` but the qcow2 metadata is robust to mid-write SIGKILL). No need to restore from `vm.qcow2.xz` unless you panicked AND the on-disk fs in the qcow2 is corrupted (rare; would manifest as boot failure).

### Atrium virtio-gpu kmod (`atrium_virtio_gpu.ko`) — D0 bring-up

Native FreeBSD driver for virtio-gpu, exposing the Atrium GPU ABI cdevs (`/dev/atrium-gpu0`, `/dev/atrium-display0`). Source: `~/src/bsd/atrium-kmod/`. Built in-VM, same as `fresco.ko`. Attaches to `virtio_pci`, not raw PCI — `VIRTIO_DRIVER_MODULE` + `virtio_get_device_type() == VIRTIO_ID_GPU(16)`.

**One-time setup** — disable the in-tree `vtgpu` driver so atrium can claim the device. Detaching `vtgpu0` at runtime hangs the VM (it holds the kernel framebuffer); a boot-time hint is the only safe path:

```sh
vssh 'echo "hint.vtgpu.0.disabled=\"1\"" >> /boot/loader.conf'
vssh reboot
```

After reboot, `devinfo -v` shows the GPU child as `vtgpu0 (disabled)`; the device unit is reserved but no driver is attached.

**Boot the VM with virtio-gpu attached:**
```sh
~/src/bsd/scripts/run-vm.sh --virtio-gpu
```

**Build + load + bind + smoke-test:**
```sh
vssh "cd /mnt/host/atrium-kmod && make"
vssh "kldload /mnt/host/atrium-kmod/atrium_virtio_gpu.ko"

# Reassign the disabled child from vtgpu to atrium_virtio_gpu, then
# enable it. The "set driver" command prints a misleading ENOENT but
# does the rename; `enable` is what actually triggers probe/attach.
vssh "devctl set driver -f vtgpu0 atrium_virtio_gpu" || true
vssh "devctl enable atrium_virtio_gpu0"

vssh "ls -l /dev/atrium-gpu0 /dev/atrium-display0"
vssh "cd /mnt/host/atrium-kmod && cc -O -Wall -o /tmp/test_caps test_caps.c && /tmp/test_caps"
```

Expected (verified 2026-04-28):
```
atrium_virtio_gpu0: <Atrium virtio-gpu (native, no linuxkpi)> on virtio_pci2
atrium_virtio_gpu0: attached: /dev/atrium-gpu0 /dev/atrium-display0 (ABI 0.1, virtio-gpu)
Atrium GPU ABI 0.1
  vendor=0x1af4 device=0x1050 family=virtio-gpu
  engine_mask=0x1 feature_flags=0x0
  IOC_ALLOC stub returns EOPNOTSUPP — ok
/dev/atrium-display0 opens — ok
```

**D0 step 2a (virtio handshake + controlq + GET_DISPLAY_INFO)** — DONE (2026-04-28). Driver negotiates features, allocates the controlq, reads device config (num_scanouts, num_capsets), and round-trips `VIRTIO_GPU_CMD_GET_DISPLAY_INFO`. Helper `atrium_vgpu_req_resp()` uses sglist + `virtqueue_enqueue` + `virtqueue_poll`; serialised by `ctrl_lock`.

**D0 step 2b (BO allocator + IOC_ALLOC/IOC_FREE/mmap)** — DONE (2026-04-28). Per-softc `TAILQ` of BOs; `contigmalloc`-backed; handle = monotonic u32; `mmap_offset = handle × 1 GiB`; `d_mmap` decodes offset → BO → physical address. Per-fd ownership via `bo->owner = atrium_gpu_file *`; close-time dtor walks the list and frees orphans. `IOC_SYNC` is a no-op (v0.1 = coherent only).

**D0 step 2c (IOC_SUBMIT + fences)** — DONE (2026-04-28). Userspace fills a command BO with virtio-gpu protocol bytes (e.g. `RESOURCE_CREATE_2D`); IOC_SUBMIT pushes them onto the controlq with a 2-segment sglist (req + resp), polls synchronously, checks `RESP_OK_NODATA`, returns a monotonic fence id. `IOC_FENCE_WAIT` is a no-op (synchronous v0.1); `IOC_FENCE_QUERY` reports the latest retired fence. v0.1 limitations are documented in the source: one command per submit, no wait fences, no response data surfaced, concurrent IOC_FREE on `cmd_handle` is UB. Async retirement (controlq interrupt → kqueue) is step 2d.

**D0 step 3 (modesetting + scanout)** — DONE (2026-04-28). `IOC_BIND_GPU` validates the cross-cdev binding. `IOC_ENUM_CONNECTORS` synthesises one virtual connector per enabled scanout. `IOC_MODES` returns one preferred mode per connector from cached `display_info`. `IOC_SET_MODE` lazily promotes a BO to a virtio-gpu resource (CREATE_2D + ATTACH_BACKING with the contig-malloc'd single physical segment) then SET_SCANOUT. `IOC_PAGE_FLIP` does TRANSFER_TO_HOST_2D + RESOURCE_FLUSH. **Visually verified**: `test_scanout.c` painted a 1280×800 BGRA gradient; QEMU Cocoa window confirmed pixels via the native ABI. v0.1 limitations: cursor (cursorq) deferred; FLIP_COMPLETE / vblank kqueue events deferred to step 3.5.

To see scanout output: boot with `--virtio-gpu --display` (Cocoa window appears with -nographic dropped to mon:stdio for serial).

D0 step 2d (async fence retirement) and step 3.5 (vblank events, hardware cursor) remain — neither blocks D1.

#### Booting with the venus paravirt Vulkan stack (D1.x V5+) — ⚠ SUPERSEDED

> **venus is no longer the GPU path.** It was the early paravirt Vulkan transport
> (guest Vulkan → virtio-gpu venus capset → host MoltenVK). It is replaced by
> **Carillon** (`run-vm.sh --carillon`, `docs/spec/carillon.md`): the ivshmem-doorbell
> paravirt path to `aqueduct-gpu-host` → MoltenVK → Metal. For pure in-VM CPU
> rendering use the **Tier-2 software Vulkan ICD** (`atrium-vk-icd`). The section
> below is kept for historical context (the V5/V6/V7 milestones, the MoltenVK fix,
> the perf characterization) — not as current operating instructions.

```sh
~/src/bsd/scripts/run-vm.sh --venus       # headless, serial on TCP:4444, monitor on /tmp/qmp.sock
~/src/bsd/scripts/run-vm.sh --venus --display    # add Cocoa window
```

`--venus` sets up `bochs-display` (boot splash) + `virtio-gpu-gl-pci,venus=on,blob=on,hostmem=512M` (the venus path), exports `VK_ICD_FILENAMES` to MoltenVK, captures `virgl_render_server` log to `/tmp/virgl-<pid>.log`, and selects the Atrium-patched QEMU under `~/src/bsd/external/qemu-build/build/qemu-system-aarch64`. Requires:

- Atrium-patched QEMU (`qemu-build/`) — venus QEMU integration + macOS host shims for the missing EGL/GBM (`virtio-gpu-virgl.c` short-circuits vrend init when `qemu_egl_display` is NULL and adds `VIRGL_RENDERER_NO_VIRGL`).
- Atrium-patched EDK2 (in the same `qemu-build/roms/edk2/`) — `VirtioGpuDxe` disabled in `ArmVirtPkg/ArmVirtQemu.dsc` and the FDF; otherwise EDK2 probes the GL/venus device at boot and hangs.
- Atrium-patched `virglrenderer` from `github.com/atrium-os/virglrenderer atrium/main` at `~/src/bsd/external/virglrenderer/build/server/virgl_render_server`, installed to `~/.local/libexec/virgl_render_server` and `~/.local/opt/homebrew/libexec/virgl_render_server` (the loader checks both). Four small macOS-host-venus patches: pthread shim for `<threads.h>` absence; render_log mirror to stderr; `__APPLE__` EOPNOTSUPP fallback in `virgl_renderer_resource_map_fixed` for HVF stage-2; comment on the `mtl_shm` `newBufferWithBytesNoCopy` choice. Source remote: `https://gitlab.freedesktop.org/virgl/virglrenderer.git` as `upstream`.
- MoltenVK ICD at `$(brew --prefix)/etc/vulkan/icd.d/MoltenVK_icd.json` (brew installs it; run-vm.sh points `VK_ICD_FILENAMES` there directly — *don't* use `/tmp/MoltenVK_atrium.json`, that path doesn't survive macOS reboot and is a known V5h regression).

Inside the VM, the V5 atrium-mesa fork lives at `/root/mesa` (cloned from `github.com/atrium-os/mesa atrium/main`). Build the venus driver:

```sh
vssh "cd /root/mesa && meson setup build-atrium -Datrium=true -Dvulkan-drivers=virtio \
      -Dgallium-drivers= -Dgles1=disabled -Dgles2=disabled -Dopengl=false -Degl=disabled \
      -Dglx=disabled -Dplatforms= -Dllvm=disabled -Dshared-glapi=disabled -Dvideo-codecs= \
      -Dtools= -Dbuildtype=debug"
vssh "ninja -C /root/mesa/build-atrium src/virtio/vulkan/libvulkan_virtio.so"
```

Mesa build prereqs (one-time): `pkg install -y meson ninja pkgconf bison flex python3 py311-mako py311-pyyaml py311-packaging vulkan-headers vulkan-loader vulkan-tools libdrm git`.

Run vulkaninfo against our ICD:

```sh
vssh "VK_DRIVER_FILES=/tmp/atrium_icd.json VN_DEBUG=init vulkaninfo --summary"
# /tmp/atrium_icd.json points library_path at /root/mesa/build-atrium/src/virtio/vulkan/libvulkan_virtio.so
```

Diagnostics:
- Guest kernel printfs (including the V5h `req_resp: enqueue type=…` traces) go to TCP serial: `nc 127.0.0.1 4444 > /tmp/atrium-console.log &` before running the test, then `tail` the file. Survives an SSH freeze, which is the usual failure mode.
- Host venus log: `tail /tmp/virgl-<qemu-pid>.log` shows the proxy/render-server side. The pattern `proxy: exported res N to unexpected fd_type -1` followed by `Broken pipe` is the V5g-era fd-export failure (use HOST3D blobs — see `feedback_venus_shmem_must_be_host3d` memory note).
- QEMU monitor: `nc -U /tmp/qmp.sock` (JSON QMP). Useful for `system_reset`, `query-status`. If QMP itself is unresponsive, HVF is wedged — only `kill -9` will recover, and the qcow2 will need fsck on next boot.

**V5h status (DONE 2026-05-07):** vulkaninfo enumerates the M4 Max as a Vulkan device through the full stack. Four bring-up fixes landed:

  1. **Apple Silicon 16 KiB host-page alignment** in atrium-kmod's BAR-window allocator. macOS mmap MAP_FIXED into a partial range of an existing anon mapping requires *host-page* alignment, not just guest-page. Guest is 4 KiB pages; host is 16 KiB. `ATRIUM_HOST_PAGE_SIZE` macro in `atrium_virtio_gpu.c` used uniformly by allocator, IOC_HOST_BLOB size rounding, and bitmap math.

  2. **Force EOPNOTSUPP fallback in virglrenderer's map_fixed on `__APPLE__`**. macOS HVF pins host pages at `hv_vm_map` time; subsequent userspace `MAP_FIXED|MAP_SHARED` doesn't refresh stage-2. Returning EOPNOTSUPP forces QEMU's `memory_region_add_subregion_overlap` fallback which goes through MemoryListener → triggers `hv_vm_unmap+hv_vm_map` → stage-2 picks up the SHM-backed pages. Patch in `virglrenderer.c:virgl_renderer_resource_map_fixed`.

  3. **MoltenVK ICD JSON path**: point `VK_ICD_FILENAMES` at brew's persistent install path (`$(brew --prefix)/etc/vulkan/icd.d/MoltenVK_icd.json`), not at a `/tmp/` copy. brew updates it on every `brew upgrade molten-vk` and it survives reboot.

  4. **`max_timeline_count = 64`** (was 1) in `atrium_init_renderer_info`. venus reserves ring 0 for the CPU timeline and allocates one ring per VkQueue; with `max=1` the first VkQueue bind fails with `VK_ERROR_INITIALIZATION_FAILED`.

End-to-end via vulkaninfo: `Virtio-GPU Venus (Apple M4 Max)`, Vulkan 1.2, 22 instance extensions, modern feature set.

**V6 status (DONE 2026-05-07):** GPU compute works end-to-end. `atrium-kmod/test_vk_compute.c` (squares 1024 floats via a compute shader) reports `PASS: 1024 elements squared correctly on Virtio-GPU Venus (Apple M4 Max)` through the full venus stack on patched MoltenVK.

The fix turned out to be a one-line bug in MoltenVK: `MVKDeviceMemory::initExternalMemory`'s import path (`VkImportMemoryMetalHandleInfoEXT` + `MTLBUFFER_BIT_EXT` handle type) was missing the `_device->makeResident(_mtlBuffer)` call that the non-import path in `ensureMTLBuffer()` does immediately after creating the MTLBuffer. On Metal 3 / Xcode 16+, `makeResident` registers the MTLBuffer with the device's `MTLResidencySet`; without it, the GPU IOMMU doesn't map the buffer and GPU writes silently go to a non-resident region the CPU never sees. No errors raised — Metal accepts encoder commands and the queue completes normally.

Fix lives at `atrium-os/MoltenVK main` (`MoltenVK/MoltenVK/GPUObjects/MVKDeviceMemory.mm`, +1 line). Standalone reproducer at `atrium-os/MoltenVK test_mvk_import.m` (no venus / virglrenderer / QEMU dependencies; ~175 lines). Pre-fix: FAIL 0/9 of slot writes visible. Post-fix: PASS 9/9.

**V7 status (DONE 2026-05-07):** real Fresco frame end-to-end through venus. `frescod-vulkan-smoke` (in the VM) accepts an `aqueduct` connection from `atrium-test-client`, runs the atrium-core compute + indirect-draw bundle on Apple M4 Max via venus, and dumps the readback as PNG. Visual confirmation: `vm/frescod-smoke-frame-0000.png` shows the magenta rect + yellow rotated path on teal background that the test client describes.

Reproduce:
```sh
# host: cross-compile both ends (cached; ~1s if no source changes)
cd ~/src/bsd/frescod && cargo build --release --target aarch64-unknown-freebsd --bin frescod-vulkan-smoke
cd ~/src/bsd/atrium-test-client && cargo build --release --target aarch64-unknown-freebsd --bin atrium-test-client

# VM: start the smoke server (FRESCOD_BUNDLES_ROOT needed because
# CARGO_MANIFEST_DIR baked into the binary points at the host path
# /Users/girivs/... which doesn't exist in-VM).
vssh "rm -f /tmp/frescod-smoke.sock /tmp/frescod-smoke-frame-*.png && \
      FRESCOD_BUNDLES_ROOT=/mnt/host/bundles \
      nohup /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod-vulkan-smoke \
        > /tmp/smoke.log 2>&1 &"

# VM: drive one frame
vssh "/mnt/host/atrium-test-client/target/aarch64-unknown-freebsd/release/atrium-test-client /tmp/frescod-smoke.sock"
# (test client holds the socket open with ^C; smoke renders + dumps on SCENE_FRAME_END before that)

# host: pull the PNG back
vssh "cp /tmp/frescod-smoke-frame-0000.png /mnt/host/vm/frescod-smoke-frame-0000.png"
open ~/src/bsd/vm/frescod-smoke-frame-0000.png
```

#### UI render pipe-clean — Pergola app → frescod → PNG (lavapipe SW path)

Proves the full UI render path with real pixels, no GPU/venus: a Pergola app draws
a scene graph → frescod renders it via the **lavapipe** software Vulkan ICD (already
installed at `/usr/local/share/vulkan/icd.d/lvp_icd.aarch64.json`) → PNG readback.
(lavapipe = Mesa reference SW renderer, kept for comparison; the bespoke Tier-2
`atrium-vk-icd` is the eventual swap-in.)

```sh
# host: cross-build the readback renderer + a Pergola app
( cd ~/src/bsd/frescod    && cargo build --release --target aarch64-unknown-freebsd --bin frescod-vulkan-smoke )
( cd ~/src/bsd/forum-demo && cargo build --release --target aarch64-unknown-freebsd )

# VM: copy to local ZFS (never execve off 9p), start the renderer on lavapipe
vssh 'R=/root/wmtest; mkdir -p $R
  cp /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod-vulkan-smoke $R/
  cp /mnt/host/forum-demo/target/aarch64-unknown-freebsd/release/forum-demo $R/
  LVP=/usr/local/share/vulkan/icd.d/lvp_icd.aarch64.json
  FRESCOD_SOCK=/tmp/frescod-smoke.sock VK_ICD_FILENAMES=$LVP VK_DRIVER_FILES=$LVP \
    FRESCOD_BUNDLES_ROOT=/mnt/host/bundles nohup $R/frescod-vulkan-smoke >/tmp/smoke.log 2>&1 & sleep 4
  FRESCO_SOCKET=/tmp/frescod-smoke.sock nohup $R/forum-demo >/tmp/demo.log 2>&1 & sleep 5
  cp /tmp/frescod-smoke-frame-0000.png /mnt/host/vm/forum-demo-frame.png'
open ~/src/bsd/vm/forum-demo-frame.png   # host: view the rendered frame
# → dark window + teal title strip + 3 colored squares + accent strip.
# Notes: FrescoSurface drops corner radius; Pergola text/glyph-run wire path is TODO (use rects).
```

#### Forum WM F0 in-VM (cross-app window management over a real socket)

Proves the F0 window-management loop end-to-end: the headless `frescod-wm-harness`
(real socket_server + EnvelopeFrontend, no GPU — runs in a gpusim-only VM) + the
`forum-wm` shell client + `wm-app-stub` app stand-ins. The harness grants the
`window-management` cap iff the connecting peer's `LOCAL_PEERCRED` uid == `FORUM_WM_UID`.

```sh
# host: release cross-builds (small → safe to cp over 9p)
( cd ~/src/bsd/frescod  && cargo build --release --target aarch64-unknown-freebsd --bin frescod-wm-harness )
( cd ~/src/bsd/forum-wm && cargo build --release --target aarch64-unknown-freebsd --bins )

# VM: copy to local ZFS (NEVER execve off 9p — panics p9fs, see §2)
vssh 'R=/root/wmtest; mkdir -p $R
  cp /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod-wm-harness $R/
  cp /mnt/host/forum-wm/target/aarch64-unknown-freebsd/release/{forum-wm,wm-app-stub} $R/'

# VM: harness (FORUM_WM_UID=0 grants root) + two apps + the shell
vssh 'R=/root/wmtest; rm -f /tmp/frescod.sock
  FRESCOD_SOCK=/tmp/frescod.sock FORUM_WM_UID=0 RUST_LOG=info nohup $R/frescod-wm-harness >/tmp/h.log 2>&1 & sleep 2
  FRESCO_SOCKET=/tmp/frescod.sock APP_TITLE=editor   nohup $R/wm-app-stub >/dev/null 2>&1 &
  FRESCO_SOCKET=/tmp/frescod.sock APP_TITLE=terminal nohup $R/wm-app-stub >/dev/null 2>&1 & sleep 2
  FRESCO_SOCKET=/tmp/frescod.sock $R/forum-wm; cat /tmp/h.log'
# → "forum-wm: declared layout — 2 surface(s), focus=1"; harness logs "granted window-management ... (uid 0)"
# Negative (deny): restart harness with FORUM_WM_UID=9999 → forum-wm fails fast (the
#   server now sends an IS_ERROR reply: "WM_ENUMERATE refused: Forbidden (code 1)";
#   op 0x520 = OP_WM_ENUMERATE).

# Production admission path (no FORUM_WM_UID) — getpeereid → app-registry → the
# owning user's policy grant for window-management (Choragus/audio_monitor pattern):
vssh 'mkdir -p /var/run/atrium /var/db/atrium/root
  printf "0 root org.atrium.forum\n" > /var/run/atrium/app-registry
  { printf "[grants.\"org.atrium.forum\"]\n";
    printf "manifest_hash = \"sha256:dev\"\n"; printf "granted_at = \"2026-06-15T00:00:00Z\"\n";
    printf "[grants.\"org.atrium.forum\".capabilities]\n"; printf "window-management = true\n";
  } > /var/db/atrium/root/policy.toml'
# Then run the harness WITHOUT FORUM_WM_UID → grant log shows
# "granted ... (app org.atrium.forum (registry+policy))". Flip the policy to
# window-management = false → denied (proves the grant is consulted, not app-id presence).
```

Other test clients exercise the bundle's other ops through the same smoke harness:

| Client                      | Op exercised | Result                                    |
|-----------------------------|--------------|-------------------------------------------|
| `atrium-test-client`        | rect + path  | `vm/frescod-smoke-frame-0000.png` ✅      |
| `atrium-textured`           | texture      | `vm/v7-textured.png` ✅                   |
| `atrium-text-demo`          | glyph_run    | `vm/v7-text.png` ⚠ outline-only artifact (reproduces on lavapipe too — pre-existing glyph shader bug, **not** a venus issue) |
| `atrium-rect-bouncer` (3 s) | rect (×91)   | `vm/v7-bounce-{00,30,62}.png` ✅, sustained ~17 fps round-trip-bound (see perf note below), zero VK_ERROR / stall / fence-timeout in `/tmp/smoke.log` |

##### V7 perf characterization (2026-05-07)

`frescod-vulkan-smoke` runs strict-serial: every frame ends with `vkQueueSubmit` → `vkWaitForFences(u64::MAX)`. That makes the per-frame wall time = critical-path round-trip. Per-phase timing (env vars `FRESCOD_SMOKE_NO_PNG=1` to skip readback+encode, `FRESCOD_SMOKE_NO_ENCODE=1` to skip just the PNG):

| ICD                                           | render_to_buffer | encode + save | fps cap |
|-----------------------------------------------|------------------|---------------|---------|
| atrium-mesa-venus → MoltenVK → Metal (M4 Max) | 60–63 ms         | ~2 ms         | ~16     |
| lavapipe (CPU SW rasterizer, in-VM)           | 0.7–2.4 ms       | ~2 ms         | 400–1400|

Lavapipe's <1 ms baseline shows the actual Vulkan-API + scene-build cost is negligible. The full 60 ms on venus is **paravirt round-trip latency** — guest `vkQueueSubmit` → venus ring → host worker (`virgl_render_server`) → MoltenVK encode → Metal command buffer → GPU exec → MTLEvent → fence write-back → virtio-gpu IRQ → guest `vkWaitForFences` returns.

Reference: native Linux + venus + Linux host typically clocks 1–5 ms on the same round-trip. The 10–60× gap on our path is platform-shape: HVF IRQ-injection latency, macOS scheduler wake-up granularity for the host worker process, and MoltenVK's per-submit Metal command-buffer encoding cost. **Not a venus stack bug — a paravirt-on-macOS-host characteristic.** Real apps with multiple frames in flight + double/triple buffering will amortize most of it; the smoke harness's strict-serial pattern is a worst case.

Tracked as a separate optimization task; not blocking any of D1/D2/D2.5.

##### V8 perf characterization (2026-05-08): 187× speedup, 16 fps → 3000+ fps

**Update**: the V7 hypothesis above was *wrong*. Cross-boundary tracing
(see `~/src/bsd/atrium-trace/`) showed the 60 ms was **not** HVF or
scheduler latency — it was a hardcoded 10 ms fence-poll timer in
QEMU's `virtio_gpu_fence_poll`. Each venus command's fence-retire was
waiting up to 10 ms for the next timer fire.

QEMU enables `VIRGL_RENDERER_ASYNC_FENCE_CB` (eventfd-driven async
fence callbacks, no polling) only when EGL is present. macOS has no
EGL, so QEMU fell back to polling. Fix in `qemu-build`: enable
async-fence-cb for venus regardless of EGL. The `virgl_write_async_context_fence`
callback works fine without a display — it just schedules a QEMU
bottom-half from the worker's fence eventfd write.

```c
/* qemu-build/hw/display/virtio-gpu-virgl.c, virtio_gpu_virgl_init */
if (!qemu_egl_display && virtio_gpu_venus_enabled(g->parent_obj.conf)) {
    virtio_gpu_3d_cbs.version = 4;
    virtio_gpu_3d_cbs.write_context_fence = virgl_write_async_context_fence;
    flags |= VIRGL_RENDERER_ASYNC_FENCE_CB;
    flags |= VIRGL_RENDERER_THREAD_SYNC;
}
```

Post-fix per-frame timings (`frescod-vulkan-smoke` log):

| Frame | Pre-fix | Post-fix |
|---|---:|---:|
| 0 (init) | 109 ms | 109 ms |
| 30 | 60 ms | 334 µs |
| 60 | 62 ms | 296 µs |
| 90 | 62 ms | 331 µs |

Trace data + writeup: `scratch/venus-perf/2026-05-08-trace*/`.

#### Headless interactive harness — real `frescod` + injected input + event-driven WM (2026-06-17)

The newest harness drives the **real `frescod`** (its `run_headless` path, not the
old `frescod-wm-harness`) on lavapipe: a continuous compositor that writes a PNG
every frame and accepts **scripted input over a UNIX socket** — so input → hit-test →
focus → WM → render can be exercised end-to-end with no `/dev/hidraw` or display
cdev. Three sockets, all opt-in via env:

- `FRESCOD_SOCK` — client socket (apps draw here; never granted window-management)
- `FRESCOD_WM_SOCK` — dedicated window-management socket (reaching it = the grant;
  this is what `forum-wm` connects to, via `FRESCO_WM_SOCKET`)
- `FRESCOD_INPUT_SOCK` — the injector. Line protocol (`frescod/src/injector_reader.rs`):
  `MOVE x y` / `BTN button 0|1` / `KEY hid_usage 0|1 [mods]` / `SCROLL dy`. Feeds the
  same `DisplayEvent` sink + cursor/hit-test/focus as the real HID readers.
- `FRESCOD_HEADLESS_PNG=<base>` → writes `<base>.png` (1280×720) every frame.
- `FRESCOD_BUNDLE=<dir>` → atrium-core bundle (compute SPVs + fonts); atrium-text is
  picked up as a sibling. Staged bundles live at `/root/wmtest/bundles/{atrium-core,atrium-text}`.

**Inject from the shell with `nc -N -U`** — the injector never closes the connection,
so plain `nc -U` HANGS. The `-N` (shutdown on EOF) is mandatory.

PNGs are written to the **9p-mounted host path** (`/mnt/host/...` == `~/src/bsd/...`,
`security_model=none` so writes land on the host fs) → then `Read`/`open` them on the
host. Data files over 9p are fine; only **execve off 9p panics p9fs** (copy binaries to
local ZFS first, see §2).

```sh
# host: cross-build (the frescod bin builds even though frescod-aqueduct-smoke is
# currently broken — build just the bin you need)
( cd ~/src/bsd/frescod  && cargo build --release --target aarch64-unknown-freebsd --bin frescod )
( cd ~/src/bsd/forum-wm && cargo build --release --target aarch64-unknown-freebsd --bins )

# VM: stage binaries to local ZFS (NEVER execve off 9p)
vssh 'cp /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod /root/frescod-z
  cp /mnt/host/forum-wm/target/aarch64-unknown-freebsd/release/forum-wm /root/forum-wm-z
  chmod +x /root/frescod-z /root/forum-wm-z'
```

**(A) z-order determinism check** — the regression guard for the
"sometimes background box present, sometimes not" bug (`render_one_frame` used to
iterate per-window node maps via `HashMap::values()`, whose order is per-process
seeded → nondeterministic painter z-order). Render the same Pergola app in N *fresh*
frescod processes; the PNGs must be **byte-identical** and show every box:

```sh
vssh 'export FRESCOD_BUNDLE=/root/wmtest/bundles/atrium-core
  for run in 1 2 3; do
    rm -f /tmp/fz$run.sock
    FRESCOD_HEADLESS_PNG=/mnt/host/scratch/ztest/z$run FRESCOD_SOCK=/tmp/fz$run.sock \
      /root/frescod-z >/tmp/fz$run.log 2>&1 & FP=$!
    sleep 3
    FRESCO_SOCK=/tmp/fz$run.sock /root/forumtest/vestibulum >/dev/null 2>&1 & VP=$!
    sleep 3; kill $VP $FP 2>/dev/null; sleep 1
  done'
md5 ~/src/bsd/scratch/ztest/z{1,2,3}.png   # host: all three hashes must match
open ~/src/bsd/scratch/ztest/z1.png        # → teal bg + both field boxes + amber "Sign in" button
# NOTE: vestibulum reads FRESCO_SOCK (not FRESCO_SOCKET); wm-app-stub reads FRESCO_SOCKET.
```

**(B) event-driven WM — create/destroy → auto-reconcile.** `frescod` broadcasts
`EV_WINDOW_CREATED`/`EV_WINDOW_DESTROYED` (0x0584/0x0585) to every connection;
`forum-wm`'s input poller reconciles when the surface set changes (and applies
focus-follows-click on a primary press over a focusable app surface):

```sh
vssh 'export FRESCOD_BUNDLE=/root/wmtest/bundles/atrium-core
  rm -f /tmp/fz.sock /tmp/fzwm.sock /tmp/fzin.sock /tmp/fzctl.sock
  FRESCOD_HEADLESS_PNG=/mnt/host/scratch/ztest/wm FRESCOD_SOCK=/tmp/fz.sock \
    FRESCOD_WM_SOCK=/tmp/fzwm.sock FRESCOD_INPUT_SOCK=/tmp/fzin.sock \
    /root/frescod-z >/tmp/fzf.log 2>&1 & sleep 3
  FRESCO_WM_SOCKET=/tmp/fzwm.sock FORUM_CTL_SOCKET=/tmp/fzctl.sock FORUM_SCREEN=1280x720 \
    /root/forum-wm-z >/tmp/fzwm.log 2>&1 & sleep 2
  FRESCO_SOCKET=/tmp/fz.sock APP_TITLE=alpha /root/wmtest/wm-app-stub >/dev/null 2>&1 & A=$!; sleep 1.5
  FRESCO_SOCKET=/tmp/fz.sock APP_TITLE=beta  /root/wmtest/wm-app-stub >/dev/null 2>&1 & B=$!; sleep 1.5
  kill $B; sleep 1.5
  cat /tmp/fzwm.log; pkill -f frescod-z; pkill -f wm-app-stub; pkill -f forum-wm-z'
# → "declared layout — 0 surface(s)" then three "reconciled on surface change":
#   1 surface (create alpha) → 2 surfaces (create beta) → 1 surface (destroy beta).
# Inject a click over a window: printf 'MOVE 640 360\nBTN 1 1\nBTN 1 0\n' | nc -N -U /tmp/fzin.sock
#   → forum-wm logs "focus-follows-click → surface N" IF the click hits a non-focused
#     focusable surface. CAVEAT: the WM_ENUMERATE stub tags every surface Document, so
#     multiple stubs share the work-area rect and fully overlap — click-to-focus between
#     them isn't geometrically distinguishable yet (needs tiling / real role declaration).
#     The focus-follows-click policy itself is unit-tested (forum-wm resolve_click_focus).

# F1 roles + render-gating + F2 switcher (stubs declare a role via APP_ROLE):
#   FRESCO_SOCKET=/tmp/fz.sock APP_TITLE=docA APP_ROLE=document /root/wm-app-stub & (etc.)
#   - a newly-created document grabs focus ("focus=N"); peers it fully covers log
#     "render-gating surface N (fully occluded)"; a panel (APP_ROLE=panel) docks beside
#     (right quarter) and is NOT gated.
#   - the switcher (Super+Tab) cycles document focus. The HID modifier byte is HEX:
#       printf 'KEY 0x2b 1 0x08\nKEY 0x2b 0 0x08\n' | nc -N -U /tmp/fzin.sock
#     → forum-wm logs "switcher → focus surface N" and render-gating follows the new
#     focus. (0x2b=Tab, 0x08=left-GUI/Super. The KEY mods field accepts decimal or 0x hex.)

# Workspaces (virtual desktops): Super+1..N switches the active workspace; a new
#   window opens on the active one; switching away render-gates a whole workspace's
#   apps (chrome + background stay global). Digit HID usages: '1'=0x1E .. so Super+2:
#       printf 'KEY 0x1f 1 0x08\nKEY 0x1f 0 0x08\n' | nc -N -U /tmp/fzin.sock
#     → "switched to <name|workspace N>"; the prior workspace's docs log "render-gating".
#   Move a window to a workspace: Super+Shift+N (Shift adds 0x02 → mods 0x0a). Super+Shift+3:
#       printf 'KEY 0x20 1 0x0a\nKEY 0x20 0 0x0a\n' | nc -N -U /tmp/fzin.sock
#     → "moved surface N to <ws>". The move PERSISTS: it resolves the app-id and writes
#     the placement to the state file (FORUM_STATE or /var/db/atrium/<user>/forum-
#     workspaces.state), loaded at startup so a relaunch/reboot lands the app there.
#   Persistent config (optional): FORUM_CONFIG=/path/forum.toml (else /var/db/atrium/
#     <user>/forum.toml). Sample at etc/forum.toml.example: workspaces=N, names=[...],
#     [assign] app-id→ws.
# Split/group (on-demand tiling): Super+S (0x16) toggles the active workspace between
#   stacked (one focused fills, rest gated) and tiled (docs side-by-side, all render):
#       printf 'KEY 0x16 1 0x08\nKEY 0x16 0 0x08\n' | nc -N -U /tmp/fzin.sock
#     → "split ON (tiled)/OFF (stacked)".
# Zoom (fullscreen): Super+F (0x09) toggles the focused surface to fill the whole
#   screen (covering chrome), everything else gated:
#       printf 'KEY 0x09 1 0x08\nKEY 0x09 0 0x08\n' | nc -N -U /tmp/fzin.sock
#     → "zoom ON (fullscreen)/OFF".
# Snap (manual tile): Super+Left/Right (0x50/0x4F) snap the focused doc to a half;
#   Super+Up (0x52) un-snaps. Snap two docs to opposite halves for side-by-side;
#   unsnapped docs gate. printf 'KEY 0x50 1 0x08\nKEY 0x50 0 0x08\n' | nc -N -U ... →
#   "snapped surface N Left".

# Login → desktop handoff (vestibulum → ostiarius → session launch). Demo without
#   the full TCB: ostiarius --serve-demo uses the LogLauncher + stub auth.
#   ost=/root/portcullis-target/.../ostiarius (workspace: portcullis/target/...)
vssh 'OSTIARIUS_SOCK=/tmp/ost.sock OSTIARIUS_OPEN=1 OSTIARIUS_SEAT=/tmp/ost-seat \
        /root/ostiarius-z --serve-demo >/tmp/ost.log 2>&1 &
  FRESCOD_BUNDLE=/root/wmtest/bundles/atrium-core FRESCOD_SOCK=/tmp/fz.sock \
    FRESCOD_INPUT_SOCK=/tmp/fzin.sock FRESCOD_HEADLESS_PNG=/mnt/host/scratch/login \
    /root/frescod-z >/tmp/fzf.log 2>&1 & sleep 3
  FRESCO_SOCK=/tmp/fz.sock OSTIARIUS_SOCK=/tmp/ost.sock /root/vestibulum-z >/tmp/vest.log 2>&1 & sleep 2
  # drive the form (username field 640,256; password 640,304; Sign in 640,368):
  printf "MOVE 640 256\nBTN 1 1\nBTN 1 0\n" | nc -N -U /tmp/fzin.sock
  printf "KEY 0x15 1\nKEY 0x15 0\n" | nc -N -U /tmp/fzin.sock   # r ... (o=0x12,o,t=0x17)
  printf "MOVE 640 368\nBTN 1 1\nBTN 1 0\n" | nc -N -U /tmp/fzin.sock; sleep 1
  cat /tmp/ost.log'  # → "launch org.atrium.forum ... choragus ... dock" = the desktop comes up
# Production: ostiarius (no --serve-demo) = JaildLauncher + PAM (build --features pam),
#   needs jaild running; vestibulum must be registered org.atrium.vestibulum (peer gate).

#### Production jailed launch (atrium-launch → jaild → real jail, VERIFIED 2026-06-18)
# A signed Forum component launched into a real jail under its per-app uid, drawing.
# Bins (portcullis workspace target): atrium-jaild, atrium-launch.
vssh '
  APP=/usr/local/share/atrium/apps/org.atrium.forum-bar; mkdir -p $APP/bin /etc/atrium/publishers
  cp /mnt/host/forum-bar/atrium.toml $APP/atrium.toml
  cp /mnt/host/forum-bar/target/aarch64-unknown-freebsd/release/forum-bar $APP/bin/forum-bar; chmod +x $APP/bin/forum-bar
  cp /mnt/host/etc/jaild.policy.toml /etc/atrium/jaild.policy.toml
  # sign the manifest (P-256, cosign-style): base64 DER → .sig; SPKI pubkey → publishers
  openssl ecparam -name prime256v1 -genkey -noout -out /root/dev-key.pem
  openssl ec -in /root/dev-key.pem -pubout -out /etc/atrium/publishers/dev.pem
  openssl dgst -sha256 -sign /root/dev-key.pem -out /root/s.der $APP/atrium.toml
  openssl base64 -A -in /root/s.der -out $APP/atrium.toml.sig
  : > /var/run/atrium/app-registry; rm -f /var/run/atrium/jaild.sock /atrium/sockets/fresco/fresco.sock
  mkdir -p /atrium/sockets/fresco
  # frescod at the CANONICAL jailed socket path (connect_default prefers it; the graphics cap mounts it)
  FRESCOD_BUNDLE=/root/wmtest/bundles/atrium-core FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock \
    FRESCOD_HEADLESS_PNG=/mnt/host/scratch/jailed /root/frescod-z >/tmp/fzf.log 2>&1 & sleep 3
  /root/jaild-z serve --policy /etc/atrium/jaild.policy.toml --socket /var/run/atrium/jaild.sock >/tmp/jaild.log 2>&1 & sleep 1
  ATRIUM_LAUNCH_SUPERVISE=1 /root/launch-z org.atrium.forum-bar alice \
    $APP/atrium.toml $APP/atrium.toml.sig / $APP/bin/forum-bar >/tmp/launch.log 2>&1 & sleep 3
  ps -axo user,uid,pid,jid,command | grep bin/forum-bar | grep -v grep   # → app-a 50000 ... jid N
  jls'                                                                    # → the live jail
# Gotchas: jaild exec_paths.allowed_prefixes must cover the bin (apps/ ✓); jaild pdfork has no PD_DAEMON
# so the launcher must HOLD the procdesc fd (ATRIUM_LAUNCH_SUPERVISE) or the kernel SIGKILLs the child;
# frescod socket is 0666 (per-app uids connect; the jail mount is the cap gate); env FRESCO_SOCKET isn't
# in jaild's env allowlist so rely on connect_default → /atrium/sockets/fresco/fresco.sock.

#### Full jailed desktop via login (ostiarius → jaild → wm+bar+dock, VERIFIED 2026-06-18)
# Install all 3 components as signed bundles (inst() helper: cp manifest+binary, openssl-sign with one
# dev-key.pem — sig is over the manifest, so swapping the binary needs no re-sign):
#   inst org.atrium.forum-wm  forum-wm/atrium.toml  <forum-wm bin>  forum-wm   (+ forum-bar, forum-dock)
# Canonical socket dirs: mkdir -p /atrium/sockets/{fresco,fresco-wm,forum-ctl,notify}
# frescod with BOTH sockets at the canonical paths:
#   FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=/atrium/sockets/fresco-wm/fresco-wm.sock
# jaild serve; then ostiarius --serve-prod (real JaildLauncher + stub auth, holds procdescs):
#   OSTIARIUS_SOCK=/tmp/ost.sock OSTIARIUS_OPEN=1 JAILD_SOCK=... ATRIUM_PUBLISHERS=/etc/atrium/publishers \
#     ostiarius --serve-prod &
#   printf '{"op":"login","user":"alice","password":"x","frontend":"gui"}\n' | nc -N -U /tmp/ost.sock
# → jls shows 3 jails (uids 50000/50001/50002); the PNG composites the desktop (top bar + wallpaper + dock).
# forum-bar declares role=Chrome, forum-dock role=Background so the WM doesn't gate them; forum-wm's forum-ctl
# socket is 0666 for the chrome's cross-uid connect (bar shows live "0 windows" = it queried forum-ctl).

#### Boot directly to vestibulum (rc services, VERIFIED 2026-06-18)
# rc.d: atrium-jaild (portcullis/jaild/etc) + atrium-frescod (frescod/etc) + atrium-ostiarius
# (portcullis/ostiarius/etc) → /usr/local/etc/rc.d/ (chmod +x). Binaries → /usr/local/bin/
# {atrium-jaild,frescod,ostiarius}. atrium-core bundle → /usr/local/share/atrium/bundles/atrium-core.
# App bundles (vestibulum + forum-*) signed under /usr/local/share/atrium/apps/ (see the recipe above).
# rc.conf:
#   sysrc atrium_jaild_enable=YES
#   sysrc atrium_frescod_enable=YES
#   sysrc atrium_frescod_headless_png=/var/run/atrium/frescod   # headless: no display device in this VM
#   sysrc atrium_ostiarius_enable=YES
# Then `reboot`: jaild → frescod (waits for its socket to bind) → ostiarius (--serve-prod boots vestibulum)
# → a jailed vestibulum (uid 50000, JID 1) renders the login (cp /var/run/atrium/frescod.png to view).
# CLI FALLBACK / anti-lockout (independent of the Atrium stack, survive any GUI failure):
#   - sshd on :2222 (vssh) — base FreeBSD, enabled.
#   - serial console getty on :4444 (ttyu0 "3wire" onifconsole) — root login out-of-band.
#   Both come up regardless of the atrium services, so enabling boot-to-vestibulum can't lock you out.
# Readiness: ostiarius connect-PROBES frescod (wait_for_frescod, not a stat — a socket file can exist while
#   stale/mid-bind) before booting vestibulum; vestibulum then connects fail-fast (3 tries) and, being a
#   supervised service, lets the supervisor recover a persistent failure.
# Gotchas: (1) `cp` over a RUNNING binary fails silently with "Text file busy" → install updated service
#   binaries with cp-to-.new + `mv -f` (rename is atomic + allowed while in use). Cost several confusing
#   reboots. (2) 9p doesn't auto-remount post-reboot: `kldload p9fs; mount -t p9fs -o trans=virtio bsd_share /mnt/host`.
# Per-app placement (app-id): frescod stamps owner_uid (getpeereid) in WM_ENUMERATE;
#   forum-wm resolves it via the launch registry /var/run/atrium/app-registry
#   ("<uid> <user> <app-id>" lines) → [assign] rule. To exercise as root (uid 0):
#       printf '0 root org.atrium.navigator\n' > /var/run/atrium/app-registry
#     + forum.toml [assign] "org.atrium.navigator" = 1 → that app opens on workspace 1.
```

#### Build cycle: rebuilding the patched MoltenVK

The atrium venus stack depends on `atrium-os/MoltenVK` (one-line fix vs upstream). After `git pull` on the fork:

```sh
cd ~/src/bsd/external/MoltenVK
rm -rf build && mkdir build && cd build
cmake -G Ninja -DCMAKE_BUILD_TYPE=Release ..
ninja MoltenVK
# brew's MoltenVK lib is read-only; chmod, replace, restore
chmod +w $(brew --prefix)/Cellar/molten-vk/*/lib/libMoltenVK.dylib
cp MoltenVK/libMoltenVK.1.4.2.dylib \
   $(brew --prefix)/Cellar/molten-vk/*/lib/libMoltenVK.dylib
chmod -w $(brew --prefix)/Cellar/molten-vk/*/lib/libMoltenVK.dylib
```

`fetchDependencies --macos` (upstream's preferred build path via xcodebuild) currently fails on Tahoe due to an unrelated Xcode plug-in DVT symbol mismatch — `cmake -G Ninja` works around it. CMake pulls SPIRV-Cross / SPIRV-Tools / Vulkan-Headers via CPM into `~/.cache/CPM/`; first build is ~3 minutes, incremental builds ~15 seconds.

Worker process picks up the patched dylib via the brew ICD JSON path (`$(brew --prefix)/etc/vulkan/icd.d/MoltenVK_icd.json`'s `library_path` is relative to the JSON's location, which is in the brew Cellar tree — so replacing the dylib in-place is sufficient).

#### Build cycle: rebuilding virglrenderer/render_server (Apple Silicon)

After editing `~/src/bsd/external/virglrenderer/`:

```sh
cd ~/src/bsd/external/virglrenderer && ninja -C build
cp build/src/libvirglrenderer.1.dylib ~/.local/lib/
cp build/server/virgl_render_server ~/.local/libexec/

# CRITICAL: re-codesign or QEMU will SIGKILL within 2 seconds of launch.
# macOS code-signing's CS_VALID page cache rejects the rebuilt dylib's
# pages because the on-disk mtime no longer matches the cached value.
# Symptom: empty /tmp/qemu-out.log, QEMU exits silently. The actual
# error is in `log show --predicate 'eventMessage CONTAINS qemu-system'`:
#   kernel: CODE SIGNING: process N: rejecting invalid page ...
#                         cs_mtime != mtime ... SIGKILL
codesign --remove-signature ~/.local/lib/libvirglrenderer.1.dylib \
                            ~/.local/libexec/virgl_render_server
codesign --force --sign - ~/.local/lib/libvirglrenderer.1.dylib
codesign --force --sign - ~/.local/libexec/virgl_render_server

# Now restart the running VM (the in-flight QEMU has the OLD dylib mmap'd):
~/src/bsd/scripts/vssh "shutdown -p now"
# wait for QEMU to exit, then:
~/src/bsd/scripts/run-vm.sh --venus </dev/null >/tmp/qemu-out.log 2>&1 &
```

Wrapper-worthy. If you find yourself doing this dance more than twice, write a `~/src/bsd/external/virglrenderer/install.sh`.

#### Build cycle: rebuilding atrium-mesa inside the VM

Iterating on a venus / atrium-renderer change you've made on the host:

```sh
# Fast path: rsync just the changed files (do this from host, not VM —
# 9p file-descriptor exhaustion makes large rsyncs over /mnt/mesa fail
# halfway with "Too many open files (24)").
rsync -az -e "ssh -p 2222" \
    ~/src/bsd/external/mesa/src/virtio/vulkan/ \
    root@127.0.0.1:/root/mesa/src/virtio/vulkan/
~/src/bsd/scripts/vssh "cd /root/mesa/build-atrium && ninja install"
```

No codesign step here — the guest is FreeBSD, not macOS. Mesa builds incremental in seconds.

**If `/root/mesa` doesn't exist** (post-baseline-restore), see "Post-restore reinstall sequence" below for the from-scratch path.

#### Build cycle: rebuilding atrium-kmod

The simple case — `/mnt/host` is mounted and `/usr/src` is present:

```sh
~/src/bsd/scripts/vssh "cd /mnt/host/atrium-kmod && make"
~/src/bsd/scripts/vssh "kldunload atrium_virtio_gpu; kldload /mnt/host/atrium-kmod/atrium_virtio_gpu.ko"
```

For changes that need to apply on first attach (e.g., a new field that gets initialized at attach time), install into `/boot/modules/` and reboot:

```sh
~/src/bsd/scripts/vssh "cp /mnt/host/atrium-kmod/atrium_virtio_gpu.ko /boot/modules/ && shutdown -r now"
```

`loader.conf` already has `atrium_virtio_gpu_load="YES"` so the new copy auto-loads on next boot.

**Gotchas when copying the kmod source into the VM:**

- `make` here means *bmake* (FreeBSD make), not GNU make. It expects `bsd.kmod.mk` from `/usr/share/mk/` and headers from `/usr/src/sys/`. If `/usr/src` is empty (post-baseline-restore), bmake stops with "Unable to locate the kernel source tree" — install `src.txz` first (see "Post-restore reinstall sequence").
- `bsd.kmod.mk` generates a `machine` symlink in the source dir pointing at the arch-specific sys headers. **`scp -r` of the atrium-kmod tree from macOS follows that symlink and dies with `local stat "machine": No such file or directory`** because the host doesn't have `/usr/src/sys/arm64/include`. Use `tar` over SSH and exclude the symlink:

  ```sh
  tar cf - --exclude='atrium-kmod/machine' --exclude='atrium-kmod/bootfb/machine' \
          atrium-kmod | \
    ssh -p 2222 root@127.0.0.1 'cd /root && rm -rf atrium-kmod && tar xf -'
  vssh 'cd /root/atrium-kmod && rm -f machine bootfb/machine && make'
  ```

- macOS `tar c` preserves extended attributes by default; the receiving FreeBSD `tar x` prints `Cannot restore extended attributes: com.apple.provenance: Unknown error: -1` for every file. **Harmless** — the file contents land correctly. Add `--no-xattrs` to silence, but the warning is just noise.

#### Deferred kmod load — so a wedged kmod doesn't brick the VM

**Problem (learned the hard way 2026-05-10/11):** if `atrium_virtio_gpu.ko` is preloaded via `/boot/loader.conf` and the kmod has any bug — panic at attach, CPU-spin on probe, deadlock in an IRQ handler — the VM boots into a state with **no diagnostic channel**: sshd never gets to run because the kmod is consuming all CPU, ddb-on-serial starves on missed interrupts, and the only recovery is `pkill -9 qemu` followed by a full xz baseline restore. That's an hour of work per failed kmod-debug iteration. The xz baseline restore loses everything not in the baseline image (atrium-mesa userspace, bundles, frescod/vestibulum binaries, /usr/src) so the next 30 minutes are spent reinstalling.

**Fix:** don't preload the kmod. Load it after sshd via `/etc/rc.d/atrium_virtio_gpu`. If the kmod is broken, sshd comes up first, you SSH in, `scp` a fixed kmod into `/boot/modules/`, and `service atrium_virtio_gpu restart`. No reboot needed in most cases; even when a reboot is needed, no xz restore.

**Setup (already in the post-2026-05-11 baseline xz):**

- `/boot/loader.conf`: **no** `atrium_virtio_gpu_load="YES"`. Just `hint.vtgpu.0.disabled="1"` so the in-tree vtgpu doesn't grab the virtio-gpu device unit at boot.
- `/etc/rc.conf`: `atrium_virtio_gpu_enable="YES"`.
- `/etc/rc.d/atrium_virtio_gpu`: kldload's the kmod, then runs `devctl set driver -f vtgpu0 atrium_virtio_gpu && devctl enable atrium_virtio_gpu0` to bind the (vtgpu-disabled) device unit to atrium. `REQUIRE: sshd` in the rcorder line so it runs *after* sshd is up.
- Source of truth: `~/src/bsd/atrium-kmod/rc.d/atrium_virtio_gpu`. `scripts/build-zfs-root.sh` installs it into new VMs.

**Iterate on a kmod change without ever burning the VM:**

```sh
# 1. Build the new kmod (host or in-VM, doesn't matter).
# 2. Copy it in:
scp -i ~/.ssh/fresco_bsd_ed25519 -P 2222 \
    ~/src/bsd/atrium-kmod/atrium_virtio_gpu.ko \
    root@127.0.0.1:/boot/modules/
# 3. Reload:
vssh "kldunload atrium_virtio_gpu 2>/dev/null; service atrium_virtio_gpu start"
# 4. If kldload panics the kernel: reboot, sshd comes back up cleanly,
#    scp a fixed kmod, repeat. No xz restore.
```

The bad-kmod recovery loop is **30 seconds** instead of 30 minutes.

#### Restoring the dev VM from `vm.qcow2.xz` baseline

**When you actually need this** (much rarer with deferred-load above): the qcow2 fs is corrupted (still rare with ZFS root); a destructive in-VM experiment broke `/lib` or `/boot` itself (see "replacing system libraries safely" below — and don't replace `libthr` again).

```sh
pkill -9 qemu-system-aarch64
xz -dkf ~/src/bsd/vm/vm.qcow2.xz   # 1.2 GB → 1.7 GB, ~30s
~/src/bsd/scripts/run-vm.sh --venus    # or whatever profile
```

ZFS imports cleanly after restore — no fsck step.

**What the baseline xz contains** (as of 2026-05-09): vanilla `build-zfs-root.sh` output + atrium-virtio-gpu kmod preloaded. **It does NOT contain:**

- atrium-mesa userspace (`/usr/local/lib/libvulkan_virtio.so`, ICD JSON)
- atrium-core / atrium-text bundles (`/usr/local/share/atrium/bundles/`)
- frescod / vestibulum binaries in `/root/`
- `/usr/src` (FreeBSD sources — not bundled to keep the xz small)
- mesa source tree at `/root/mesa/`

So after a baseline restore, a "ready to render" state requires the post-restore reinstall sequence below.

#### Post-restore reinstall sequence (atrium-mesa + bundles + kmod)

Mount host shares first:

```sh
vssh '
mkdir -p /mnt/host /mnt/mesa
mount /mnt/host                                          # bsd_share, in /etc/fstab as noauto
mount -t p9fs -o trans=virtio mesa_share /mnt/mesa       # mesa-only share, mount on demand
'
```

**Bundles** (small, fine over 9p):

```sh
vssh '
mkdir -p /usr/local/share/atrium/bundles
rsync -a /mnt/host/bundles/atrium-core /mnt/host/bundles/atrium-text \
        /usr/local/share/atrium/bundles/
'
```

**atrium-mesa source** — **DO NOT `rsync` over 9p**. The mesa tree is ~30k files and 9p exhausts its file-descriptor pool with "Too many open files (24)" mid-rsync, leaving a partial copy. Use SSH-rsync (host → guest):

```sh
rsync -avq --delete --exclude=.cache --exclude=build --exclude=.git \
  -e "ssh -i ~/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o BatchMode=yes -p 2222" \
  ~/src/bsd/external/mesa/ root@127.0.0.1:/root/mesa/
```

**atrium-mesa build** — meson + ninja, in-VM (mesa is C, builds fast):

```sh
vssh 'cd /root/mesa && meson setup build-atrium -Datrium=true -Dplatforms= -Dbuildtype=release && ninja -C build-atrium install'
```

**atrium-kmod** — the baseline xz already has the kmod preloaded, but it's the *baseline* kmod from when the xz was rolled, missing fixes you've made since. Rebuild + drop into `/boot/modules/` + reboot:

```sh
# Get the source into the VM. tar+ssh (NOT scp -r — it follows symlinks
# and the bsd.kmod.mk-generated `machine` symlink in atrium-kmod/ trips it):
cd ~/src/bsd
tar cf - --exclude='atrium-kmod/machine' --exclude='atrium-kmod/bootfb/machine' \
        atrium-kmod | \
  ssh -i ~/.ssh/fresco_bsd_ed25519 -p 2222 root@127.0.0.1 \
        'cd /root && rm -rf atrium-kmod && tar xf -'

# Build needs /usr/src for bsd.kmod.mk + sys headers. The baseline xz
# doesn't have it — install the matching FreeBSD source tarball first:
vssh 'fetch -o /tmp/src.txz https://download.freebsd.org/releases/arm64/aarch64/16.0-CURRENT/src.txz && tar -xf /tmp/src.txz -C /'

vssh 'cd /root/atrium-kmod && rm -f machine bootfb/machine && make'
vssh 'cp /root/atrium-kmod/atrium_virtio_gpu.ko /boot/modules/ && shutdown -r now'
```

`tar` over SSH from macOS prints `Cannot restore extended attributes: com.apple.provenance` warnings for every file — these are harmless macOS xattr noise. The actual files transfer fine.

**Re-snapshot the baseline** once everything is back in place and known-good (saves the next restore from doing this whole dance):

```sh
vssh 'shutdown -p now'
xz -k --extreme ~/src/bsd/vm/vm.qcow2 -o ~/src/bsd/vm/vm.qcow2.xz.new
mv ~/src/bsd/vm/vm.qcow2.xz.new ~/src/bsd/vm/vm.qcow2.xz
```

#### Replacing system libraries safely (don't burn the VM)

Lesson from 2026-05-10: replacing `/lib/libthr.so.3` with a custom build (`WITHOUT_PTHREADS_ASSERTIONS=yes`) bricked all dynamically-linked binaries, including sshd. The replacement library was 137280 bytes — exactly the size of the original — but had some undiagnosed ABI mismatch. Recovery required full xz restore + reinstall.

**Procedure when you genuinely need to replace a `/lib` or `/usr/lib` library:**

1. **Verify the replacement library loads** with a tiny test program *first*, ideally chrooted or via `LD_LIBRARY_PATH` so failure is recoverable:

   ```sh
   # In VM, before touching /lib:
   vssh 'mkdir -p /tmp/libtest && cp /tmp/libthr-build/.../libthr.so.3 /tmp/libtest/
         LD_LIBRARY_PATH=/tmp/libtest /usr/bin/cat /etc/hosts'
   ```

   If `cat` segfaults or rtld errors, the replacement is broken — **stop here**. Do not copy into `/lib`.

2. **Keep a copy of the original** before clobbering. `chflags noschg` strips the system-immutable flag, but the file is still there to restore from:

   ```sh
   vssh 'chflags noschg /lib/libthr.so.3 && cp /lib/libthr.so.3 /root/libthr.so.3.orig'
   vssh 'cp /tmp/.../libthr.so.3 /lib/libthr.so.3'
   # Test ssh from a SECOND terminal (don't close the first one!)
   ssh -i ~/.ssh/fresco_bsd_ed25519 -p 2222 root@127.0.0.1 'echo alive'
   # If that works, you're fine. If not, rollback:
   vssh 'cp /root/libthr.so.3.orig /lib/libthr.so.3'   # via the still-open shell
   ```

3. **Don't replace `libc` or `libsys`.** Those affect every binary including the rollback `cp` itself, and you can wedge yourself with no escape. Only consider this for libraries that aren't a hard dependency of `cp`/`sh`/`ssh`.

#### Capturing the Cocoa window for screenshots

Two paths, depending on whether `--display` is set:

**With `--display` (Cocoa window visible)** — QMP socket is wired:

```sh
echo "screendump /tmp/atrium.ppm" | nc -U /tmp/qmp.sock -w 2
sips -s format png /tmp/atrium.ppm --out vm/atrium.png
```

By default this dumps **console 0**. The default `--venus` profile used to attach two display devices (bochs-display + virtio-gpu-gl-pci); console 0 was bochs (showing "Guest has not initialized the display"), console 1 was the venus framebuffer. As of commit 19b75bd, the `--venus` profile drops bochs and virtio-gpu-gl-pci is the sole device on console 0 — `screendump` without args captures the venus output directly.

If you ever re-add a secondary display device, dump a specific head with: `screendump <filename> <device-id> <head>` — the device id comes from `info qom-tree | grep <device-name>` on the QMP socket.

**Without `--display`** — QMP isn't wired (HMP goes to stdio in headless mode). To screenshot, modify `run-vm.sh` to add `-monitor unix:/tmp/qmp.sock,server=on,wait=off` alongside `-display none`, or reboot with `--display`.

#### MoltenVK env knobs the venus host worker inherits

`run-vm.sh --venus` exports these for the worker process (forked from QEMU):

| Var | Purpose |
|---|---|
| `VK_ICD_FILENAMES` / `VK_DRIVER_FILES` | Brew-installed MoltenVK ICD JSON. |
| `DYLD_LIBRARY_PATH` | Brew lib dir, for bare-basename `dlopen`. |
| `MVK_CONFIG_SYNCHRONOUS_QUEUE_SUBMITS=1` | Force MoltenVK to commit + wait Metal command buffers inline inside `vkQueueSubmit`. Required for the worker to dump consistent state right after submit returns; also stabilizes GPU debugging. |
| `VIRGL_LOG_FILE` / `VIRGL_LOG_LEVEL=debug` | virglrenderer proxy log to `/tmp/virgl-<pid>.log`. |

**D1 step 1 (atrium-gpu-rs Rust binding)** — DONE (2026-04-28). Crate at `~/src/bsd/atrium-gpu-rs/` exposes `Gpu`, `Bo`, `Display`, `Connector`, `Mode`. `examples/gradient.rs` visually verified.

**D1 host cross-compile** — DONE (2026-04-28). `~/src/bsd/.cargo/config.toml` configures rust-lld + FreeBSD CRT objects (Scrt1.o, crti.o, crtbeginS.o, crtendS.o, crtn.o) from `sysroot/`; `rust-toolchain.toml` pins nightly + rust-src for `-Z build-std`. Incremental cross-builds for atrium-gpu-rs land in ~2s on the macOS host vs minutes in the VM. Build with `cargo build --target aarch64-unknown-freebsd --release --example <name>`; binary lands at `target/aarch64-unknown-freebsd/release/examples/<name>` (visible in VM via 9p). **In-VM cargo is no longer the iteration loop** — host cross is.

**D1 step 2(a) (tiny-skia + atrium-gpu-rs hello_rect)** — DONE (2026-04-28). `examples/hello_rect.rs` rasterizes a rounded rect with linear gradient fill + white stroke + yellow accent into the scanout BO's mmap'd memory using tiny-skia's `PixmapMut::from_bytes` (zero-copy), then page-flips. **Visually verified** via QEMU Cocoa window. R↔B swap in userspace bridges tiny-skia's RGBA to the kmod's hardcoded BGRA scanout format; will be removed when SET_MODE plumbs format.

D0 step 2d (async fence retirement) and step 3.5 (vblank events, hardware cursor) remain deferred — neither blocks D1.

**D1 step 2(b) (frescod first-light)** — DONE (2026-04-28). Standalone binary at `~/src/bsd/frescod/` owns the display + GPU cdevs, runs a 30 fps frame loop, renders an animated analog clock inside a shadowed panel via tiny-skia, page-flips. **Visually verified** — no fresco-server, no Metal, no winit, no QEMU host compositing. ~330 lines including drop shadow, tick marks, hands, frame heartbeat.

**D1 step 2(c.0) (fresco-server → dual-target lib)** — DONE (2026-04-28). `~/src/fresco-server/` is now a Cargo lib + bin. macOS-only deps (winit, metal, objc2-*, core-graphics-types, raw-window-handle) gated under `[target.'cfg(target_os = "macos")'.dependencies]`. macOS-only modules cfg-gated: `render::metal_backend`, `input::capture`, the existing winit-coupled bin (moved to `main_macos.rs`, called from a stub `main.rs`). `GpuBackend` trait decoupled from `winit::Window` — `new()` removed from the trait, each concrete backend has its own constructor. `InputEvent` lifted from `input::capture` to `input::mod` so platform-neutral modules (ivshmem, network) can import it.

Both targets build cleanly:
- `cd /Users/girivs/src/fresco-server && cargo check --release` — full macOS bin
- `cd /Users/girivs/src/bsd/frescod && cargo check --target aarch64-unknown-freebsd` — FreeBSD lib (frescod pulls fresco-server as a path dep; the cross-compile config in `bsd/.cargo/` propagates).

`frescod` smoke-tested as still running after the restructure.

**D1 step 2(c.1) (tiny_skia_backend in fresco-server)** — DONE (2026-04-28). New module `fresco_server::render::tiny_skia_backend` behind feature `tiny-skia-backend`. Implements `GpuBackend` over an internal `tiny_skia::Pixmap`:
- `resize` reallocates the pixmap; `set_scale` records DPI scaling.
- `render_frame` walks `SceneGraph::render_list()` and rasterizes each `RenderItem`: decodes mesh from CAS, builds a tiny-skia `Path` from the triangle list, decodes material (solid color or linear gradient), applies the affine 2D part of the world matrix as a `Transform`, calls `fill_path`. Textured materials, radial gradients, clip rects, and per-window FBOs are TODO for later sub-steps.
- Convenience accessors: `pixmap_mut()` for direct-draw integration, `pixels()` for raw RGBA, `copy_to_bgra(dst)` for the virtio-gpu scanout swap.

`frescod` refactored to use the backend: instead of allocating its own `PixmapMut::from_bytes(bo.as_mut_slice())`, it owns a `TinySkiaBackend` for the same dimensions, draws into `backend.pixmap_mut()`, then `backend.copy_to_bgra(bo.as_mut_slice())` before page flip. **Visually verified** — same clock as before, now running through fresco-server's lib.

**D1 step 2(c.2) (SceneGraph-driven render)** — DONE (2026-04-28). frescod now constructs a `SceneGraph` + `CasStore` programmatically each frame and renders via `backend.render_frame(scene, cas, frame, cursor)`. Helper module `src/scene_build.rs` encodes the wire-format blobs (BlobHeader + body): solid-color material (0x0200), linear-gradient material (0x0201), mesh (0x0100) with vertex_data + index_data hashes. **Visually verified**: dark gradient background, translucent center panel, five orbiting colored squares (animating per-frame at ~0.7 rad/s), pulsing yellow center dot. Every visible rect flows `RenderItem → CAS → TinySkiaBackend → BO`. The same data path a real protocol client would feed.

**D1 step 2(c.3) (clock through SceneGraph)** — DONE (2026-04-28). frescod's clock is now built entirely from `RenderItem`s: gradient bg + panel rect + 64-segment disk (face) + 80-segment ring (outer rim) + 12 oriented-rect ticks (heavier at quarters) + 3 oriented-rect hands (hour / minute / second) + small disk (hub). 19 RenderItems per frame; mesh primitives stored once at startup. New scene_build helpers: `store_disk(n_segs)`, `store_ring(n_segs, inner_ratio)`, `push_oriented_rect(cx, cy, angle, len, w)`, `push_disk`, `push_ring`. Visually verified — clock animates, every pixel except the heartbeat went through SceneGraph + tiny_skia_backend.

**D1 step 2(c.4) (Unix-socket Fresco protocol)** — DONE (2026-04-29). frescod now exposes `/tmp/frescod.sock` (override via `FRESCOD_SOCK`). On accept, a per-connection thread reads 128-byte `Command` structs and dispatches a v0.1 subset against shared `Arc<Mutex<{CasStore,SceneGraph}>>`: `CMD_UPLOAD_BEGIN`/`DATA`/`FINISH` (inline path; up to 112 bytes initial in BEGIN, 116 per DATA, FINISH commits via `CasStore::finish_upload` and writes a `COMP_UPLOAD_COMPLETE`), `CMD_SET_ROOT` (sets scene root + marks dirty). DMA staging not supported — kmod-mediated mechanism, deferred.

`atrium-test-client` (new crate `~/src/bsd/atrium-test-client/`) builds 9 wire-format blobs (vertex/index/mesh/material/transform/renderable/scene_node/node_list/scene_root), uploads each via inline framing, then SET_ROOT. Compositor's render loop sees `root_hash != NULL`, calls `SceneGraph::traverse`, walks → renders. **Visually verified**: magenta rect at (200, 200) 400×300, exactly matching the test client's spec.

Two bugs fixed during bring-up (recorded so we don't repeat):
- BEGIN must include initial blob bytes at payload offset 8 (up to 112). `cas.begin_upload` registers the staging entry; if BEGIN's data is empty/zeroed the entry seeds with zeros and `finish_upload` produces a different hash than the local one.
- `SceneNode` BlobHeader.flags bit 0x01 = VISIBLE. Without it, `traverse_node` early-returns silently and the node never renders.

**D1 step 2(c.5) (fresco-socket-rs client lib + bouncer demo)** — DONE (2026-04-29). New crate `~/src/bsd/fresco-socket-rs/` with a reusable Rust API for Fresco-over-Unix-socket: `Connection::connect(path)`, `connection.upload_blob(&[u8]) -> Hash` (handles BEGIN/DATA/FINISH framing + hash verification), `connection.set_root(hash)`. Wire-format encoders live in `fresco_socket::wire` (mirrors `scene::nodes::*` parsers). atrium-test-client refactored to use the lib (~30 lines) and a new bin `atrium-rect-bouncer` animates a rect bouncing across the viewport at 30 fps — every frame uploads 5 fresh blobs (transform/renderable/scene_node/node_list/scene_root) and SET_ROOTs. **Visually verified.**

**D1 step 2(c.6) (CommandFrontend wired into socket dispatch)** — DONE (2026-04-29). frescod now constructs the full fresco-server state stack: `CasStore` + `SceneGraph` + `SlotTable` + `WmCompositor::new_with_window0` (with `init_decorations`) → `CommandFrontend`. Socket connections call `frontend.dispatch(&cmd, client_id)` for every command. Per-connection unique u8 `client_id` assigned from a static `AtomicU8` counter. The full Fresco opcode set is now reachable over the wire — UPLOAD_*, SET_ROOT/CAMERA, all SLOT_* ops, CREATE_WINDOW, FRAME_BEGIN/END, etc. — though multi-window FBO rendering still TODO in `TinySkiaBackend::sync_fbos / render_window_to_fbo`.

Lock-order rule: **cas first, scene second** (matches `CommandFrontend::handle_set_root`). The render loop must follow this or socket threads + render thread deadlock under contention.

Verified: magenta-rect demo + 30fps bouncer both work through CommandFrontend with no protocol or visual changes. ~150 ops/sec sustained, no deadlocks.

**D1 step 2(c.7) (slot-graph rendering path)** — DONE (2026-04-29). Slot-graph apps work end-to-end without per-window FBO infrastructure. fresco-socket-rs gained `frame_begin/end`, `slot_alloc_renderable(slot_id, xform, renderable)`, `slot_set_root`, `slot_set_content`, `slot_set_xform_inline`, `slot_free`. New demo `atrium-slot-demo` animates a yellow-orange rect with **3 commands/frame** (FRAME_BEGIN + SLOT_SET_XFORM + FRAME_END) — matches the protocol shape atrium-edit/atrium-term use on the kmod transport.

Two bugs fixed during bring-up:
- `SLOT_FLAG_VISIBLE` (0x01) MUST be set in CMD_SLOT_ALLOC's flags. Without it `SlotTable::traverse_slot` early-returns silently.
- frescod's render loop must NOT call `scene.traverse(&mut cas)` when `root_hash == NULL_HASH` — traverse() unconditionally clears render_list before its early-return, wiping the slot-built list.

**D1 step 2(c.8) (textured material rendering)** — DONE (2026-04-29). `TinySkiaBackend` decodes `NODE_TEXTURE` blobs into `tiny_skia::Pixmap`s (cached by hash), and renders `NODE_MATERIAL_TEXTURED` items via tiny-skia's `Pattern` shader. fresco-socket-rs gained `wire::pixel_data`, `wire::texture`, `wire::material_textured`, plus `Connection::upload_texture(rgba, w, h)` convenience. `atrium-textured` demo (256×256 checkerboard + xy gradient) visually verified.

Subtle correctness fix during bring-up: tiny-skia's `fill_path` post-concats its `transform` argument onto the shader's via `Shader::transform`. Pattern.transform must therefore map pattern-pixel-space → **path-local space**, not pattern→pixmap space — `Transform::from_scale(1.0/tex_w, 1.0/tex_h)` for our unit-rect mesh.

**D1 step 2(c.9) (text rendering on FreeBSD-native)** — DONE (2026-04-29). atrium-text-demo (new bin in atrium-test-client) shapes a string with `fresco-text` (rustybuzz + swash), extracts each glyph's sub-image from the master atlas as premultiplied (A,A,A,A) RGBA, uploads each as its own CAS texture, and emits a textured RenderItem per glyph. **Visually verified**: "Hello, FreeBSD!" at 64px, anti-aliased, properly kerned. The first text rendered on the Atrium FreeBSD-native stack.

Per-glyph-as-texture is wasteful (14 textures for 14 glyphs); the canonical approach uses a SHARED atlas with per-vertex UVs and a single textured material. That requires vertex stride 20 (POSITION+UV) handling in `tiny_skia_backend`, deferred to step 2(c.10).

**D1 step 2(c.10) (atrium-edit-socket — first real app on FreeBSD-native)** — DONE (2026-04-29). New crate `~/src/bsd/atrium-edit-socket/` reuses atrium-edit's `buffer.rs` + `keymap.rs` unchanged. New `glyph_cache.rs` shapes each printable-ASCII character individually via `fresco_text::shape_and_rasterize`, uploads one texture per character, and remembers `(material_hash, metrics)` per glyph. New `render.rs` walks the buffer's visible window and emits one RenderItem per glyph in screen-pixel coords (no camera projection — uses `flags=0x01` ortho path).

**Visually verified**: `atrium-edit-socket /mnt/host/test-assets/scratch.txt` renders the full file content — title, separator, command listing, status line. Reads as a real text editor's output.

v0.1 limitations:
- Single-shot render — no event loop. Bidirectional events (poll_event / wait_event) come in step 2(c.11).
- Per-glyph texture (~93 textures uploaded at startup for ASCII printables); per-vertex UV with shared atlas is step 2(c.12).
- No cursor visualization. Editor logic in buffer.rs is intact; just not drawn yet.

**D1 step 2(c.11) (bidirectional events)** — DONE (2026-04-29). fresco-socket-rs gained `Event` enum (`WindowCreated`, `WindowResized`, `CloseRequested`, `WindowFocus`, `Other`) and `Connection::poll_event` / `wait_event(timeout)` APIs. Inbound 128-byte Completions are demuxed by `comp_type`: response-shaped (UPLOAD_COMPLETE) returns to the command call that asked; event-shaped (WINDOW_CREATED, etc.) lands in `pending_events`. New `Connection::create_window(w, h, title)` and `destroy_window` helpers. Verified via new `atrium-window-demo` binary: CREATE_WINDOW → `window_id = 1` returned via the event path.

Async WM-emitted events (CloseRequested when titlebar X clicked, WindowResized when titlebar drag-resizes) won't fire today — they require real input. Step 2(c.12) plumbs `/dev/usbhid` so input enters CommandFrontend, the WM acts on it, and async events flow back through the per-connection completion stream.

**D1 step 2(c.12) (per-connection async writer + ticker, atrium-edit-socket cursor blink)** — DONE (2026-04-29). frescod's `socket_server` now runs **two threads per connection**: reader (commands → CommandFrontend → response) and writer (drains an `mpsc::Receiver<Completion>` to the socket). Reader sends responses through the same channel. A shared `Vec<Sender<Completion>>` (`event_subs`) lets async producers fan out events to every connected client. The reader's send-failure on a dead writer cleanly tears down the connection.

Stand-in async producer: a 1 Hz "ticker" thread broadcasts `COMP_WINDOW_FOCUS` toggles to all subscribers. atrium-edit-socket's renderer toggles `cursor_visible` on each event and re-renders → server-driven cursor blink. Verified in the QEMU window: yellow cursor block on the file's content. The event loop pattern (`while let Some(ev) = conn.wait_event(None)? { ... }`) is now the editor's main loop — drop-in ready for real input events.

**D1 step 2(c.13) (interactive atrium-edit-socket via injected keystrokes)** — DONE (2026-04-29). New protocol primitive `CMD_INJECT_KEY` (vendor opcode 0xF000) lets a client push HID-coded keystrokes into frescod's event broadcast. frescod's `reader_loop` intercepts the opcode, builds `COMP_INPUT_KEY` (0x14) Completions, fans out via `event_subs`. fresco-socket-rs gained `Event::Key { window_id, hid_usage, pressed, modifiers }` and `Connection::inject_key`. New `atrium-keyboard` test client maps an ASCII string to HID Usage codes (with shift modifiers) and injects them as down/up pairs at 25 cps. atrium-edit-socket's wait_event loop receives Key events, runs through the existing keymap → buffer-mutation → re-render path. **Visually verified**: "Hello atrium-edit on FreeBSD!" typed into an empty buffer, cursor visible at end, status shows `[*] [new buffer]`.

Two correctness fixes during bring-up:
- `set_read_timeout(Duration::from_millis(0))` is rejected on FreeBSD; `Connection::poll_event` switched to `set_nonblocking(true) / set_nonblocking(false)`.
- ssh-spawned editor processes die when their parent ssh closes. Use `nohup … >log 2>&1 < /dev/null &` to detach for unattended runs.

**D1 step 2(c.14) (native FreeBSD keyboard input — `/dev/input/event3` via hkbd → evdev)** — DONE (2026-04-29). frescod's `input_reader` thread opens the keyboard event device (probed by sysctl name match against "kbd"/"keyboard"), reads 24-byte `struct input_event` records, translates Linux keycodes to USB HID Usage Page 0x07, tracks shift/ctrl/alt modifier state, and broadcasts `COMP_INPUT_KEY` via `event_subs`. Source is evdev for tractability today; the wire stays HID per the protocol contract.

QEMU args added in `scripts/run-vm.sh --display`: `-device qemu-xhci -device usb-kbd -device usb-tablet`. The USB keyboard appears as `hkbd0` → evdev `event3`. The kbdmux0 multiplexer (event0) sees no input on `-machine virt` since there's no console keyboard, so we explicitly skip "multiplexer" devices in the probe.

**Visually verified**: typing in the QEMU Cocoa window appears in atrium-edit-socket in real time. `[*]` modified flag confirms each keystroke mutates the buffer. Screenshot shows two lines of typed text with cursor at end.

D1 step 2(c.15)+ candidates: cleared.

**D1 step 2(c.22) (atrium-bootfb kmod + atrium-splash binary)** — INFRASTRUCTURE COMPLETE; QEMU GOP-source pending (2026-04-29).

Goal: paint a boot splash on the EFI GOP framebuffer between the kernel handing off to userspace and `atrium-virtio-gpu` claiming the scanout via `/dev/atrium-display0`. No quick workarounds — proper kmod + rc.d service path.

**Components (all built, all cross-compile clean)**

- `atrium-kmod/bootfb/` — `atrium_bootfb.ko`. At `MOD_LOAD` reads `MODINFOMD_EFI_FB` from the bootloader's preserved metadata (`preload_search_info(preload_kmdp, MODINFO_METADATA | MODINFOMD_EFI_FB)` — the same lookup `vt_efifb` uses), publishes `/dev/atrium-bootfb0` with `d_mmap` returning the framebuffer's physical pages and `d_ioctl ATRIUM_BOOTFB_IOC_GET_INFO` reporting width/height/stride/format/masks. `VM_MEMATTR_WRITE_COMBINING` for the mapping so sequential writes batch. No exclusive ownership of the device — `vt_efifb` may also be writing console text into the same memory; the splash overpaints it.
- `atrium-bootfb-rs/` — safe Rust binding (`BootFb::open` → mmap'd `&mut [u8]`).
- `atrium-splash/` — userspace splash binary using the existing `tiny_skia::Pixmap`. Renders a gradient panel + hand-drawn "atrium" wordmark + an orbiting indicator dot at 30 fps. Polls `/dev/atrium-display0` and exits cleanly when it appears (handoff to frescod).
- `atrium-splash/atrium-splash.rc` — rc.d service script. `REQUIRE: mountcritlocal`, `BEFORE: FILESYSTEMS netif` so it paints the moment the root FS is up.

**Setup on the VM (or any FreeBSD UEFI system)**

```sh
# One-time install — kmod into /boot/modules so loader.conf can preload.
cp /mnt/host/atrium-kmod/bootfb/atrium_bootfb.ko /boot/modules/
echo 'atrium_bootfb_load="YES"' >> /boot/loader.conf

# Splash binary + rc.d service
cp /mnt/host/atrium-splash/target/aarch64-unknown-freebsd/release/atrium-splash \
   /usr/local/libexec/atrium-splash
cp /mnt/host/atrium-splash/atrium-splash.rc /usr/local/etc/rc.d/atrium_splash
chmod +x /usr/local/etc/rc.d/atrium_splash
echo 'atrium_splash_enable="YES"' >> /etc/rc.conf

reboot
```

The kmod is preloaded by `loader.efi` *before* the kernel's main thread starts — it attaches at `SI_SUB_DRIVERS` order so `/dev/atrium-bootfb0` exists before any userspace code runs. The rc.d service starts the splash binary right after `mountcritlocal`. Splash exits when atrium-virtio-gpu (or another native GPU driver) creates `/dev/atrium-display0`.

**The QEMU caveat** (why this isn't visually verified in our bring-up VM)

The prebuilt EDK2 firmware bundled with our QEMU build doesn't ship `VirtioGpuDxe`, so `virtio-gpu-pci` produces no GOP. We tried `bochs-display` (which has `BochsDisplayDxe`) — same story: no `MODINFOMD_EFI_FB` metadata reaches FreeBSD. `vt_efifb`'s probe returns `CN_DEAD` for the same reason; vt falls back to a non-graphical backend and the kernel runs with serial console only. This is a firmware-level limitation, not a kmod problem.

On real UEFI hardware (laptops/desktops/servers), GOP is universally present and the standard `MODINFOMD_EFI_FB` flow works — atrium-splash will paint as designed.

For a future verification path in QEMU: a more recent EDK2 build with `VirtioGpuDxe` and/or `BochsDisplayDxe` is the cheap fix. `scripts/run-vm.sh --bochs --display` swaps virtio-gpu for bochs-display so when the EDK2 build catches up, that mode exercises splash without further changes.

**D1 step 2(c.19) (multi-window FBO compositing in tiny_skia_backend)** — DONE (2026-04-29). `tiny_skia_backend` gains a per-window FBO map (`window_fbos: HashMap<u16, Pixmap>`):

- `sync_fbos(live)` reconciles the map against the WM's live windows each frame — allocates new FBOs at the right size, drops FBOs whose window is gone or whose dimensions changed.
- `render_window_to_fbo(id, scene, cas)` clears the FBO and rasterizes that window's render_list into it. Each window has its own `Arc<Mutex<SceneGraph>>` from `WmCompositor`; routable client commands (CMD_SET_ROOT, CMD_SLOT_*, CMD_FRAME_*) carry `cmd.flags = window_id` to dispatch to the right scene.
- `render_screen_with_windows(scene, cas, layered)` clears the screen, rasterizes the screen scene, then for each window in z-order: blits its FBO, strokes a 1-px border, rasterizes that window's decorations (titlebar / close button / title text). The interleave is critical: a higher-z window's FBO blit covers a lower-z window's titlebar in the overlap region.

Wire support:
- `fresco-socket-rs::Connection` gains `default_window: u16` + `set_default_window(id)`. `write_cmd` stamps `cmd.flags = self.default_window` on routable opcodes (CMD_SET_ROOT, CMD_SLOT_*, CMD_FRAME_*) so apps can route their commands to their own window without hand-stuffing flags. `window_set_pos(id, x, y)` issues CMD_WINDOW_SET_POS.
- `WmCompositor::compose_overlay_for(id)` returns one window's decorations (extracted from the all-windows `compose_overlay`).

App side: `atrium-edit-socket` and `atrium-term-socket` now `create_window` + `window_set_pos` + `set_default_window` at startup.

**Quirks fixed during bring-up (in this slice)**

1. **Initial focus = window 0 broadcast every keystroke to every client.** WM init sets `focus = Some(0)` and apps don't auto-raise on CREATE_WINDOW. `input_reader` now resolves the effective focus: if the WM still has window 0 focused but real client windows exist, route to the topmost non-zero window in `z_order`. Click-to-focus continues to work normally; this is just a sane default for the "no clicks yet" state.
2. **Lower-z titlebar drawn over higher-z content.** `compose_overlay` returned all decorations as one slice and the backend rasterized them after all FBO blits — so a lower-z window's titlebar appeared on top of a higher-z window's content area. Fixed by interleaving per-window: for each window in z-order, blit its FBO then rasterize *that window's* decorations, so higher-z FBOs cover lower-z titlebars in the overlap.
3. **Close button "did nothing".** Click was hitting the close button correctly and the app was exiting cleanly, but the WM still had the window registered and the FBO continued to render — so the visible window stayed on screen even though the process was gone. `socket_server::reader_loop` now calls `cleanup_client_windows` on disconnect, walking the WM and `destroy_with_focus_shift` on every window owned by the leaving client. `tiny_skia_backend::sync_fbos` then drops the orphaned FBO on the next frame.
4. **Two clients launched simultaneously hit a CAS hash mismatch.** Sequential launch (one app, sleep, next app) avoids it. Root cause is in the upload path under contention; not yet fixed for parallel launch — workaround documented.

Window background uses alpha `0xE0` (~88% opaque) so faint show-through hints at the layering. Apps can fill themselves opaque if they want full opacity.

**D1 step 2(c.20) (parallel-launch CAS race + atrium-clock-socket)** — DONE (2026-04-29).

Race fix: `CasStore::upload_staging` was keyed by `cmd.sequence_id` (u32). Each client allocates sequence ids from its own `next_seq` counter starting at 1, so two clients' uploads collided on the same key — bytes interleaved, hashes mismatched, the second app errored out with "server hash does not match local hash". Key changed to `u64`; the frontend composes it as `(client_id as u64) << 32 | sequence_id` so the namespace is per-client. Two apps now upload concurrently without contention.

New crate `atrium-clock-socket` (`~/src/bsd/atrium-clock-socket/`): a simple analog clock — 12 hour ticks (with quarter-hour ticks twice as thick) + hour/minute/second hands + a small red center hub + a 1 Hz `EVFILT_TIMER`-driven re-render. Pure-geometry app (no glyph atlas, no pty, no buffer); validates that the multi-window FBO pipeline works for non-text content. Each tick/hand uses one shared centered-unit-rect mesh `(-0.5..0.5)²` and a per-frame `oriented_rect` 4×4 matrix encoding `T(cx,cy) · R(θ) · S(length, thickness)`.

Three-app verification: editor + terminal + clock launched in parallel via a single `vssh` command, all three created their windows, uploaded glyph atlases / clock geometry, and rendered onto the screen scene with z-ordered FBO compositing. Visually verified.

**D1 step 2(c.21) (final candidate cleanup)** — DONE (2026-04-29).

- **`CMD_INJECT_KEY` cfg-gated.** The vendor-extension opcode that lets a client push synthetic key events is now behind the `inject-input` Cargo feature on `frescod`, default off. Production builds reject the opcode with a logged warning; test/automation builds (`cargo build --features inject-input`) keep working. `atrium-keyboard` test client still depends on this opcode being accepted, so it requires the feature.

- **Real HID descriptor parsing.** `pointer_reader` no longer hardcodes the QEMU `usb-tablet` 6-byte report layout. It runs `HIDIOCGRDESC` at startup, walks the descriptor (Main / Global / Local items), and builds a `PointerLayout` recording bit offsets + sizes for buttons / X / Y / wheel plus an absolute-vs-relative flag for the X/Y axes. `decode_report` then walks any pointer's reports against the layout. Verified against the QEMU usb-tablet (5-button absolute). Real USB mice with relative axes + wheel decode through the same code path; cursor advances by the report's signed deltas instead of being scaled from absolute axes.

- **atrium-find-socket** (`~/src/bsd/atrium-find-socket/`): two-pane file browser ported to fresco-socket-rs. `dir.rs` and `keymap.rs` copied unchanged from atrium-find (already HID-shape and protocol-agnostic); new `glyph_cache.rs` mirrors atrium-edit-socket's atlas mode; new `render.rs` emits per-glyph render items in screen-pixel space across header / left list / right preview / footer layout. Selection highlight is a translucent indigo bar under the selected row.

Four-app multi-window demo: `atrium-edit-socket` + `atrium-term-socket` + `atrium-clock-socket` + `atrium-find-socket` launched in parallel via a single `vssh` command, each in its own window, FBO-composited together. Validates the full multi-window pipeline with four very different content types (text editor, pty-driven terminal, animated geometry, list-driven file browser).

**D1 step 2(c.15) (mouse routing + drag/resize/close-button)** — DONE (2026-04-29). New `pointer_dispatch` module owns WM intercepts (close button → resize edge → titlebar → content+raise+focus) and per-window event routing; both readers feed it cooked events. Software cursor overlay drawn directly into the tiny-skia pixmap each frame. Click auto-raises and focuses; drag/resize match the existing macOS-path semantics (emits `COMP_WINDOW_RESIZED` to the owner on release).

**D1 step 2(c.16) (native keyboard + pointer via /dev/hidraw*)** — DONE (2026-04-29). Both readers open the kernel's hidraw cdev directly; no AT-scancode translation, no Linux-shape evdev. Identification is by HID report descriptor (`HIDIOCGRDESC` ioctl):

- `input_reader` finds the first hidraw whose descriptor starts with USAGE_PAGE(Generic Desktop) ; USAGE(Keyboard) (`05 01 09 06`). Reads 8-byte USB HID Keyboard boot reports. Bytes are *already in the wire format we want* — bytes 2..7 carry HID Usage Page 0x07 codes verbatim. Press/release derived by diffing successive reports' key sets; modifier byte at offset 0 maps to the wire's 3-bit shift/ctrl/alt bitmap.
- `pointer_reader` finds the first hidraw whose descriptor starts with `05 01 09 02` (Mouse). Hardcodes the QEMU usb-tablet's 6-byte report layout (1 byte buttons + 2× 16-bit absolute axes + signed wheel). HID button bits map to Usage Page 0x09 numbers (1=primary, 2=secondary, 3=middle); Linux `BTN_*` doesn't appear anywhere.

Tried first: `/dev/kbd0` `KDSKBMODE(K_RAW)`. On modern hidbus FreeBSD `/dev/kbd0` is the `kbdmux(4)` multiplexer view and rejects `K_RAW` (`ENOTTY`); the underlying physical keyboard `/dev/kbd1` is held exclusively by kbdmux. Hidraw works in parallel — no driver detach, no exclusive-claim juggling.

Tried first: `/dev/uhid*`. On modern systems with `hms(4)`/`hkbd(4)` claiming the HID interfaces via hidbus, `/dev/uhid*` doesn't get created. Detaching `ums0` and reattaching as `uhid` only works on systems that still have the legacy `ums(4)` driver — modern is `hms(4)` on hidbus, which uses `hidraw(4)` as the parallel-access path.

Setup: load `hidraw.ko` at boot. `scripts/first-boot-setup.exp` runs `sysrc kld_list+=hidraw` so it's persistent. To apply to an already-set-up VM:

```sh
vssh "kldload hidraw && sysrc kld_list+=hidraw"
```

`pointer_reader` falls back to legacy `/dev/uhid*` if no hidraw descriptor matches — keeps the legacy path alive for any system that ships uhid instead of hidraw.

**Quirk fixed during bring-up:** hidraw enforces exclusive open. Spawning the keyboard and pointer reader threads in parallel races: both probe `/dev/hidraw*` looking for their device, and one gets EBUSY for the device the other just opened (probe-and-close happens fast but the kernel's exclusive-lock release lags). Fix is in `main.rs`: spawn pointer first, sleep 200 ms, then spawn keyboard. The probes are now sequential.

**D1 step 2(c.18) (per-vertex-UV / atlas-mode glyphs + atrium-term-socket)** — DONE (2026-04-29). `Material.uv_region: [f32; 4]` extends `NODE_MATERIAL_TEXTURED` with an optional `[36..52]` block carrying `u0, v0, u1, v1`. `tiny_skia_backend` builds the `Pattern.transform` from the material's UV region — full-texture default `[0, 0, 1, 1]` reduces to the old `scale(1/tex_w, 1/tex_h)`; sub-rect UVs slice a shared atlas. fresco-socket-rs gains `wire::material_textured_uv` and `Connection: AsRawFd` (lets apps register the socket fd in their own kqueue alongside the pty fd).

`atrium-edit-socket/glyph_cache.rs` rewritten to atlas mode: shapes printable ASCII as one `GlyphAtlas`, uploads pixels as a single CAS texture, builds 94 `material_textured_uv` blobs each pointing at one glyph's UV cell. Storage is now 1 atlas + 94 thin materials instead of 94 separate textures. Deduplication wins double: the atlas is the only large blob, and the unit-rect mesh is shared across every glyph in every visible character.

`atrium-term-socket` (new crate at `~/src/bsd/atrium-term-socket/`) ports atrium-term to fresco-socket-rs. `grid.rs` / `keymap.rs` / `pty.rs` copied unchanged from atrium-term (already protocol-agnostic). New `glyph_cache.rs` mirrors the editor's atlas mode. `main.rs` uses `kqueue` to multiplex the pty master fd and `Connection::as_raw_fd()` so a single `kevent()` wakes on either pty output or a server event.

### Verification (M3 stack — Vulkan + envelope wire)

After the M2.7 / M3 cutover, frescod renders via Vulkan
(`HeadlessRenderer` + SPIR-V bundle dispatch) instead of tiny-skia.
Apps speak the envelope-based wire (CLASS_DISPLAY) via `fresco-client`
instead of fresco-socket-rs's 128-byte `Command` / `Completion` shape.
Verification therefore needs three new things on the guest:

  1. **A Vulkan ICD** — Mesa lavapipe is the SW path. virtio-gpu
     doesn't expose Vulkan from QEMU's stock virgl, so lavapipe
     (Mesa's CPU rasterizer) is what makes `vkCreateInstance` succeed
     in the guest.
  2. **The atrium-core SPIR-V bundle** — frescod loads
     `bundles/atrium-core/{compute,pipelines}/*.spv` at startup. The
     `.spv` files are gitignored; build them once on the host.
  3. **A bundle path the in-VM frescod can find** — default search
     looks at `<frescod manifest dir>/../bundles/atrium-core` and
     `/usr/local/share/atrium/bundles/atrium-core`. Cross-compiled
     frescod runs from `/mnt/host/frescod/target/.../frescod`, so the
     workspace-relative search lands at `/mnt/host/bundles/atrium-core`
     — which works as long as `build.sh` was run on the host (the
     `.spv` files end up under `~/src/bsd/bundles/atrium-core/` and
     are visible to the guest via the 9p share).

Cross-compile everything on the host (~3 s for incremental, longer
on a clean tree):
```sh
# Build the SPIR-V bundle once. Requires `glslang` on the host
# (`brew install glslang`).
~/src/bsd/bundles/atrium-core/build.sh

# All four shipping apps + frescod.
for c in frescod atrium-clock-socket atrium-edit-socket \
         atrium-term-socket atrium-find-socket; do
    cd ~/src/bsd/$c && cargo build --release --target aarch64-unknown-freebsd
done
```

Boot the VM with the QEMU window so virtio-gpu scanout is visible:
```sh
~/src/bsd/scripts/run-vm.sh --virtio-gpu --display
```

Guest-side setup (one-time per fresh VM, persisted via `sysrc`):
```sh
# 9p share + Atrium GPU kmod (D0 work — already covered above)
vssh "kldload p9fs; mount -t p9fs -o trans=virtio bsd_share /mnt/host"
vssh "kldload /mnt/host/atrium-kmod/atrium_virtio_gpu.ko"
vssh "devctl set driver -f vtgpu0 atrium_virtio_gpu" || true
vssh "devctl enable atrium_virtio_gpu0"

# Hidraw — keyboard + mouse access for frescod's input readers.
vssh "kldload hidraw && sysrc kld_list+=hidraw"
vssh "ls /dev/hidraw*"   # expect /dev/hidraw0 (kbd) /dev/hidraw1 (mouse)

# Mesa with lavapipe (Vulkan ICD). Confirmed working pkg names on
# FreeBSD-CURRENT 16.0-CURRENT main-n285005-e9fc0c538264:
#
#     pkg install -y mesa-devel vulkan-loader vulkan-tools
#
# `mesa-libs` is OpenGL-only — the Vulkan drivers (lavapipe + others)
# ship in `mesa-devel` (despite the name; it's the rolling-Mesa port).
# Sanity check with `vulkaninfo --summary`; the line
#     deviceName = llvmpipe (LLVM 19.1.7, 128 bits)
# confirms lavapipe is selected.
#
# Heads-up: on -CURRENT the FreeBSD-base repo's metadata may be tagged
# for an OS version pkg considers "wrong", causing `pkg update` to
# error out. Workaround:
#     echo 'FreeBSD-base: { enabled: no }' > /usr/local/etc/pkg/repos/FreeBSD.conf
# (FreeBSD-ports + FreeBSD-ports-kmods still update normally and
# carry mesa-devel + vulkan-*.)
vssh "echo 'FreeBSD-base: { enabled: no }' > /usr/local/etc/pkg/repos/FreeBSD.conf
      pkg install -y mesa-devel vulkan-loader vulkan-tools
      vulkaninfo --summary | head -20"
```

The pristine vm.qcow2 ships with a 5 GB rootfs; mesa-devel + 25 deps
add ~270 MB of extracted files plus another ~50 MB of pkg cache,
which doesn't fit. Grow the disk first (one-shot, persists):
```sh
vssh "shutdown -p now"
qemu-img resize ~/src/bsd/vm/vm.qcow2 +20G
~/src/bsd/scripts/run-vm.sh --virtio-gpu --display &
until vssh true 2>/dev/null; do sleep 5; done
vssh "gpart recover vtbd1; gpart resize -i 3 vtbd1; growfs -y /"
# (Note: vtbd1, not vtbd0. The crash-test.img drive in run-vm.sh
#  takes the vtbd0 slot, pushing the rootfs disk to vtbd1.)
```

> **✅ atrium-virtio-gpu kmod attach — RESOLVED 2026-05-05 (e210165).**
> The fix was a one-line probe-priority bump (`BUS_PROBE_DEFAULT` →
> `BUS_PROBE_VENDOR` in `atrium_virtio_gpu_probe`). With this, atrium
> wins the virtio-child probe race against vtgpu at boot — vtgpu's
> attach never runs, vt(4) never gets the bad framebuffer callback
> registered, and `/dev/atrium-display0 + /dev/atrium-gpu0` come up
> cleanly with no runtime devctl. Required loader.conf:
>
>     atrium_virtio_gpu_load="YES"
>
> (plus the .ko in /boot/modules/.) The `hint.vtgpu.0.disabled="1"`
> recommendation in earlier session notes is no longer needed; probe
> priority handles it. **Use the boot-time path; never use
> `devctl set driver -f vtgpu0 atrium_virtio_gpu` at runtime — it
> still triggers the panic described below.**
>
> Verified end-to-end in QEMU+HVF: frescod-vulkan-smoke +
> atrium-test-client renders pixel-identical to the host MoltenVK
> build via Mesa lavapipe.
>
> Diagnosis history (kept for reference):

> **⚠ atrium-virtio-gpu kmod attach causes a vt(4) NULL-deref panic
> on -CURRENT (2026-05-05) — root cause confirmed via ddb.** What
> looked like a deadlock is actually a panic-inside-panic: the
> first panic is in `vt_timer → vt_flush → vtgpu_fb_bitblt_text →
> vt_fb_bitblt_bitmap` with `panic: Offset 0x000002 out of fb size`,
> the second is `vtterm_cngrab` faulting on `0x1` while trying to
> grab the console for the panic message — which keeps the kernel
> alive in a half-dead state long enough that ssh times out instead
> of cleanly rebooting on panic. ddb capture from `~/src/bsd/scripts/
> ddb_session.py break` shows the trace cleanly.
>
> Mechanism: when `atrium_virtio_gpu` is attached to virtio_pci3 via
> `devctl set driver -f vtgpu0 atrium_virtio_gpu`, vtgpu's softc is
> torn down via its detach path. vt(4) holds a stale callback
> pointer to `vtgpu_fb_bitblt_text` (vtgpu registered as vt's
> framebuffer backend at attach time and doesn't deregister on
> detach). The next periodic `vt_timer` tick (~1s later) calls into
> the freed bitblt state and panics on `info->fb_size == 0`.
>
> The earlier in-source patches (cv_wait + lazy GET_DISPLAY_INFO)
> didn't and couldn't fix this — the panic is in vt's stale
> callback, not in atrium_virtio_gpu's code at all. Those patches
> are still architecturally correct (the controlq path was a
> bring-up shortcut and should be cleaned up) but they're not the
> fix for this hang.
>
> **The proper fix is to never go through vtgpu's detach** — i.e.
> have atrium_virtio_gpu claim virtio_pci3 *before* vtgpu's identify
> creates the placeholder vtgpu0 child. Steps tried that DON'T
> work:
>
> - `hint.vtgpu.0.disabled="1"` in loader.conf: vtgpu's identify
>   still creates the *named* `vtgpu0` placeholder even when its
>   attach is disabled by the hint. virtio_pci3 sees a named child
>   and never offers the slot to other drivers.
> - `atrium_virtio_gpu_load="YES"` preload: the kmod is loaded
>   early but VIRTIO_DRIVER_MODULE registration alone doesn't make
>   it claim a slot already occupied by a named child of a
>   different name.
> - `devctl delete -f vtgpu0` post-boot: removes the placeholder,
>   but `devctl rescan virtio_pci3` reports "Operation not
>   supported by device" — the virtio bus doesn't implement the
>   rescan ivar — so no re-probing happens.
>
> The proper fix is to **add an `identify` routine to
> `atrium_virtio_gpu`** that creates an `"atrium_virtio_gpu"`-named
> child of virtio_pci3 at boot, with a `DEVICE_PASS` that runs
> before vtgpu's identify. Then atrium_virtio_gpu owns the slot
> from the start; vtgpu never attaches; vt never gets the bad
> callback registered. Estimated patch: ~30 lines (DEVICE_IDENTIFY
> method + a class identify routine). Reference: `sys/dev/virtio/
> *.c` for identify-based virtio attach patterns.
>
> Until that patch lands, full-stack scanout verification is
> blocked. ddb traces below for reference.

> **⚠ atrium-virtio-gpu kmod attach hang signature (pre-ddb
> diagnosis above).**
>
> ```sh
> kldload /mnt/host/atrium-kmod/atrium_virtio_gpu.ko
> devctl set driver -f vtgpu0 atrium_virtio_gpu     # exit 0, silent
> # kernel deadlocks within ~1 second; SSH dies, only kill -9 qemu recovers
> ```
>
> The `set driver -f` returns exit 0 — and `devctl enable
> atrium_virtio_gpu0` (if you race it) reports "Device busy", proving
> the new attach completed and `atrium_virtio_gpu0` was created. The
> deadlock is in the `set driver -f` itself: vtgpu's implicit detach
> + atrium_virtio_gpu's attach interact with virtio bus locks in a
> way the kmod (last built Apr 28) wasn't safe against. Historical
> "worked" runs were timing-lucky.
>
> Confirmed *not* the cause: post-Apr-30 session state, mesa-devel
> install, growfs, loader.conf preloads, hint.vtgpu.0.disabled,
> command-batching style. Same hang with a one-line trigger on
> pristine baseline.
>
> Suspected cause: kernel ABI drift since Apr 28 — vtgpu's
> `detach()` and/or virtio_pci's bus locks gained order constraints
> the kmod doesn't honor. **Path forward:** rebuild the kmod in-VM
> against current kernel headers (C builds in the guest are fine
> per §5 quirks; only `cargo --release` is forbidden). If the rebuild
> doesn't fix it, instrument with WITNESS / lock-order tracing and
> follow the lock chain.
>
> **Investigation update (2026-05-05):** rebuilt the kmod in-VM
> against the current kernel headers — same hang. So it's not ABI
> drift. Found a real bring-up shortcut in the kmod's controlq
> path: `VQ_ALLOC_INFO_INIT(..., NULL, ...)` registers no interrupt
> callback, and `atrium_vgpu_req_resp` busy-polls via
> `virtqueue_poll` while holding `ctrl_lock`. This worked when MSI-X
> for the controlq wasn't being delivered; on modern -CURRENT the
> host fires the IRQ at completion and with no callback the IRQ
> stays pending. Wrote a callback + cv_wait patch (`cv_init` in
> attach, real intr handler that dequeues + `cv_signal`s, replace
> `virtqueue_poll` with `while (!ctrl_done) cv_wait(...)`) — the
> patch builds cleanly but **still hangs at `set driver -f`**. So
> the IRQ-storm theory was incomplete; there's a second issue,
> probably sleeping in attach context conflicting with a newbus
> topology lock the patched cv_wait now hits. Next-session moves:
> attach kgdb (`-s` to qemu, `kgdb` from host); enable WITNESS in
> the kernel build for a real lock-order trace; or defer
> `GET_DISPLAY_INFO` out of attach entirely (e.g. on first cdev
> open) so attach doesn't sleep at all. The patch is in-tree
> (`atrium-kmod/atrium_virtio_gpu.c`) since the structural
> direction (callback-driven controlq) is correct even if it
> didn't fully unstick this hang.
>
> Until resolved, scanout-integrated runs of frescod + the migrated
> apps are blocked. The no-scanout subset (`frescod-vulkan-smoke`
> renders to PNG without ever opening `/dev/atrium-display0`)
> remains the actionable substitute for the rendering-pipeline
> correctness check — once `frescod-vulkan-smoke`'s separate
> `vkCreateInstance` issue (see below) is sorted.

> **⚠ frescod-vulkan-smoke fails `vkCreateInstance` on FreeBSD/aarch64
> with mesa-devel-24.1.7 + vulkan-loader-1.4.336 (2026-05-05).**
> `vulkaninfo --summary` reports `llvmpipe (LLVM 19.1.7)` cleanly,
> proving lavapipe is reachable. The same shell session running
> `frescod-vulkan-smoke` errors with `HeadlessRenderer::new:
> create_instance`. Likely an ICD JSON path mismatch — vulkaninfo
> warned about
> `/usr/local/lib/libvulkan_radeon.so` differing from the installed
> `libvulkan_radeon-devel.so`, which suggests at least one ICD JSON
> points at a nonexistent .so name. ash-rs's loader may be stricter
> than vulkaninfo's. Investigating needs `find /usr/local/share -name
> '*.json'` (the `vulkan/icd.d` path was missing in our session,
> hinting the package layout is non-standard) and possibly
> `VK_DRIVER_FILES=/path/to/lvp_icd.aarch64.json` to override.
> Diagnose in a session not also fighting the kmod hang.

Launch frescod + all four apps (each via its own `vssh`):
```sh
# frescod itself. The default bundle search succeeds at
# /mnt/host/bundles/atrium-core — set FRESCOD_BUNDLE to override.
vssh "nohup /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod \
        > /tmp/comp.log 2>&1 < /dev/null &"
sleep 1

vssh "nohup /mnt/host/atrium-edit-socket/target/aarch64-unknown-freebsd/release/atrium-edit-socket \
        /mnt/host/test-assets/scratch.txt > /tmp/edit.log 2>&1 < /dev/null &"

vssh "nohup /mnt/host/atrium-term-socket/target/aarch64-unknown-freebsd/release/atrium-term-socket \
        > /tmp/term.log 2>&1 < /dev/null &"

vssh "nohup /mnt/host/atrium-clock-socket/target/aarch64-unknown-freebsd/release/atrium-clock-socket \
        > /tmp/clock.log 2>&1 < /dev/null &"

vssh "nohup /mnt/host/atrium-find-socket/target/aarch64-unknown-freebsd/release/atrium-find-socket \
        / > /tmp/find.log 2>&1 < /dev/null &"
```

Expected log output:

`/tmp/comp.log`:
```
frescod: connector N 1280x800 @ 60000 mHz, target 30 fps
frescod: atrium-core bundle loaded (3 ops)
frescod: listening on /tmp/frescod.sock
frescod: keyboard reading /dev/hidraw0
frescod: mouse reading /dev/hidraw1
```
The "(3 ops)" count covers rect + texture + path. If it says "(0 ops)"
then `bundle_path` failed — set `FRESCOD_BUNDLE=/mnt/host/bundles/atrium-core`
explicitly.

`/tmp/edit.log` (and similarly term/find):
```
buffer: N lines (path=Some("/mnt/host/test-assets/scratch.txt"))
connected to /tmp/frescod.sock
window 1 created — 720x540
glyph cache: 94 glyphs, line_height=20.0 baseline=14.5 cell_w=10.0
rendered initial view; now waiting for events
```
Note: the previous "glyph atlas: 512x512 px, 94 glyphs uploaded as
1 CAS texture" line is gone — M3d switched to per-glyph slots
(94 small CAS textures, 1 slot each).

Verification points (visual, in the QEMU window):
- **Magenta scenes render at all** — confirms the Vulkan path through
  lavapipe works end-to-end. If you see the Atrium teal background
  but no client content, frescod is alive but Vulkan rendering is
  failing silently; check `MESA_LOADER_DEBUG=1 vulkaninfo` for ICD
  resolution.
- **Atrium teal background** — the same `[0.04, 0.50, 0.55, 1.0]`
  clear color seen in host smoke renders.
- **Editor**: text renders at top-left `(16, 16)` per `scratch.txt`'s
  contents. Cursor block (yellow) at column 0 row 0 initially.
- **Terminal**: `$ ` prompt visible; cursor block half-alpha amber
  under the cursor cell.
- **Clock**: 12 white tick marks, three hands (white hour/minute,
  red second), red centre hub. The second hand sweeps once per
  second (1 Hz EVFILT_TIMER drives re-render).
- **Find**: cwd path at top, file list with `> selected/` marker on
  one row + indigo selection bar, preview pane on the right, footer
  showing `[name] N entries`.
- **Per-window keyboard routing** — type into the QEMU window. The
  keystroke routes to the M3 input_reader's "topmost window if focus
  is screen" fallback, so the most-recently-created window receives
  it. Closing apps in reverse order changes which one gets keys.

#### M3 known limitations vs the legacy stack

The new frescod is intentionally minimal at this milestone. The
following legacy capabilities are **not yet rewired** on the
HeadlessRenderer + EnvelopeFrontend path:

- **Server-drawn window chrome** (titlebars, close button, drag-to-
  move, edge resize). The `Compositor::init_decorations` /
  `compose_overlay` machinery still exists but isn't called by the
  new render loop — it expects the legacy SceneGraph + tiny-skia
  path. Apps appear as their content rect with no decoration.
- **Click-to-focus / click-to-raise.** `pointer_reader` emits
  `EV_INPUT_POINTER_BUTTON` envelopes but doesn't do WM intercepts
  (click → raise → focus-change). Focus moves only via the
  "topmost window when no explicit focus" fallback in `input_reader`.
- **Resize** — pointer button drag near edges doesn't run any WM
  resize logic; windows stay at their `WINDOW_CREATE`-requested
  dimensions. `EV_WINDOW_RESIZED` events therefore never fire.
- **Server cursor sprite.** Pointer position is tracked
  (`Compositor::cursor`) and fans out as `EV_INPUT_POINTER_MOTION`,
  but no cursor overlay is drawn. The QEMU window's host cursor is
  what you see.
These are the next concrete follow-ups on the M3-final → "Fresco is
real" path. (`WindowHints.initial_position` is honored at WINDOW_CREATE
as of `7eb833b` — the four migrated apps spread out instead of
stacking at the origin.) None of them block the rendering-correctness verification
above.

---

## 5. Quirks and gotchas (running list — append as found)

### macOS / APFS
- **APFS is case-insensitive by default** — `tar -xJf base.txz` fails because FreeBSD man pages have `ALQ.9.gz` and `alq.9.gz` (hardlinks). Workaround: extract only what's needed for cross-compile:
  ```sh
  tar -xJf tarballs/base.txz -C sysroot/ \
      --include='./usr/include/*' --include='./usr/lib/*' \
      --include='./lib/*' --include='./usr/libdata/*'
  ```

### download.freebsd.org CDN
- The `Latest/` symlink directory under `snapshots/VM-IMAGES/16.0-CURRENT/aarch64/` lists files but every file 404s. Use the dated subdirectory (e.g. `20260413/`) directly. Files there have suffixes like `-20260413-e9fc0c538264-285005.qcow2.xz`.

### QEMU EFI firmware
- `~/src/bsd/external/qemu-build/build/qemu-bundle/.../edk2-aarch64-code.fd` is a *symlink* pointing at `~/src/bsd/external/qemu-build/build/pc-bios/edk2-aarch64-code.fd`, but only the bz2 source exists. Decompress it:
  ```sh
  mkdir -p ~/src/bsd/external/qemu-build/build/pc-bios
  bunzip2 -kc ~/src/bsd/external/qemu-build/pc-bios/edk2-aarch64-code.fd.bz2 \
      > ~/src/bsd/external/qemu-build/build/pc-bios/edk2-aarch64-code.fd
  ```
- pflash files **must** be exactly 64 MiB. `cp` then `truncate -s 67108864`. The decompressed `.fd` is already 64 MiB so the truncate is a no-op but kept defensively in `run-vm.sh`.

### macOS HVF — venus shmem aliases QEMU heap unless host pages are pre-faulted
- Symptom: frescod (running venus over the QEMU host-visible BAR) aborts with `Fatal error 'mutex … own … is not on list 0x0 0x0' at line 138 in file /usr/src/lib/libthr/thread/thr_mutex.c`. Reading the venus ring `status` word (offset 0x80 of the ring shmem) returns the caller's own `pthread.tid` instead of a valid `VkRingStatusFlagsMESA` value, and venus writes into the "shmem" clobber adjacent libthr struct-pthread fields (specifically the per-thread `mutexq` head's `tqe_prev`).
- Root cause: HVF's `hv_vm_map` captures the host VA→PA backing for the guest BAR aperture at call time. Anonymous SHM-fd-backed pages handed to `hv_vm_map` may not be faulted in yet; HVF then captures stale host pages (CoW-zero, or whatever QEMU's heap previously laid down there) for the guest's BAR view.
- Fix lives in **two places**:
  - `external/qemu-build/hw/display/virtio-gpu-virgl.c` `virtio_gpu_virgl_map_resource_blob`: under `CONFIG_DARWIN`, write a non-zero sentinel then zero into every host page of `data` (forces unique allocation) and `mlock` the range, all before `memory_region_add_subregion_overlap` runs the MemoryListener → `hv_vm_map`.
  - `external/mesa/src/virtio/vulkan/vn_ring.c`: `atrium_ring_mutex_init` puts the ring's pthread mutex on its own page-aligned allocation via `_pthread_mutex_init_calloc_cb`. This is defense-in-depth — even if a residual HVF aliasing event slips past the QEMU pre-fault, it cannot land on the same page as libthr's struct pthread bookkeeping.
- The architecturally cleaner fix would be in Apple's HVF kernel side. File against Apple if a developer relationship channel exists.

### Dev VM has LGPL/GPL deps from old `dejavu` pkg install — must not ship in runtime image
- `pkg info dejavu` was installed during early bring-up. It pulls in `fontconfig` (MIT) → `freetype2` (FTL/GPLv2+ dual) → `gettext-runtime` (LGPL21+/GPLv3+) and `libiconv` (GPLv3). Atrium runtime policy is permissive-only; the dev VM is fine for now but the production root image (D5+ Tessera-root) must rebuild fonts from `test-assets/DejaVu*.ttf` directly (Bitstream Vera license = permissive) without the `dejavu` pkg. Copy script:
  ```sh
  mkdir -p /usr/local/share/fonts/dejavu
  cp ~/src/bsd/test-assets/DejaVu*.ttf /usr/local/share/fonts/dejavu/
  ```
- Frescod's font search path (`fresco-scene-server/src/text.rs`) includes `/usr/local/share/fonts/dejavu` so this just works without any extra wiring.

### macOS HVF + ivshmem
- The doorbell only works because of the **1 ms poll timer patch** in `~/src/bsd/external/qemu-build` (`hw/misc/ivshmem-pci.c`). Upstream QEMU on macOS+HVF does not deliver MSI-X from ivshmem because GLib's main loop never polls pipe fds under HVF. Do not regress this when rebasing QEMU.
- HVF reports `ISV=0` on guest LDP/STP/LDR/STR to MMIO. Patched in `target/arm/hvf/hvf.c` to decode the instruction and emulate. Without the patch, qemu asserts.

### MSI-X on FreeBSD/aarch64/qemu+HVF — WORKS (2026-06-03; the old "broken, use polling" finding was wrong)
The Carillon bring-up (`carillon-kmod/`) root-caused this properly. **MSI-X
works** on FreeBSD/aarch64 under qemu+HVF, via the GIC ITS, doorbell
round-trip in ~0.02 s. The prior conclusion ("IORT empty, MSI-X broken")
was a misdiagnosis. The actual chain:
- **`pci_alloc_msix` ENXIO was FreeBSD's MSI blacklist, not the IORT.**
  `pci_msix_blacklisted()` blacklists MSI on "non-PCIe chipsets"; on
  arm64 the `pcie_chipset` global is unset, so the qemu host bridge
  (lacking `PCI_QUIRK_ENABLE_MSI_VM`) is blacklisted → MSI-X off for
  **every** PCI device (that's why virtio-blk showed INTx).
  **Fix:** `hw.pci.honor_msi_blacklist=0` in the guest `/boot/loader.conf`.
  With it off, virtio moves onto `its0,N` MSI-X too.
- **The IORT is fine.** Under `gic-version=3` qemu emits a 128-byte IORT:
  ITS-group node + Root-Complex node mapping RIDs `0..0xffff` → the ITS.
  (The "84 bytes" was the gic-version=2 case — no ITS, minimal IORT.)
- **Requires `gic-version=3`** (ITS only exists on GICv3). See below.
- **Driver must allocate the MSI-X table BAR** (BAR1) `RF_ACTIVE` before
  `pci_alloc_msix` — else "table_bar not mapped" → ENXIO → INTx.
- Not an HVF limit (HVF injects guest MSIs via userspace), not the ITS
  (emulated ITS works under HVF), not a qemu patch. Host→guest MSI
  delivery rides the existing ivshmem **poll-timer** (`msix_notify`).
- karythra-os used MSI-X too (it programs the GIC/MSI-X directly); it
  works there for the same reason — HVF supports it.
- Carillon's kmod is MSI-X-first with a legacy-INTx fallback (the INTx
  path needs no qemu changes either, but is slower — relies on the
  poll-timer's IntrStatus path + a host watchdog timeout).

### `-machine virt,gic-version=`
- **`gic-version=3`** (changed from 2 on 2026-06-03). Required for the GIC
  ITS, hence PCI MSI-X (Carillon doorbell + virtio all move onto MSI-X).
  GICv2 has no ITS → no PCI MSI on arm64 ACPI. v3 is a superset; nothing
  regressed.

### Carillon transport bring-up (VM, verified) — `carillon-kmod/` + `aqueduct-gpu-host --transport carillon`
The doorbell GPU transport (docs/spec/carillon.md). Host daemon +
`ivshmem-doorbell` + guest kmod; round-trip verified over MSI-X.
```sh
# host: start the Carillon IvshmemServer (creates /tmp/carillon.sock)
~/src/bsd/aqueduct-gpu-host/target/debug/aqueduct-gpu-host \
    --transport carillon --backend software \
    --carillon-sock /tmp/carillon.sock --carillon-shm /tmp/carillon.shm &
# host: boot the VM (run-vm.sh aborts if the sock is missing)
~/src/bsd/scripts/run-vm.sh --carillon
# one-time guest prereq (then reboot): MSI-X needs the blacklist off
vssh 'grep -q honor_msi_blacklist /boot/loader.conf || \
      echo hw.pci.honor_msi_blacklist=0 >> /boot/loader.conf'
# in VM: build + load the kmod, run the smoke round-trip
vssh '[ -d /usr/src/sys ] || tar -xJf /mnt/host/tarballs/src.txz -C /'
vssh 'cp -r /mnt/host/carillon-kmod /tmp/ck && cd /tmp/ck && make && \
      kldload ./carillon.ko'                       # -> /dev/carillon0, "MSI-X (1 vector)"
vssh 'cc -O -o /tmp/cs /tmp/ck/carillon_smoke.c && /tmp/cs'  # -> ROUND-TRIP OK ~0.02s
```
- The kmod is built in-VM (KBI). `make` picks up `bsd.kmod.mk` + `/usr/src/sys`.
- BAR2 mapped `VM_MEMATTR_WRITE_BACK` (cacheable) — coherent across HVF.
- "doorbell: MSI-X (1 vector)" = good; "using legacy INTx" = blacklist
  still on (loader.conf not applied / not rebooted) or BAR1 not mapped.

**Full real frame VM → host → Metal (verified).** The capstone: the daemon
in *bridge mode* (`--transport carillon --backend moltenvk`) bridges the
shared-memory byte FIFOs to a real `Session`, so a VM client drives the
whole aqueduct-gpu wire to MoltenVK/Metal.
```sh
# host: MoltenVk needs the brew lib on the dyld path
DYLD_LIBRARY_PATH=/opt/homebrew/lib \
  ~/src/bsd/aqueduct-gpu-host/target/debug/aqueduct-gpu-host \
    --transport carillon --backend moltenvk \
    --carillon-sock /tmp/carillon.sock --carillon-shm /tmp/carillon.shm &
~/src/bsd/scripts/run-vm.sh --carillon
# host: cross-compile the guest pump (release is fine on the HOST)
( cd ~/src/bsd/carillon-guest && cargo build --target aarch64-unknown-freebsd --release )
# in VM: load the kmod, run the guest (drives a real frame, reads it back)
vssh 'kldstat | grep -q carillon || (cd /tmp/ck && make && kldload ./carillon.ko)'
vssh '/mnt/host/carillon-guest/target/aarch64-unknown-freebsd/release/carillon-guest'
#   -> ROUND-TRIP OK: green triangle rendered on the host GPU, delivered to the VM
```
- `carillon-guest` reuses `GpuClient` (frame) + `carillon-transport` pumps
  (byte FIFOs); doorbells are the cdev `ioctl` RING (BAR0 doorbell) / WAIT
  (MSI-X ISR). Host reuses `Session` verbatim — no transport-specific code.
- The daemon must be in *bridge mode* (current `--transport carillon`); the
  old frame-only handler is incompatible with the guest's byte-FIFO wire.

### `needs_render` whitelist must include WM-state changes
- The server only re-renders when `process_commands` returns `needs_render = true`. The trigger list is whitelisted — historically just `CMD_RENDER` (0x0300) and `CMD_FRAME_END` (0x0304). Any command that changes WM-visible state (window create/destroy/move/title — opcodes `0x05xx`) must also flip the bit, otherwise the command processes correctly but the screen stays stale until the next user input fires another redraw. Symptom: clicked close button takes 2–3 s to actually remove the window. Fixed by widening the whitelist to `(opcode & 0xff00) == 0x0500`.

### Two-ring wait: input + completion
- `fresco_input_wait` only returns on input-ring data — it treats completion-ring wakes as "spurious" and keeps blocking. With async window events on the completion ring (CLOSE_REQUESTED etc.), apps that wait on `fresco_input_wait` add up to `ms` latency to every window event. Use `fresco_event_wait(in_out, window_out, ms)` instead — it drains both rings on each wake and returns `1` for input, `2` for window event, `0` for timeout. fresco-rs `Connection::wait_event` calls this under the hood.

### Callouts and `callout_init_mtx`
- When `callout_init_mtx(&co, &mtx, 0)` is used, the registered callback is **invoked with the mutex already held**. Calling `mtx_lock` inside the callback panics with "_mtx_lock_sleep: recursed on non-recursive mutex". Use `mtx_assert(&mtx, MA_OWNED)` to document and skip the lock. Found this the hard way during fresco.ko bring-up.

### EFI NVRAM and PCI device address shifting
- Adding/removing `-device` entries shifts PCI addresses, invalidating the saved boot entry in `edk2-arm-vars.fd`. EFI then drops into the UEFI Shell instead of booting. **Always blank `edk2-arm-vars.fd`** before launching qemu — `run-vm.sh` does this on every run. Costs ~3 s of ESP discovery per boot.

### Building inside the VM
- **Do not point `SYSDIR` at `/mnt/host/freebsd-src/usr/src/sys` for builds** — reading thousands of headers over 9p with the default `msize` (8 KiB) starves SSH and wedges qemu under HVF (CPU pegs at 100% per vcpu, no progress, ssh banner-exchange timeout). Symptom: `kgdb` warning at qemu startup ("9p: degraded performance: a reasonable high msize should be chosen on client/guest side") followed by total VM unresponsiveness.
- **Fix:** extract `tarballs/src.txz` once inside the VM to local UFS; rely on the default `SYSDIR=/usr/src`:
  ```
  vssh "tar -xJf /mnt/host/tarballs/src.txz -C /"
  vssh "cd /mnt/host/fresco-kmod && make"   # SYSDIR=/usr/src by default
  ```
  Source code stays on the host (under `~/src/bsd/fresco-kmod/`); only the bulky FreeBSD source tree is local to the VM.
- **VM resources updated 2026-04-28** — 4 vcpus / 12 GB RAM. Earlier note about lockups at 4 vcpus + 4 GB was a memory-pressure issue, not a vcpu issue: cargo compiling the Rust crates against `-smp 2 -m 4096` thrashes ssh dead. With 12 GB RAM, 4 vcpus run cleanly through cargo + kmod builds simultaneously.

### FreeBSD VM behavior
- The non-cloudinit qcow2 image **auto-grows the root partition** on first boot via `growfs` in rc. Disk is 6 GB virtual; resize the qcow2 (`qemu-img resize vm.qcow2 +20G`) and reboot to extend further.
- Default root login on serial console: **no password** (empty). After `first-boot-setup.exp` ran, the password is `fresco` and key-based SSH is set up.
- The `virtio_p9fs.ko` module auto-loads when the 9p device is probed. No manual `kldload` required.
- Mount syntax for the 9p share: `mount -t p9fs -o trans=virtio bsd_share /mnt/host`. **Not** virtiofs — this is plain virtio-9p.
- `/etc/fstab` entry uses the `late` mount option (the mount happens after networking, since p9fs needs the virtio bus).
- WITNESS is enabled in GENERIC; expect "lock order reversal" warnings at boot from in6 RA processing — harmless, not Fresco's problem.

### expect-driven serial console
- Terminal width wraps long pasted strings at 80 cols, which makes the *display* look mangled but does **not** corrupt the file when writing via heredoc. Verify the actual file before assuming a problem.

---

## 6. Ports of call — credentials and addresses

| Resource | Value |
|---|---|
| VM SSH | `localhost:2222` (key: `~/.ssh/fresco_bsd_ed25519`) |
| VM root password | `fresco` (serial console fallback only) |
| VM IP (inside qemu user-net) | `10.0.2.15` |
| Gateway / host-as-seen-from-guest | `10.0.2.2` |
| GPU server socket | `/tmp/fresco-shmem.sock` |
| GPU server shmem | `/tmp/fresco-shmem` (16 MB) |
| 9p mount tag | `bsd_share` → host `~/src/bsd` → guest `/mnt/host` |

---

## 7. Multi-window protocol design (phase B)

The single-window kiosk model doesn't scale to a real desktop. Multi-window comes in three phases (B1 → B2 → B3); we've **landed B1 foundations** as of 2026-04-26 (wire opcodes reserved, server-side `Compositor`/`Window` skeleton, stub handlers). The actual per-window dispatch refactor is the next focused piece.

### Architecture choice: option (B), per-process multiplexing on shared shmem

Considered three options for "how multiple apps share Fresco":

- **(A)** Multiple ivshmem regions, one per app. Simple but loses CAS dedup across apps (each app re-uploads its own copy of fonts/icons).
- **(B)** One ivshmem region; kernel module hands each `open()` a private ring slice; CAS staging is shared. **Chosen** — preserves dedup, single cdev path, allows multiple windows per process.
- **(C)** A `fresco-displayd` daemon that owns the cdev and proxies for clients (X11/Wayland model). Adds an in-process hop and reinvents what the protocol could do natively. Skipped.

### Isolation model

Three identity concepts, three rules:

```
client_id  — assigned by kernel module on open(); stored in cdev softc;
             never sent on the wire (server reads "this came from ring slice N
             so it's client N's"). Userspace can't forge it.

window_id  — assigned by server on CMD_CREATE_WINDOW; opaque token. Server
             tracks Window { owner: client_id }. Commands targeting a window
             must come from that window's owner — checked at dispatch.

slot_id    — chosen by client; scoped to a window. Slot 1 in window A is
             unrelated to slot 1 in window B (each Window struct has its
             own SlotTable).
```

CAS is **shared** across all clients. Sharing leaks nothing because hashes are 256 random bits — knowing a hash ≡ already having the content. Same security property git relies on. This keeps the dedup story working desktop-wide (system font, common icons uploaded once).

### Wire-protocol additions (reserved, mostly stubbed)

```
opcodes (server's command/protocol.rs):
  0x0500 CMD_CREATE_WINDOW    payload: u32 w, u32 h, u32 flags, [u8;16] short title
  0x0501 CMD_DESTROY_WINDOW   payload: u32 window_id
  0x0502 CMD_WINDOW_SET_ROOT  payload: u32 window_id, u16 slot_id
  0x0503 CMD_WINDOW_SET_TITLE payload: u32 window_id, utf8 bytes (≤ 116)
  0x0504 CMD_WINDOW_PRESENT   payload: u32 window_id (per-window FRAME_END)

completion types:
  0x10 COMP_WINDOW_CREATED          id = server-assigned window_id
  0x11 COMP_WINDOW_RESIZED          id = window_id; payload has new w/h
  0x12 COMP_WINDOW_CLOSE_REQUESTED  user clicked the title-bar ⨯
  0x13 COMP_WINDOW_FOCUS            status: 1 = focused, 0 = blurred

control register (added):
  CTRL_CURRENT_WINDOW @ offset 72 (u32)
    Guest writes the active window before issuing slot/frame ops; server
    routes them to that window's slot table. Avoids growing every slot
    command with an extra window_id field.
```

### Phasing

- **B1a** — DONE. Frontend dispatch routes `CMD_SLOT_*` / `CMD_FRAME_*` through the per-window scene+slot Arcs.
- **B1b** — DONE. Server-side titlebar + close button + drag-to-move + click-to-raise.
- **B1c** — DONE (2026-04-27). Title text with ellipsis truncation and `Theme.close_button_gutter`; per-window input routing tagging `InputEvent.target_window` (pointer = hit-tested window, keyboard = focused window); reused on the guest side via `fresco_input_t.target_window` and fresco-rs `Event::*.target_window`. See `fresco-rs/examples/event_target.rs`. Notable fix: the screen pass has color+stencil only, no depth attachment, so `depth_state` must use `compare = Always, write = false` — `Less` compare without a depth attachment was driver-defined and on macOS Metal silently discarded decorations of windows past the first. Render-list order enforces draw order.
- **B2** — DONE. Per-window FBOs (color + stencil per window) composited via `WindowOverlay`.
- **B2-kmod** — DONE (2026-04-28). Per-client ring slices: 4 slots × {32 KiB cmd ring + 32 KiB comp ring} at `0x10000 + slot*0x10000`, with per-slot R/W ptrs at ctrl `0x100 + 16*slot`. Input ring (0x1000) and CAS staging (after slot region) stay shared. Kmod allocates a slot per `open()` via `devfs_set_cdevpriv` from a 4-bit bitmap; `FRESCO_IOC_CLIENT_ID` returns it; dtor releases on close. libfresco queries the slot in `fresco_open` and routes raw_submit / completion_poll through `fresco_slot_cmd_ring_offset(slot)` and `fresco_ctrl_*_(slot)`. Server fans-in across all slots in `process_commands`, dispatches each command tagged with the originating `client_id`, and routes outbound completions (CLOSE_REQUESTED / FOCUS / RESIZED) to the *owner's* slot. Window ownership is now enforced — `handle_window_set_*` and routable slot/frame ops reject if `win.owner != current_client`. Smoke test: `fresco-rs/examples/multi_client` runs two FreeBSD processes side by side, each with its own decorated window; close clicks land on the right client only.
- **B1c-tail** — DONE. `fresco_window_event_t` (CLOSE_REQUESTED / RESIZED / FOCUS) drained via a small queue inside `fresco_t`; `fresco_event_wait` unifies input + window waits. fresco-rs surfaces `Event::CloseRequested`, `Event::WindowResized`, `Event::WindowFocus`. FOCUS emission: `Compositor::raise` returns `Option<FocusChange>`, `destroy_with_focus_shift` does the same when focus moves; `App::emit_focus_change` and `CommandFrontend::pending_completions` push blurred/focused pairs. RESIZED emission via new `CMD_WINDOW_SET_SIZE` (0x0506) → `fresco_window_set_size` / `Connection::window_set_size`; server re-runs `rebuild_window_title` so ellipsis tracks the new width, FBOs realloc via `sync_fbos`. `examples/event_target.rs` is the all-in-one smoke test.
- **Client-disconnect cleanup** — DONE (2026-04-28). New ctrl reg `CTRL_SLOTS_ALIVE_MASK` (u32 at 0x40); kmod sets the per-slot bit on `open()` and clears it in the cdevpriv dtor (which runs even on process kill). Dtor also rings the server doorbell so wakeup is immediate, not poll-bound. Server snapshots the mask each `process_commands`; any 1→0 transition triggers `cleanup_disconnected_clients`, which calls `destroy_with_focus_shift` for every window owned by the dead slot and emits the resulting FOCUS shifts to the survivors. FBO release happens automatically next frame via `sync_fbos`. Smoke: `pkill multi_client teal` while pink keeps running — teal's titlebar + content disappear immediately, pink unchanged.
- **Per-slot input rings + DMA upload** — DONE (2026-04-28). Layout reworked: shmem grew 16→32 MiB; slot stride 64→80 KiB (cmd 32 + comp 32 + input 16); per-slot ctrl regs grew 16→24 bytes (added input_w/input_r). Server routes each input event to the *target window's owner*'s slot (no broadcast ring). New per-slot 7 MiB staging region at `STAGING_BASE + slot * 0x700000`. `CMD_UPLOAD_DMA` (0x0004) finally implemented: client memcpy's blob into staging, sends one cmd with length, server reads + hashes + stores. `fresco_cas_put` switches to the DMA path for blobs > 4 KiB. A 4 MiB glyph atlas was 36 175 inline `CMD_UPLOAD_DATA` cmds — now 1. **Was the load that exposed the slot-1 starvation when running two `atrium-edit` instances; with DMA, slot 0's bring-up takes <1 ms of cmd-ring time, so slot 1 isn't starved.**
- **Real apps over multi-window** — DONE (2026-04-28). Both `atrium-edit` and `atrium-term` ported: each instance creates its own decorated window via `create_window`, parameterizes its renderer with the window's logical pixel size (so the camera fits the FBO, not the screen), filters input by `target_window`, and exits cleanly on `Event::CloseRequested`. Smoke: editor + terminal side by side as separate FreeBSD processes; keystrokes route to the focused window's owner only.
- **B3 — drag-to-resize edges** — DONE (2026-04-28). 8 px hit band straddling each window's outer perimeter; corners produce two-edge bitmasks. Press → start `ResizeAnchor` (records start cursor/pos/size); cursor-moved → compute new pos+size from delta (left/top edges shift pos too) clamped to MIN_WINDOW_W/H = 120/80; release → emit `RESIZED` completion to owner. `rebuild_window_title` runs every cursor tick during resize so the ellipsis tracks the new width live. Resize-edge hit takes priority over titlebar drag because the top edge band overlaps the titlebar's top. FBO realloc happens automatically each frame via `sync_fbos`.
- **App-side resize re-layout** — DONE (2026-04-28). Renderers in atrium-edit and atrium-term now expose `set_view_size(w, h)`. Both apps listen for `Event::WindowResized` and update their renderer's view dims. atrium-term additionally recomputes cols/rows from the new pixel size, calls `Grid::resize` (preserves content within new bounds), and `Shell::resize` (`ioctl TIOCSWINSZ`) so the kernel pty fires `SIGWINCH` to the foreground process group — verified `tput cols` reports the new column count after a drag-resize.
- **Defensive hardening** — DONE (2026-04-28).
  - **`process_commands` round-robin** with `MAX_DRAIN_PER_ROUND = 64`. Outer `'rounds` loop keeps cycling until every slot is empty within its cap, so no slot can monopolize the server even under high cmd-ring load.
  - **FBO realloc debounce** at 33 ms (~30 Hz). `WindowFbo` carries `last_resized: Instant`; `ensure_window_fbo` skips reallocation if the desired size differs from current AND elapsed < 33 ms. A cursor-driven drag stops churning Metal textures at the input event rate; the next sync pass after the drag settles picks up the final size.
  - **Per-slot ctrl reg stride** bumped 24 → 32 bytes (8 bytes reserved per slot for future per-slot state without moving every offset). Mirrored across `ivshmem.rs`, `protocol.h`, `fresco.c`.

### Security layering (orthogonal to protocol)

The protocol's `client_id` + ownership check is sufficient for graphics-layer isolation. FreeBSD's primitives complement, not replace:

- **Capsicum** — apps should `cap_enter()` after opening their cdev + data files; restricts blast radius if the app is exploited.
- **Jails** — for running untrusted apps (eventual app-store path); `/dev/fresco0` exposed via devfs.rules; protocol-level isolation unchanged.
- **MAC framework** — overkill for desktop; skip.

## 8. Deferred optimizations

Captured here so they're not forgotten. None blocks current work.

### Metal pipeline trims for 2D-only workloads
The event-driven render loop (commit on `fresco-server`, 2026-04-25) gave us ~95% of the power win — the GPU drops to DVFS idle when the scene is static. The remaining ~5% is in the Metal pipeline configuration:

- **`maxDrawables = 2`** (currently 3 in `metal_backend.rs`). One fewer drawable in flight = less memory pressure and one less framebuffer kept hot.
- **Drop the depth attachment** for the 2D path. We don't z-test anything; `depthAttachmentPixelFormat = .invalid` on the pipeline + skip `depth_texture` allocation. Saves a full-screen depth buffer (~6 MiB at 2048×1536).
- **Stencil only when used**. The renderer allocates `stencil_texture` unconditionally, but slot graph clipping is the only consumer. Track whether any item has `clip_rect` set this frame; skip the stencil attachment when none.
- **Solid-color fast path in fragment shader**. The current shader handles solid + gradient; for solid-only frames a smaller pipeline state object would shave a few µs/draw and reduce shader register pressure.
- **`preferredFramesPerSecond = 0`** on the CAMetalLayer hint — declares we're not driving at vsync rate.

Estimated additional saving: ~5–10% of the remaining baseline + a few MB of GPU memory. Worth doing once we have a real workload to measure against.

### libfresco
- **Cross-compile from macOS** — the Makefile already supports `bmake TARGET=... SYSROOT=...`, but I haven't actually exercised the cross path yet. Worth verifying before the Slint backend, since faster iteration matters more there.
- **Persistent CAS dedup cache** — currently in-process only. A small file under `~/.cache/fresco/seen-hashes` would let subsequent runs skip re-uploading common blobs.
- **Multi-threaded handle** — single fresco_t is non-thread-safe. Toolkits that emit scene mutations from multiple threads will need either a global mutex internally or one fresco_t per thread.

## 9. Recipes

### Re-do first-boot setup from scratch
```sh
cd ~/src/bsd
xz -dkf vm/vm.qcow2.xz                    # restore pristine image
rm -f vm/edk2-arm-vars.fd                 # blank EFI vars
PUBKEY=$(cat ~/.ssh/fresco_bsd_ed25519.pub)
./scripts/first-boot-setup.exp ~/src/bsd "$PUBKEY"
```

### Recover from VM filesystem corruption (full restore)
Symptoms: VM boots to single-user mode with `UNEXPECTED SOFT UPDATE
INCONSISTENCY` and `Automatic file system check failed; help!` in
`/tmp/vm.log`. Backup is `vm/vm.qcow2.xz` (pristine, pre-first-boot).
After restore, the VM has no Rust toolchain — reinstall via pkg.

```sh
# 1. Stop everything
pkill -f qemu-system-aarch64; sleep 2
pkill -f fresco-server

# 2. Restore qcow2 from xz backup (clobbers current image)
cd ~/src/bsd
rm -f vm/vm.qcow2
xz -dk vm/vm.qcow2.xz                     # produces vm/vm.qcow2

# 3. Re-run first-boot-setup (sets root password, installs ssh key)
rm -f vm/edk2-arm-vars.fd                 # blank EFI vars
PUBKEY=$(cat ~/.ssh/fresco_bsd_ed25519.pub)
./scripts/first-boot-setup.exp ~/src/bsd "$PUBKEY"
# This expect script boots, configures, and shuts down cleanly.

# 4. Start fresco-server, then VM (with --gpu for ivshmem)
> /tmp/fresco-server.log
RUST_LOG=info nohup ~/src/fresco-server/target/release/fresco-server \
    /tmp/fresco-shmem > /tmp/fresco-server.log 2>&1 &
sleep 1
> /tmp/vm.log
nohup ~/src/bsd/scripts/run-vm.sh --gpu > /tmp/vm.log 2>&1 &
until ~/src/bsd/scripts/vssh true 2>/dev/null; do sleep 3; done

# 5. Reinstall toolchain inside the VM (pristine image has no rust)
~/src/bsd/scripts/vssh "ASSUME_ALWAYS_YES=yes pkg bootstrap && \
    pkg install -y rust git curl"

# 6. Mount the host share and load the kmod (per-boot)
~/src/bsd/scripts/vssh 'mount -t p9fs -o trans=virtio bsd_share /mnt && \
    kldload /mnt/fresco-kmod/fresco.ko'
```

Notes:
- Source code, libfresco, fresco-rs, kmod live on the host — restoring
  the VM does not lose any code (only the guest's pkg cache + cargo
  build artifacts under `/mnt/fresco-rs/target`, which rebuild).
- First `cargo run` after restore will recompile fresco-rs (~30 s) plus
  rebuild libfresco (`cd /mnt/libfresco && make`) since cas.o etc. are
  source-side but their build outputs are guest-rebuilt under /mnt.
- After restore, **always run `vshutdown` to power off** instead of
  pkill — the new image hasn't yet hit corruption, and SIGKILL during
  UFS softdep flush is what caused the corruption in the first place.

### Grow the VM disk
```sh
~/src/bsd/scripts/vssh "shutdown -p now"
qemu-img resize ~/src/bsd/vm/vm.qcow2 +20G
~/src/bsd/scripts/run-vm.sh &
~/src/bsd/scripts/vssh "gpart recover vtbd0; gpart resize -i 3 vtbd0; growfs -y /"
```

### Install pkg + dev tools in the guest
```sh
~/src/bsd/scripts/vssh "ASSUME_ALWAYS_YES=yes pkg bootstrap && pkg install -y git tmux"
```

### Run the GPU server alongside the VM
```sh
# terminal 1
cd ~/src/fresco-server && RUST_LOG=info cargo run --release
# terminal 2 — wait for "Waiting for QEMU to connect" then:
~/src/bsd/scripts/run-vm.sh --gpu
```

### Run vestibulum (Pergola login screen) on screen

Phase 6 of the Pergola track. Cross-built for FreeBSD aarch64; runs
inside the venus VM against a `frescod` daemon driving real scanout.

```sh
# host: build (cached after first time)
cd ~/src/bsd/vestibulum && cargo build --release --target aarch64-unknown-freebsd

# in-VM: start frescod (the daemon, NOT frescod-vulkan-smoke)
~/src/bsd/scripts/vssh "FRESCOD_BUNDLES_ROOT=/mnt/host/bundles \
    nohup /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod \
        > /tmp/frescod.log 2>&1 &"

# in-VM: launch vestibulum (defaults FRESCO_SOCK=/tmp/frescod.sock)
~/src/bsd/scripts/vssh "/mnt/host/vestibulum/target/aarch64-unknown-freebsd/release/vestibulum"
```

Login form appears in the QEMU display window. Click into the
username field, type, click into password, type, press Sign in.
`vestibulum` prints the captured username + status, sets
`done=true`, and exits cleanly. Esc aborts.

Real local-auth integration (pam_local equivalent) is a separate
follow-up; the binary today demonstrates the toolkit + wire path
end-to-end.

### Deploy Portcullis end-to-end in a fresh VM

After `Re-do first-boot setup from scratch` above, get from "fresh
qcow2 with sshd" to "user logs in and lands in a session jail with
working app launches" in ~5 minutes.

```sh
VSSH=~/src/bsd/scripts/vssh
PORT=~/src/bsd/portcullis

# 1. Cross-compile the Portcullis binaries (host; ~7s clean, ~2s incremental).
cd $PORT
cargo build --target aarch64-unknown-freebsd --release \
    -p portcullis-cli -p portcullisd -p atrium-session

# 2. Install zsh in the guest (atrium-login execs into zsh).
$VSSH 'ASSUME_ALWAYS_YES=yes pkg install -y zsh'

# 3. Install binaries (atrium-session MUST be setuid root — login(1)
#    runs the user's shell as the user, so atrium-login has no privs;
#    atrium-session escalates to do the mount + jail -c work).
$VSSH "install -m 0755  /mnt/portcullis/target/aarch64-unknown-freebsd/release/portcullis      /usr/local/bin/portcullis &&
       install -m 0755  /mnt/portcullis/target/aarch64-unknown-freebsd/release/portcullisd     /usr/local/bin/portcullisd &&
       install -m 04755 /mnt/portcullis/target/aarch64-unknown-freebsd/release/atrium-session  /usr/local/bin/atrium-session &&
       install -m 0755  /mnt/portcullis/atrium-session/install/atrium-login                    /usr/local/bin/atrium-login &&
       install -m 0755  /mnt/portcullis/portcullisd/install/portcullisd                        /usr/local/etc/rc.d/portcullisd"

# 4. Add a test user with atrium-login as their shell.
$VSSH "pw useradd atrium -m -G wheel -s /usr/local/bin/atrium-login -w no
       mkdir -p /home/atrium/.ssh
       cp /root/.ssh/authorized_keys /home/atrium/.ssh/authorized_keys
       chown -R atrium:atrium /home/atrium/.ssh
       chmod 700 /home/atrium/.ssh"

# 5. Enable + start portcullisd. Pre-creates /atrium/sockets/ and
#    /var/lib/atrium/{apps,overlays,jails,sessions} on first start.
#    Daemon is multi-tenant: socket world-connectable (mode 0666),
#    per-user authorization happens inside via getpeereid(2).
$VSSH 'sysrc portcullisd_enable=YES
       service portcullisd start
       ls -la /atrium/sockets/portcullis.sock'

# 6. Verify: ssh in as atrium, run a smoke command.
ssh -i ~/.ssh/fresco_bsd_ed25519 -p 2222 atrium@localhost <<'EOF'
hostname    # should print "atrium-session" (the session jail's hostname)
id          # uid=1001(atrium) — matches the host uid (atrium-session
            # composes its in-jail passwd to mirror host uids so the
            # daemon's getpeereid lookup resolves correctly)
ls /apps    # the apps registry
exit
EOF

# 7. Install a test app + launch via session jail to verify the
#    SCM_RIGHTS pty handoff path:
$VSSH 'mkdir -p /var/lib/atrium/apps/test.hello
       cp /rescue/sh /var/lib/atrium/apps/test.hello/sh
       cat > /var/lib/atrium/apps/test.hello/atrium.toml <<MANIFEST
[app]
id = "test.hello"
name = "Hello"
version = "0.1.0"
entry = "runme"

[capabilities]
network = "none"
MANIFEST
       cat > /var/lib/atrium/apps/test.hello/runme <<RUNME
#!/sh
echo "hello from app jail!"
echo "pid=\$\$"
RUNME
       chmod +x /var/lib/atrium/apps/test.hello/runme
       /usr/local/bin/portcullis link-apps'

# Then ssh in as atrium and run:
#     /apps/test.hello/test.hello
# Expected output:
#     hello from app jail!
#     pid=<small number>
```

Known papered-over issues (see commits 4e52f9d, plus
multi-tenancy fixes in the next commit):
- /usr/local/etc/zshrc isn't installed yet; default zsh prompt fine.
- App jails inherit no /etc, no /sbin — apps must bring their own
  binaries (or a curated etc); intentional sealed-app behaviour.

### jaild — the privileged jail broker (bring-up, first run 2026-06-14)

`jaild` (`atrium-jaild`) is the Portcullis TCB: the sole caller of
`jail_set`/`pdfork`/`execve`, validating every request against a static policy.
Cross-compiles like the rest (`cargo build --release --target
aarch64-unknown-freebsd -p jaild`). First in-VM bring-up:

```sh
vssh "cp /mnt/host/portcullis/target/aarch64-unknown-freebsd/release/atrium-jaild /root/ &&
      mkdir -p /etc/atrium /var/run/atrium /var/log/atrium &&
      cp /mnt/host/etc/jaild.policy.toml /etc/atrium/"
vssh "/root/atrium-jaild check-policy --policy /etc/atrium/jaild.policy.toml"   # ok: schema_version=1 services=4
vssh "/root/atrium-jaild serve --policy /etc/atrium/jaild.policy.toml \
        --socket /var/run/atrium/jaild.sock --state /var/run/atrium/jaild.state.toml &"
# socket is srw------- root (mode 0600); jaild refuses any non-root peer.
```

Drive it with the test client `atrium-portcullisd-jclient <socket> <cmd>` —
`ping` / `create <name> <path>` / `exec <name> <path> <bin> [args]` / `remove
<jid>`. **jaild enforces the policy strictly** — every layer of the allow-list:

- *name* must match an allowed prefix (`atrium-`/`app-`/`system-`/`user-`);
- *jail path* must be in `mount_sources` (or `/`, special-cased for smoke tests);
- *exec binary* must be under `exec_paths.allowed_prefixes`
  (`/usr/local/bin/atrium-`, `/usr/local/share/atrium/apps/`, …).

A full **jailed exec** (the `pdfork`+`jail_set`+`execve` path) — proven in-VM:

```sh
# put a binary under the allowed exec prefix
vssh "mkdir -p /usr/local/share/atrium/apps/app-hello && cp /rescue/sh /usr/local/share/atrium/apps/app-hello/sh"
# launch it confined; jaild forks, jails, execs, and returns a procdesc fd
vssh "/root/atrium-portcullisd-jclient /var/run/atrium/jaild.sock \
        exec app-hello / /usr/local/share/atrium/apps/app-hello/sh -c 'echo JAILED_OK > /tmp/jp.txt'"
# -> {kind: jail_created, pid: N, procdesc_attached: true}; the process runs (marker written).
```

The rc.d service is `atrium_jaild_enable=YES` (`portcullis/jaild/etc/atrium-jaild`).

> **Lyra/Choragus integration (the remaining chain):** the audio capability
> grants (`audio` / `microphone` / `audio_monitor`, all now in the Portcullis
> manifest schema) flow manifest → portcullisd (user approval) → the per-user
> grant store. For Choragus to read a *real* grant (vs its hand-written
> `choragus.grants`), the Lyra app must be **launched by Portcullis** in a jail
> with a distinct uid, so choragusd's `getpeereid(2)` resolves the uid → the
> app's grant. That app-launch path is the next integration step.

### FreeBSD source fork (for kernel patches)

We maintain a downstream fork of FreeBSD src for kernel-side patches
we need across Atrium subsystems (vfs tweaks, scheduler hints, GPU
ABI evolution, etc.). The fork lives at
`atrium-os/freebsd-src-fork` on GitHub (created via GitHub's
server-side fork API — instant, no upload of the full FreeBSD tree).

```sh
# One-time clone (partial — only fetches blobs on demand; ~3GB
# checked out, less if you don't materialise everything).
git clone --filter=blob:none https://github.com/freebsd/freebsd-src.git \
    ~/src/freebsd-src
cd ~/src/freebsd-src

# Two remotes:
#   upstream = freebsd/freebsd-src (read-only; sync from this)
#   atrium   = atrium-os/freebsd-src-fork (push our branches here)
git remote rename origin upstream
git remote add atrium https://github.com/atrium-os/freebsd-src-fork.git

# Working branch tracks our cumulative patches.
git checkout -b atrium/main upstream/main
git push -u atrium atrium/main
```

**Workflow for a new kernel patch:**

```sh
cd ~/src/freebsd-src
git checkout -b atrium/<topic> atrium/main
# ... edit, build (KERNCONF=GENERIC make -j8 buildkernel from /usr/src
# inside the VM, NOT host — kernel build needs FreeBSD make + libs)
git commit
git push atrium atrium/<topic>
```

**Periodic upstream sync:**

```sh
cd ~/src/freebsd-src
git fetch upstream
git checkout atrium/main
git rebase upstream/main      # or merge, our call per patch series
git push atrium atrium/main --force-with-lease
```

**Why server-side fork instead of full re-push:** pushing 1.5GB of
git pack from a partial clone requires fetching all missing blobs
from upstream first (slow). The GitHub server-side fork (`gh repo
fork freebsd/freebsd-src --org atrium-os`) creates the fork on
GitHub's backend in seconds with no upload, and we just push our
diffs as branches.

(There's an empty `atrium-os/freebsd-src` placeholder repo lingering
from a setup misstep. Delete via the web UI when convenient — needs
`delete_repo` scope which `gh auth refresh` can grant.)

### Live kernel debug — kgdb workflow

For interactive kernel-side debugging (single-step, breakpoints,
struct inspection), the VM exposes a gdb stub via QEMU's `-s` flag
and a cross-gdb on the host attaches to it.

One-time setup on the host:
```sh
brew install aarch64-elf-gdb
```

Start the VM with the stub enabled:
```sh
~/src/bsd/scripts/run-vm.sh --kgdb
```

This adds `-s` to QEMU (gdb stub on `127.0.0.1:1234`).  The script
also auto-sends `cont` to QMP because QEMU 10.x + HVF + `-s` starts
the guest paused (undocumented, differs from KVM); without the
auto-cont the boot wouldn't proceed.  Boot is otherwise identical
to a normal `run-vm.sh` invocation; the stub is dormant until gdb
attaches.

In a second terminal, attach gdb:
```sh
~/src/bsd/scripts/kgdb-attach.sh                  # default: LAMINAR-DEV
~/src/bsd/scripts/kgdb-attach.sh GENERIC          # another kernconf
~/src/bsd/scripts/kgdb-attach.sh /full/path.full  # explicit symbol file
KGDB_REFRESH=1 ~/src/bsd/scripts/kgdb-attach.sh   # force re-copy after rebuild
```

The script fetches the matching `kernel.full` (debug symbols) from
the VM's `/usr/obj` to the host via the 9p share, then runs
`aarch64-elf-gdb` against it with `target remote 127.0.0.1:1234`.
First attach copies ~90 MB; subsequent attaches reuse the cached
copy in `vm/kgdb-symbols/`.

When gdb pauses, ALL vCPUs halt.  When you `c` or `detach`, the
guest resumes.

Useful commands inside gdb:
```
(gdb) info threads                       # vCPUs as gdb threads
(gdb) bt                                 # backtrace
(gdb) b sched_laminar_setcpu             # set breakpoint
(gdb) b /usr/src/sys/kern/sched_laminar.c:342
(gdb) c                                  # continue
(gdb) p curthread->td_critnest           # FreeBSD pcpu-aware deref
(gdb) si / ni                            # single-step instr / next
(gdb) info reg                           # ARM64 registers
(gdb) detach                             # resume guest, leave gdb
(gdb) Ctrl-C                             # interrupt running guest
```

Note: gdb's `curthread` lookup works only after a breakpoint hits
in kernel code (per-CPU resolution).  Source file references like
`sched_laminar.c:905` show as "No such file or directory" warnings
because gdb is on the host and source lives in the VM; the
breakpoint line numbers still work.  Add `directory
~/src/bsd/freebsd-src/usr/src/sys` inside gdb to fetch source via
the host clone if needed.

Common diagnostic recipes:

1. **Witness panic / critnest leak**: set a breakpoint at the
   function suspected of leaking, on entry inspect
   `curthread->td_critnest`, single-step the dance, watch the
   counter on each spinlock_enter/exit / mtx acquire.

2. **Sched-state inspection at a panic**:
   ```
   (gdb) target remote 127.0.0.1:1234
   (gdb) bt
   (gdb) up                              # walk up the stack
   (gdb) info locals
   (gdb) p ((struct laminar_tdq *)PCPU_GET(sched))->ltdq_load
   ```

3. **Conditional breakpoint on specific thread**:
   ```
   (gdb) b sched_laminar_add if td->td_proc->p_pid == 1234
   ```

Trade-offs vs ddb:
- **gdb pro**: source-level, conditional breakpoints, hardware
  watchpoints (HVF passes them through), persistent across boots.
- **gdb con**: pauses ALL CPUs on attach; less aware of kernel
  thread state than the in-tree `kgdb` post-mortem tool.
- **ddb pro**: in-kernel, knows kernel structs natively, can run
  while other CPUs continue.  Already enabled in this kernel.
- **ddb con**: text-only, no breakpoints, scripting via DDB
  scripts is awkward.

Recommended split: ddb for post-panic forensics ("show alllocks",
"show pcpu", "ps", "bt"), gdb for stepping through a live bug.

### crash dumps for post-mortem analysis

For panics that leave the kernel in DDB, `dump` writes a minidump
to the configured dump device.  Then `kgdb /boot/<kernel>/kernel
/var/crash/vmcore.N` reads it back symbolically.

Set up dump device once (kernel config already enables
`debug.minidump=1`):
```sh
~/src/bsd/scripts/vssh "swapinfo | head -3"
~/src/bsd/scripts/vssh "dumpon /dev/vtbd0p2"   # adjust device
~/src/bsd/scripts/vssh "sysrc dumpdev=/dev/vtbd0p2"
```

After a panic at DDB prompt: `dump`, then `reboot`.  On next boot
savecore extracts to `/var/crash/`.  Then in the VM:
```sh
kgdb /boot/laminar/kernel /var/crash/vmcore.0
(kgdb) bt
(kgdb) thread N
(kgdb) print td_critnest
```

---

## 10. Lyra audio (in-VM)

The `lyra/` crate is **lyrad** (the deadline-broker audio daemon) + **lyra-effect**
(an effect node as a separate, jailable process). Design doc:
`docs/spec/atrium-lyra-architecture.md`; the deterministic model lives in the
gpusim repo. Audio reaches the host through `-device intel-hda` → coreaudio (live)
or `-audiodev wav` (headless capture, lossy — use `LYRA_DUMP` for ground truth).

```sh
# Host: boot with audio (coreaudio = you hear it; AUDIO_BACKEND=wav = capture).
scripts/run-vm.sh --audio

# Host: cross-compile lyrad + lyra-effect (NEVER --release in the VM; see §4).
cd ~/src/bsd/lyra && cargo build --release --target aarch64-unknown-freebsd

# VM: snd_hda attaches under HVF with no MSI fuss. Bit-perfect is the sysctl
# that matters (hw.snd.maxautovchans doesn't exist on this build).
vssh "kldload snd_hda 2>/dev/null; sysctl dev.pcm.0.bitperfect=1; ls /dev/dsp*"

# VM: lyrad + lyra-effect must be SIBLINGS (lyrad finds lyra-effect via
# current_exe().with_file_name). Copy both to /root.
vssh "cp /mnt/host/lyra/target/aarch64-unknown-freebsd/release/lyrad \
         /mnt/host/lyra/target/aarch64-unknown-freebsd/release/lyra-effect /root/"
```

**First sound** — a bit-perfect 440 Hz tone (the hardware clock advances exactly
48000 frames/sec consumed):

```sh
vssh "/root/lyrad --tone"
```

**Glitch-free-under-load** — the deadline-lane thesis on real hardware. The lane
is gated behind a tunable (default off); enable it once per boot:

```sh
vssh "sysctl kern.sched.deadline_enable=1"   # required for any lane/sponsor/adopt
```

`--feed <secs> <spinners> [lane]`; with the lane, codec underruns stay 0 under N
CPU hogs (without, hundreds). A/B:

```sh
vssh "/root/lyrad --feed 5 16"        # NO lane  -> play_underruns in the hundreds
vssh "/root/lyrad --feed 5 16 lane"   # deadline lane -> 0 (clean window)
```

**The full L3 path — a Capsicum-confined C plugin processes live audio, and
survives its own crash.** Compile the reference C node (the `lyra_node.h` ABI)
in-VM, then route the tone through it. `LYRA_TREMOLO=0` makes the *built-in* path
passthrough, so any modulation is unambiguously the C plugin; `LYRA_CAPSICUM=1`
makes the node `cap_enter()`-self-confine (opt-in Capsicum — NOT a forced FreeBSD
/Portcullis jail, which would be imposed from outside and is a separate, still-TO-
BUILD layer); `LYRA_DUMP` writes the exact emitted i16 stereo bytes for offline
verification.

```sh
# VM: build the plugin (C builds in the VM are fine; cc targets FreeBSD natively).
vssh "cc -shared -fPIC -O2 -I/mnt/host/lyra/include \
         /mnt/host/lyra/plugins/tremolo.c -o /root/tremolo.so -lm"

# VM: tone -> confined C tremolo node -> OSS. Expect, in order on stderr:
#   hosting C node 'tremolo' / confined (Capsicum capability mode) / play_underruns=0
vssh "cd /root && LYRA_PLUGIN=/root/tremolo.so LYRA_CAPSICUM=1 LYRA_TREMOLO=0 \
         LYRA_DUMP=/root/dump.raw /root/lyrad --effect 3"

# K-b adoption (needs deadline_enable=1): LYRA_ADOPT=1 makes lyrad self-sponsor a
# graph entity and the effect ADOPT it -- the confined C node runs on lyrad's CBS
# budget, charged back (a heavy plugin throttles the client, not lyrad). Adds two
# stderr lines: "graph entity sponsored (pid tid)" + "adopted client entity ...
# charged to its budget". All four L3 mechanisms compose in one run:
vssh "cd /root && LYRA_ADOPT=1 LYRA_PLUGIN=/root/tremolo.so LYRA_CAPSICUM=1 \
         LYRA_TREMOLO=0 /root/lyrad --effect 3"

# Crash-isolation: the 2nd positional after --effect is the crash frame. The
# confined node aborts mid-stream; lyrad detects the gone child and BYPASSES to
# dry — audio continues, play_underruns stays 0.
vssh "cd /root && LYRA_PLUGIN=/root/tremolo.so LYRA_CAPSICUM=1 LYRA_TREMOLO=0 \
         /root/lyrad --effect 3 48000"

# Verify the dump (ground truth, independent of the lossy wav capture): pull it
# over 9p and check carrier + tremolo envelope.
vssh "cp /root/dump.raw /mnt/host/scratch/lyra-dump.raw"
# host: 144000 frames/3s, carrier ~440 Hz, envelope depth ~0.50, LFO ~5 Hz
#       == tremolo.so's defaults -> the C node ran.
```

> **Cleanup after a run:** lyrad's `--effect` doesn't drain like `--tone`. If you
> hear stale-buffer crackle, write a little silence and let the device close
> cleanly: `vssh "dd if=/dev/zero of=/dev/dsp0 bs=4096 count=8"`. Never `kill -9`
> lyrad and leave the HDA buffer mid-write.

**Choragus (`choragusd`) — the policy/session layer + the full control/data
plane.** lyrad is the RT engine (mixer); choragusd decides routing/ducking/
volume/privacy and drives lyrad over `lyra-protocol` (Aqueduct class 5). Cross-
compile all three crates (`lyra`, `choragus`, `lyra-protocol`) from the host.

```sh
# lyrad as a DYNAMIC MIXER + control plane: a control socket (Ctl frames from
# choragusd) and a data socket (<ctl>.data, fd-passes anon-shm rings to sources).
vssh "cp /mnt/host/lyra/target/aarch64-unknown-freebsd/release/{lyrad,lyra-feed} \
         /mnt/host/choragus/target/aarch64-unknown-freebsd/release/choragusd /root/"
vssh "/root/lyrad --control /tmp/lyrad.ctl 8 &"     # plays the mix to OSS for 8s

# choragusd as the SESSION DAEMON: apps register, policy drives lyrad. --grants
# points at the capability store (etc/choragus.grants.example is the format).
vssh "/root/choragusd --daemon /tmp/choragus.sock /tmp/lyrad.ctl --grants /root/choragus.grants &"

# an app: register under a role -> choragusd opens the slot + applies policy.
# A second app registering as 'comms' ducks the media app automatically.
vssh "/root/choragusd --app /tmp/choragus.sock media 6 --id org.atrium.player &"
vssh "/root/choragusd --app /tmp/choragus.sock comms 2 --id org.atrium.meet"

# privacy: a recorder requesting the system monitor is DENIED unless its app-id is
# granted audio_monitor in the store (default-deny).
vssh "/root/choragusd --app /tmp/choragus.sock media 1 monitor --id org.evil.spyware"  # DENIED

# data plane: a source feeds REAL audio into a stream's fd-passed ring. lyra-feed
# connects to lyrad's DATA socket, receives the ring fd (SCM_RIGHTS), writes a
# tone; lyrad mixes it (vs the synth fallback). id 0 = the first stream.
vssh "/root/lyra-feed 0 550 7 /tmp/lyrad.ctl.data &"   # 550 Hz into stream 0
```

> **Verify the mix spectrally** (the wav capture is lossy): `LYRA_DUMP=/root/x.raw`
> on lyrad writes the exact mixed i16 stereo; pull it over 9p and Goertzel at the
> fed/synth frequencies. Ducking shows as a −18 dB drop on the targeted stream
> only. **Don't `kill -9` `lyra-feed`** — the named-ring path leaks shm; the
> fd-passed path (anon shm) is race-free but still prefer clean exit.

---

## 11. Dev VM rebuild — ZFS root (2026-05-09)

The dev VM was rebuilt onto **ZFS root** to eliminate UFS-softdep-flush
corruption from mid-write `kill -9`. With ZFS, the on-disk format is
always atomically consistent — every `shutdown -p` is clean, every
`kill -9` recovers without fsck. This was the recurring source of "the
qcow2 is dirty, restore from baseline + reinstall pkgs" pain.

### What's in the new baseline

`vm/vm.qcow2` (post-rebuild snapshot, ~1.8 GiB compressed):

- **Disk**: 32 GiB qcow2 (vs. 6 GiB old UFS baseline) — room for full
  mesa + vulkan + dev tooling without space anxiety.
- **Layout**: GPT, 200 MiB EFI (FAT32, FreeBSD `loader.efi` as
  `BOOTAA64.EFI`) + remainder as `freebsd-zfs`.
- **zpool**: `zroot` with `compress=lz4`, `atime=off`. Datasets:
  `ROOT/default` (mounted /), `usr/{home,obj,ports,src}`, `var/{audit,
  crash,log,mail,tmp}`, `tmp`. ARC capped at 2 GiB via
  `vfs.zfs.arc_max=2147483648` in `/etc/sysctl.conf`.
- **Hostname**: `atrium-dev`.
- **SSH**: host keys generated, `~/.ssh/authorized_keys` carries the
  dev `fresco_bsd_ed25519` pubkey, `PermitRootLogin without-password`.
- **`/etc/fstab`**: `bsd_share /mnt/host p9fs rw,trans=virtio,noauto`
  — opt-in 9p host share (`mount /mnt/host` to attach).
- **`/boot/loader.conf`**:
  - `zfs_load="YES"` + `vfs.root.mountfrom="zfs:zroot/ROOT/default"`
  - `p9fs_load="YES"` + `virtio_p9fs_load="YES"`
  - `atrium_virtio_gpu_load="YES"` — preload kmod so its probe wins
    against stock vtgpu at boot (BUS_PROBE_VENDOR > BUS_PROBE_DEFAULT).
    Runtime kldload after vtgpu has already attached doesn't displace
    it; preloading is the clean fix.
  - The kmod itself lives at `/boot/modules/atrium_virtio_gpu.ko`.
- **Pkgs pre-installed**: `vulkan-loader`, `vulkan-tools`,
  `mesa-libs`, `mesa-dri`, `rsync`, `pkg`. The lavapipe (CPU
  rasterizer) Vulkan ICD is at `/usr/local/lib/libvulkan_lvp.so` /
  `/usr/local/share/vulkan/icd.d/lvp_icd.aarch64.json`.

### What's NOT in the baseline (intentionally)

- **atrium-mesa / venus userspace ICD** — needs to be built from
  source per host. Not included; frescod falls back to lavapipe via
  the standard Vulkan loader.
- **`/usr/src` kernel source** — re-extract on demand:
  ```sh
  vssh "tar -xJf /mnt/host/tarballs/src.txz -C /"
  ```
- **Big build artifacts** (`/usr/obj`, mesa build trees) — kept out
  so the baseline stays small.

### Building the rebuild

The full procedure is captured in `scripts/build-zfs-root.sh`. To
recreate from scratch (e.g. after a base.txz refresh):

1. Boot the existing VM with `vm-zfs.qcow2` (blank, 32 GiB) attached
   as a second disk. Use `qemu-img create -f qcow2 vm-zfs.qcow2 32G`.
2. Inside VM, mount 9p share + build atrium-kmod (KBI-bound so must
   build in-VM):
   ```sh
   mount -t p9fs -o trans=virtio bsd_share /mnt/host
   tar -xJf /mnt/host/tarballs/src.txz -C / # only if /usr/src missing
   cd /mnt/host/atrium-kmod && make
   ```
3. Run `/mnt/host/scripts/build-zfs-root.sh` — partitions vtbd0,
   creates zpool, extracts base.txz + kernel.txz, configures /etc and
   /boot, sets up SSH, generates host keys, exports pool.
4. `shutdown -p now`.
5. On host: `mv vm/vm.qcow2 vm/vm-old.qcow2 && mv vm/vm-zfs.qcow2
   vm/vm.qcow2`.
6. Boot — should come up as `atrium-dev` with `/dev/atrium-{gpu,
   display}0` already present.
7. Snapshot: `xz -k -T 0 -2 vm/vm.qcow2` produces a new
   `vm/vm.qcow2.xz` baseline.

### Known follow-up (not blocking dev work)

The kmod's BLOB scanout path uses `BLOB_MEM_HOST3D` + `USE_MAPPABLE`
(which requires a virtio-gpu context, which we satisfy with a
kmod-internal venus-capset scanout context). End-to-end scanout is
gated on `CTX_CREATE` getting a fence response back from the host;
right now the host's `virgl_render_server` worker spawns
(`atrium-scanout` visible in `ps`) but the fence completion never
reaches the guest, so the kmod's `req_resp` times out. atrium-mesa
userspace exhibits the same failure (vulkaninfo falls back to
lavapipe). This is a host-side fence-routing issue; tracked
separately.

### Building atrium-mesa (venus userspace)

The Atrium fork of Mesa lives at `~/src/bsd/external/mesa` (sibling to `~/src/bsd`).
Source is rsynced into the VM rather than 9p-mounted because the
build opens many files concurrently and 9p's FD pressure leads to
"too many open files" mid-copy.

```sh
# Host: rsync source to VM
rsync -a --exclude='.git/' --exclude='build*/' --exclude='__pycache__' \
    -e 'ssh -i ~/.ssh/fresco_bsd_ed25519 -p 2222 \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null' \
    ~/src/bsd/external/mesa/ root@localhost:/root/mesa/

# VM: install build deps
vssh "pkg install -y meson ninja py311-mako py311-pyyaml pkgconf \
    wayland wayland-protocols vulkan-headers expat zstd llvm"

# VM: apply atrium patches + complete the meson seam wiring
# (Hunks 1+2 of the patch may need manual fixup if upstream has
# rearranged the seams — see ATRIUM-FORK.md in the mesa repo. The
# short version is: top-level meson.build needs `with_atrium =
# get_option('atrium')`; src/meson.build needs `if with_atrium
# subdir('atrium') endif` placed before src/virtio/vulkan; and
# src/virtio/vulkan/meson.build needs an `if with_atrium ... endif`
# block adding files_atrium_venus + inc_atrium + -DHAVE_ATRIUM.)
vssh "cd /root/mesa && patch -p1 < .atrium-patches/0001-vn_renderer-add-atrium-backend-dispatch.patch"

# VM: configure + build (no wayland WSI — FreeBSD lacks
# CLOCK_MONOTONIC_RAW which mesa's wayland WSI uses)
vssh "cd /root/mesa && meson setup build-atrium \
    -Datrium=true \
    -Dvulkan-drivers=virtio \
    -Dgallium-drivers= \
    -Dgles1=disabled -Dgles2=disabled -Dopengl=false -Dglx=disabled \
    -Degl=disabled -Dglvnd=false \
    -Dvulkan-layers= \
    -Dplatforms= \
    -Dprefix=/usr/local \
    -Dvulkan-icd-dir=share/vulkan/icd.d \
    --buildtype=release"
vssh "cd /root/mesa && ninja -C build-atrium"

# VM: install (lands /usr/local/lib/libvulkan_virtio.so +
# /usr/local/share/vulkan/icd.d/virtio_icd.aarch64.json)
vssh "cd /root/mesa && ninja -C build-atrium install"
```

`scripts/run-vm.sh` exposes `~/src/bsd/external/mesa` as a second 9p share
(`mesa_share` mount tag) for inspecting the build dir from host —
but **don't** `cp -r` from `/mnt/mesa` inside the VM; 9p's FD pool
exhausts mid-copy and locks up the guest. Always rsync over SSH.

## 12. Tier-2 software Vulkan renderer (atrium-spv-*)

The tier-2 path runs arbitrary SPIR-V shaders on the CPU when no
native GPU path exists. AOT-on-first-use: SPIR-V → IR → object →
`ld` → `.so`, cached on disk, then dlopen'd and called. **Not a
JIT** — static translation + execution, per the locked design
decision (`docs/spec/tier2-renderer.md` §D1).

### Crate map

| Crate | Role |
|-------|------|
| `atrium-spv-ir` | Types-only SSA IR. `Op` enum, `Type`, `Block`/`BlockKind`, `Module`. No logic. |
| `atrium-spv-pcmap` | Binary PC-map sidecar format (`ATRPCMAP` magic) for crash triage. |
| `atrium-spv-frontend` | SPIR-V → IR. Structured-CFG recovery, constant hoist, offset table (constraint A2). |
| `atrium-spv-backend-cranelift` | IR → object via Cranelift. The graceful-degradation fallback + differential twin. |
| `atrium-spv-backend-bespoke` | IR → ARM64 object via `pptk-codegen-arm64::asm`. The perf path. |
| `atrium-spv-tests` | Interpreter (the F1-clean oracle — walks SPIR-V directly, zero shared frontend code) + `assert_shader_agrees`. |
| `atrium-spv-differential` | `CraneliftRunner` + `BespokeRunner`; three-way harness. |
| `atrium-spv-loader` | `ShaderCache`: SHA-256-keyed `.so` cache + dlopen. Spawns `atrium-spv-compile` on miss. |
| `atrium-spv-compile` | Standalone compile binary the loader shells out to. Tries the bespoke ARM64 backend first, falls back to Cranelift on `Unsupported` (spec §2 production order); emits `<hash>.so` + `<hash>.pcmap` and a `"backend":"bespoke"\|"cranelift"` metrics line. |

### Three-tier model

1. **bespoke** — ARM64 codegen, the production hot path. Linear-scan
   register allocator, single-pass ISel.
2. **Cranelift** — fallback for IR shapes bespoke returns
   `Unsupported` for (matrices, images/samplers, atomics,
   derivatives). Also the differential twin.
3. **interpreter** — the test oracle. Reads SPIR-V via `rspirv::dr`
   directly; **constraint F1**: zero shared code with the production
   frontend, so a frontend bug makes two-of-three runners agree with
   each other and *disagree* with the interpreter.

### Bespoke backend opcode coverage (as of v17)

Full common fragment-shader surface:
- scalar + vec FP arithmetic (FAdd/FSub/FMul/FDiv/FNeg)
- integer arithmetic (IAdd/ISub/IMul/SDiv/UDiv/INeg) + bitwise
  (And/Or/Xor/Not) + shifts (Shl/LShr/AShr)
- all 16 comparisons (6 FOrd + 10 int signed/unsigned)
- int↔float conversions (ConvertSToF/UToF/FToS/FToU)
- structured CFG: if/else, loops, switch — Branch / BranchCond /
  Switch with post-emit relocation patching
- Phi at merge blocks (per-edge `fmov_s` moves)
- Select (scalar + vec, `fcsel_s`)
- Dot, VectorTimesScalar, VectorShuffle, CompositeConstruct,
  CompositeExtract
- AccessChain + Load through push-constant / uniform pointers

Register pools (all caller-saved, no prologue):
- V16..V31 — f32 scalar linear-scan pool
- W10..W12 — Bool pool (fcmp/icmp results)
- W13..W17 — integer scalar pool

ABI (AAPCS64 fragment split): `X0`=in_varyings `X1`=uniforms
`X2`=push_constants `X3`=samples_mask `X4`=out_color `X5`=out_depth;
`S0..S3`=frag_coord.

### Running the tests

```bash
# Each crate independently (host, fast — bespoke needs cc + dlopen)
for c in atrium-spv-{ir,pcmap,frontend,tests,backend-cranelift,\
backend-bespoke,loader,compile,differential}; do
  (cd "$c" && cargo test)
done

# The three-way differential harness is the key signal:
(cd atrium-spv-differential && cargo test)   # 23 tests, all 3 runners
```

`atrium-spv-compile` must be built before `atrium-spv-loader` /
`aqueduct-gpu-host` integration tests run — they shell out to the
binary by path (`../atrium-spv-compile/target/debug/`).

### Host integration

`aqueduct-gpu-host` wraps the loader: `Tier2Registry` (SHA-256 →
compiled `LoadedShader`) + `Tier2Backend` (a `Backend` trait impl
that runs `atrium_fs_main` per pixel during `submit_frame`).
`Session::handle_pipeline_create` auto-binds a graphics pipeline's
fragment shader to its `tier2_id` so the standard wire sequence
(SHADER_UPLOAD → PIPELINE_CREATE → SUBMIT_FRAME) drives Tier-2
execution with no out-of-band wiring.

### pptk dependency

The bespoke backend's ARM64 encoder is `pptk-codegen-arm64` from the
**adjacent PPTK repo** (`~/src/pptk`), not vendored. Tier-2 work
added f32-scalar encoders there (`fadd_s`/`fsub_s`/`fmul_s`/`fdiv_s`,
`fcmp_s`, `fmov_s`, `fcsel_s`, `scvtf_s_from_w`, `fcvtzs_w_from_s`,
etc.) — companion commits live in the pptk repo, not here.

### In-VM verification of the bespoke backend

The unit + three-way differential suites all run on the macOS
**host** (Mach-O, `Aarch64Darwin`). The bespoke backend's actual
production target is FreeBSD/aarch64 (ELF, `Aarch64FreeBSD`) — a
different object format and the genuine AAPCS64 environment. To
close that gap there are four focused in-VM scripts, each
verifying a different layer of the production chain, plus a
wrapper that runs all four with a single pass/fail summary:

```bash
# Dev VM must be up (scripts/run-vm.sh) + reachable on :2222.
sh atrium-spv-backend-bespoke/verify/run-all-in-vm.sh   # the lot
```

| script                     | layer verified on-target                                  |
|----------------------------|-----------------------------------------------------------|
| `run-in-vm.sh`             | bespoke backend object emission + AAPCS64 codegen         |
| `run-e2e-in-vm.sh`         | the production `atrium-spv-compile` binary + backend selection |
| `run-loader-e2e-in-vm.sh`  | `atrium-spv-loader`'s `ShaderCache` (hash, handshake, disk cache, dlopen) |
| `run-pcmap-e2e-in-vm.sh`   | the `.pcmap` sidecar round-trip through the loader's parser |

`run-all-in-vm.sh` sequences them and tallies the result; a
sub-script failure doesn't abort the run, so one break can't
mask the others. Each sub-script is also runnable on its own
— the rest of this section documents them individually.

```bash
sh atrium-spv-backend-bespoke/verify/run-in-vm.sh
```

What it does:
1. Host: `cargo run --example emit_freebsd_obj` cross-emits a
   constant-colour fragment shader as a FreeBSD/aarch64 **ELF**
   object (`compile(module, Target::Aarch64FreeBSD)`).
2. `scp`s the object + `verify/harness.c` into the VM.
3. VM: `cc -shared` links the object → `.so`; `cc` builds the
   dlopen harness; harness calls `atrium_fs_main` per the
   fragment ABI and prints the RGBA.
4. Compares VM output against the host-side expected value.

The script covers eleven shaders:
* **const** — constant-colour store: exercises the ELF object
  format, the exported symbol, and the Store path.
* **ifelse** — `if (push_const.scale < 0.5) red else blue`:
  AccessChain + Load + FOrdLt + BranchCond + multi-block CFG +
  branch relocation. Driven with two inputs (`0.2` → then,
  `0.8` → else) so **both `b.cond` outcomes** run on-target.
* **loop** — `acc = 0; for (i = 0; i < n; i++) acc += i`,
  output white iff `acc == n*(n-1)/2`. Exercises the i32 Phi /
  int Load / integer linear-scan pool, the back-edge `Branch`
  + its relocation, the callee-saved-register prologue
  (`stp`/`ldp` of X19..X28 — loops outrun the 5 caller-saved
  int slots), and the loop-liveness machinery. Driven with
  `n=5` (sum 10 → white) and `n=4` (sum 6 → black).
* **arith** — `out = (n*2+1)*0.125`, no control flow: IMul,
  IAdd, ConvertSToF (`scvtf`), FMul on the W-reg integer pool
  + the int→float path. The on-target twin of the host
  `three_way_int_arith_and_convert` differential test. The
  0.125 scale is a negative power of two so every result is
  exact in f32 and prints identically under Rust's `{}` and
  the harness's C `%g`. Driven with `n=3` and `n=1`.
* **bitwise** — nibble / xor / or extraction from an i32
  push-const: `AShr`, `BitAnd`, `BitXor`, `BitOr`, then
  `ConvertSToF` + `FDiv`, all power-of-two normalised. The
  on-target twin of the host `three_way_bitwise_and_shift`
  differential test. Driven with `n=0x53` and `n=0xC1`.
* **vecarith** — `(a + b) * (a - b)` over two constant
  `vec4`s: per-lane `FAdd` / `FSub` / `FMul` and the V-reg
  vector lane allocator across three chained vec4 ops. The
  on-target twin of the host `three_way_vec_arithmetic`
  differential test.
* **switch** — `switch (n) { 0:red 1:green 2:blue
  default:white }` from an i32 push-const: `OpSwitch`
  multi-target jump codegen + a 5-block CFG with four
  branch relocations converging on a merge block. Driven
  with `n=0` (a case), `n=2` (last case), `n=7` (default)
  so the case path and the fall-through both run. On-target
  twin of the host `three_way_switch_*` differential tests.
* **phi** — `if (scale < 0.5) chosen = 1.0 else chosen =
  0.25`, where `chosen` is produced by an `OpPhi` at the
  merge block (the then/else blocks are empty — they only
  carry the branch). Phi convergence in a *non-loop* CFG:
  each arm's value must land in the Phi's register on the
  correct predecessor edge. Driven with `0.2` (then) and
  `0.8` (else). On-target twin of the host `three_way_phi_*`
  differential tests.
* **shuffle** — `OpVectorShuffle` (`va.bgra`): ARM64
  lane-shuffle codegen, moving lanes between V-register
  positions. On-target twin of host
  `three_way_vector_shuffle_bgra`.
* **cextract** — `OpCompositeExtract` + `OpCompositeConstruct`:
  single-lane extraction and recombination in a new order.
  On-target twin of host `three_way_composite_extract`.
* **dot** — `OpDot` + `OpVectorTimesScalar` + per-lane
  `FMul` + `CompositeConstruct` threading the dot result
  through a lane. On-target twin of host
  `three_way_dot_and_composite`.

Verified 2026-05-14 on FreeBSD 16.0-CURRENT arm64:
```
PASS  const        -> [0.125 0.375 0.625 1]
PASS  ifelse then  -> [1 0 0 1]
PASS  ifelse else  -> [0 0 1 1]
PASS  loop n=5     -> [1 1 1 1]
PASS  loop n=4     -> [0 0 0 1]
PASS  arith n=3    -> [0.875 0.875 0.875 1]
PASS  arith n=1    -> [0.375 0.375 0.375 1]
PASS  bitwise 0x53 -> [0.3125 0.1875 0.97265625 0.32421875]
PASS  bitwise 0xC1 -> [0.75 0.0625 0.41796875 0.81640625]
PASS  vecarith     -> [0.1875 0.75 2 3]
PASS  switch n=0   -> [1 0 0 1]
PASS  switch n=2   -> [0 0 1 1]
PASS  switch n=7   -> [1 1 1 1]
PASS  phi then     -> [1 1 1 1]
PASS  phi else     -> [0.25 0.25 0.25 1]
PASS  shuffle      -> [0.75 0.5 0.25 1]
PASS  cextract     -> [1 0.25 0.75 0.5]
PASS  dot          -> [0.25 0.125 0.0625 0.4375]
```
In-VM coverage now mirrors the host three-way differential
corpus: every common-shader opcode + CFG path the host
suite exercises is also verified on the production
FreeBSD/aarch64 target.
The harness prints with `%.9g`, not the default `%g` —
`%g` caps at 6 significant digits, so an exact value like
`0.97265625` would print `0.972656` and spuriously diverge
from the host's Rust `{}` (shortest round-trip) expected
string.
The bespoke ELF + AAPCS64 codegen — object format, `cc -shared`
link, `dlopen`/`dlsym`, the fragment ABI, the conditional-
branch path, **and counted loops** (Phi back-edges,
callee-saved prologue, loop-liveness extension) — all run
correctly on the production target, not just the macOS host.

> Loop liveness was a multi-bug fix landed 2026-05-14:
> flat-order linear-scan can't see a loop body re-executes,
> so (1) Phi-arm sources used on a back-edge had live ranges
> that ended before their own definition, and (2)
> loop-invariant values used only in the loop header expired
> mid-loop. Both surfaced *only* on-target — the macOS-host
> differential harness had been silently skipping the
> bespoke runner because i32 Phis returned Unsupported.
> Lesson: the in-VM check is not optional; the host suite
> alone masked a whole class of bugs.

### End-to-end in-VM verification (production compile chain)

`run-in-vm.sh` emits the object file *host-side* and only
ships the `.o`. That proves the bespoke backend's codegen,
but not the rest of the production chain. `run-e2e-in-vm.sh`
closes that gap — it drives the real `atrium-spv-compile`
binary, cross-built for FreeBSD/aarch64 and run **inside the
VM**, on a raw SPIR-V input:

```bash
sh atrium-spv-backend-bespoke/verify/run-e2e-in-vm.sh
```

```text
SPIR-V file  ->  atrium-spv-compile (in VM)
             ->  frontend + bespoke backend + `cc -shared`
             ->  <hash>.so + <hash>.pcmap
             ->  dlopen + atrium_fs_main  ->  pixels
```

So this exercises the production binary's argument handling,
the **bespoke-first / Cranelift-fallback selection**, the
in-VM linker invocation, and the `.pcmap` sidecar — the
whole chain the daemon's `Tier2Backend` shells out to, on
the real target. Each row asserts *which* backend the
binary's G7 metrics line reports, so a silent regression
to the Cranelift fallback (or a broken bespoke path) fails
the row — not just diverging pixels.

The corpus includes a deliberate **fallback probe**: the
`unordcmp` shader uses `OpFUnordLessThan`, which the
bespoke backend has no arm for. `atrium-spv-compile` must
fall back to Cranelift for it — the row asserts
`backend=cranelift` *and* correct pixels, exercising the
production fallback path on-target.

`emit_freebsd_obj` gained a `spirv` mode (`emit_freebsd_obj
<out.spv> spirv <kind> [args]`) that writes the raw SPIR-V
module instead of a compiled object, so the e2e script
reuses the same shader builders as `run-in-vm.sh`.

Verified 2026-05-14 on FreeBSD 16.0-CURRENT arm64 — seven
shaders on the bespoke path, two on the Cranelift fallback,
all pixel-correct:
```
PASS  const        -> [0.125 0.375 0.625 1]   (backend=bespoke)
PASS  ifelse then  -> [1 0 0 1]               (backend=bespoke)
PASS  ifelse else  -> [0 0 1 1]               (backend=bespoke)
PASS  loop n=5     -> [1 1 1 1]               (backend=bespoke)
PASS  loop n=4     -> [0 0 0 1]               (backend=bespoke)
PASS  switch n=1   -> [0 1 0 1]               (backend=bespoke)
PASS  switch n=9   -> [1 1 1 1]               (backend=bespoke)
PASS  unordcmp lt  -> [1 0 0 1]               (backend=cranelift)
PASS  unordcmp ge  -> [0 0 1 1]               (backend=cranelift)
```

### Loader-cache in-VM verification

`run-e2e-in-vm.sh` drives the `atrium-spv-compile` binary
directly. `run-loader-e2e-in-vm.sh` goes one layer up — it
exercises `atrium-spv-loader`'s `ShaderCache`, the
daemon-side component that hashes SPIR-V, looks up the
on-disk cache, spawns `atrium-spv-compile` only on a miss,
dlopens the result, and hands back typed entry-point
pointers.

```bash
sh atrium-spv-backend-bespoke/verify/run-loader-e2e-in-vm.sh
```

The driver (`atrium-spv-loader/examples/loader_e2e_driver.rs`,
cross-built + run in the VM) loads each shader **twice**:

1. **cold** — a real `compile_binary`; the cache miss
   spawns it, writing `<hash>.so` + `<hash>.pcmap`.
2. **warm** — a *fresh* `ShaderCache` (empty in-memory
   state) whose `compile_binary` is a deliberately bogus
   path. A miss here would try to spawn it and fail — so
   reaching a `LoadedShader` proves the load was served
   purely from the on-disk `.so`, with **no re-compile**.

It then calls the disk-cache-loaded entry point and prints
the RGBA. A PASS therefore proves, on the production
target: loader hashing, the loader↔compile-binary
handshake, the path-versioned disk cache, the dlopen/dlsym
path, and the AAPCS64 entry-point call.

Verified 2026-05-14 on FreeBSD 16.0-CURRENT arm64 (the
`unordcmp` row also confirms a Cranelift-fallback shader
round-trips through the loader cache):
```
PASS  const        -> [0.125 0.375 0.625 1]
PASS  ifelse then  -> [1 0 0 1]
PASS  loop n=5     -> [1 1 1 1]
PASS  switch n=2   -> [0 0 1 1]
PASS  unordcmp lt  -> [1 0 0 1]
```

### .pcmap sidecar round-trip in-VM

`atrium-spv-compile` writes a `<hash>.pcmap` sidecar next
to every `<hash>.so` — the host-PC → SPIR-V-offset map the
daemon's crash handler uses for source attribution. The
loader reads it back through `PcMap::from_bytes`, storing
`Some(PcMap)` on the `LoadedShader`.
`run-pcmap-e2e-in-vm.sh` drives `loader_e2e_driver` in
`--check-pcmap` mode on the target — it loads each shader
through the loader, then prints the parsed sidecar state
instead of running the shader.

```bash
sh atrium-spv-backend-bespoke/verify/run-pcmap-e2e-in-vm.sh
```

The driver also exercises `PcMap::lookup` — the host-PC →
SPIR-V-offset binary search the crash handler actually
calls — on a mid-function PC, and reports `mid_lookup=Z`
(`none` if the search fails).

Verified 2026-05-14 on FreeBSD 16.0-CURRENT arm64 — the
sidecar the production compile binary emits round-trips
cleanly through the loader's parser on-target, with real
per-instruction entries, and `lookup` resolves a
mid-function PC to its source offset:
```
PASS  const     -> entries=7  first_spirv=0   last_host=100 mid_lookup=0
PASS  ifelse    -> entries=15 first_spirv=0   last_host=156 mid_lookup=456
PASS  loop      -> entries=21 first_spirv=0   last_host=160 mid_lookup=504
PASS  switch    -> entries=19 first_spirv=0   last_host=220 mid_lookup=516
PASS  unordcmp  -> entries=1  first_spirv=436 last_host=0   mid_lookup=436
```

> The bespoke backend originally emitted only a single
> stub pcmap entry (`push(0, first_offset)`); commit
> `0328e45` made `emit_function` record a real
> `(host_offset, spirv_offset)` pair per lowered IR
> instruction, so crash triage can now attribute a fault
> to its source SPIR-V offset. (`unordcmp` is the
> Cranelift-fallback shader — Cranelift's own pcmap is
> separate and sparser.)

### bespoke-vs-Cranelift benchmark

`run-bench.sh` runs the `bench_driver` example
(`atrium-spv-compile/examples/bench_driver.rs`) over a
12-shader corpus, host and in-VM, measuring the two axes
that decide whether the bespoke backend earns its
complexity:

* **compile** — ns for `backend::compile()` alone
  (frontend + `cc` link excluded). The cache-miss latency
  budget.
* **run** — ns per `atrium_fs_main` call of the linked +
  dlopen'd shader. The per-draw hot path (spec §8.1).

```bash
sh atrium-spv-backend-bespoke/verify/run-bench.sh
```

First run, 2026-05-14, in-VM (FreeBSD/aarch64) — `Nx` is
the bespoke speedup (>1 = bespoke faster). The `heavy`
shader (added second run) is the only one whose runtime
clears call overhead:

| shader   | compile | run   | run ns (besp / clif) |
|----------|---------|-------|----------------------|
| const    | 2.30x   | 1.22x | tiny — call overhead |
| ifelse   | 5.15x   | 0.69x | tiny — call overhead |
| loop     | 3.20x   | 0.95x | 35 / 33              |
| switch   | 3.65x   | 0.74x | tiny — call overhead |
| arith    | 2.95x   | 1.02x | tiny — call overhead |
| vecarith | 2.60x   | 0.80x | tiny — call overhead |
| dot      | 2.44x   | 0.67x | tiny — call overhead |
| shuffle  | 3.23x   | 1.05x | tiny — call overhead |
| cextract | 2.78x   | 1.07x | tiny — call overhead |
| phi      | 3.21x   | 0.82x | tiny — call overhead |
| **heavy**| **4.67x** | **0.78x** | **1290 / 1002**  |

> **Reading the result.** The bespoke backend wins
> **compile time decisively — 2.3–5.2× faster** than
> Cranelift across the board (single-pass ISel vs
> Cranelift's full optimisation pipeline). On **runtime it
> does not yet win**: on the `heavy` shader — a 512-iter
> loop, ~1.3 µs/call, the only shader with real signal —
> bespoke is **0.78× (28% slower)** than Cranelift. Host
> and in-VM agree to within 1%, so this is real, not
> noise.

#### Why bespoke loses the `heavy` loop — disasm analysis

bespoke loop body = **17 instructions**, Cranelift = **10**.
The seven-instruction gap, per iteration:

| issue | bespoke emits | Cranelift emits | cost |
|-------|---------------|-----------------|------|
| compare→branch not fused | `cmp; cset w,lt; cmp w,#0; b.ne` | `cmp; b.lt` | **2** |
| Phi-move not coalesced | `fadd s25,…; fmov s16,s25` (×2 accumulators) | `fadd s17,…` straight into the Phi reg | **2** |
| induction-var copy not coalesced | `add w15,w13,w14; mov w13,w15` | `add w3,w3,#1` in place | **1** |
| branch-to-next-block not elided | `b 0xa8` (target is the next insn) | — | **1** |
| no immediate `add` | `mov w14,#1` (hoisted) + `add …,w14` | `add …,#0x1` | ~0/iter |

This **does not contradict the architecture** — spec §8.1
says bespoke perf "ramps in shader-by-shader" — but it
turns the headline justification into a concrete backlog.

#### Bespoke backend optimisation backlog (data-driven)

In priority order (impact × inverse-risk):

1. **Compare→branch fusion.** ✅ **DONE** (`7d11ff4`). When
   an `IComparison` / `FComparison` result feeds *only* a
   `BranchConditional`, skip materialising the i32 bool
   (constraint B4) — emit `cmp` + a fused `b.<cond>`. 2
   insns/iter. Correct, but the heavy-shader runtime
   barely moved (~3%): the removed `cset`/`cmp` are cheap
   integer ops a superscalar core runs in parallel with
   the FP work — they were never on the critical path.
2. **Branch-to-next-block elision.** ✅ **DONE**
   (`53388b6`). A block's unconditional branch to the
   block emitted immediately after it is dropped — control
   falls through. Applied to `Op::Branch`, the BranchCond
   false edge, and the Switch default jump.
3. **Immediate-form add/sub.** ✅ **DONE** (`162a633`). A
   small integer constant (imm12 range) used only as an
   add/sub operand rides in the instruction's immediate
   field instead of occupying a W-reg — `add w,w,#1`. Both
   code-size and register-pressure hygiene; the freed
   W-reg eases #5's problem. (The `fmov s,#imm` / `movi`
   float-immediate forms are deferred — they need new pptk
   encoders and only touch the run-once prologue.)
4. **Phi / copy coalescing.** ✅ **DONE** (`8c53315`). When
   a Phi arm's value is produced by a scalar binary op,
   that op writes straight into the Phi register and the
   `fmov`/`mov` is elided. These moves sat *on* the
   loop-carried dependency chain — this was the real
   runtime lever, as the disasm analysis predicted.
5. **Register pressure relief — V8..V15 extension.** ✅
   **DONE** (pptk `b513d15`, atrium `7a25c72`). The
   linear-scan RA aborted once f32 pressure exceeded the
   16 caller-saved V16..V31 — a four-accumulator loop
   failed to compile outright. Route A: added V8..V15 as
   an overflow tier, mirroring the W19..W28 callee-saved-
   integer mechanism (prologue `stp d`, epilogue `ldp d`,
   via new pptk encoders). f32 ceiling 16 → 24, covering
   essentially all real fragment shaders. The new `heavy4`
   shader (four accumulators, five Phis) is the proof:
   it overflowed before, now compiles — verified bit-exact
   in the three-way differential (`three_way_heavy4`) and
   executing correctly on the FreeBSD/aarch64 target.
   *True unbounded spill/reload* remains future work, but
   is now a rare-case completeness item, not a blocker.

#### Result after opts #1–#5

`heavy` shader runtime, bespoke vs Cranelift (`Nx`, >1 =
bespoke faster):

| stage                  | in-VM ns/call (besp / clif) | ratio |
|------------------------|-----------------------------|-------|
| baseline               | 1290 / 1002                 | 0.78x |
| + compare→branch (#1)  | 1250 / 1002                 | 0.77x |
| + Phi coalescing (#4)  | 1113 / 1097                 | 0.99x |
| + branch elision (#2) + immediate add (#3) | 1087 / 1090     | **1.00x** |

The bespoke backend is now at **exact runtime parity**
with Cranelift on the heavy loop (1.00x, host agrees)
**while still compiling ~4.7× faster** — it now earns its
complexity. Opt #4 was the real runtime lever (the
loop-carried Phi `fmov`s); #1/#2/#3 are code-size and
register-pressure hygiene; #5 lifts the f32 register
ceiling 16 → 24 so register-pressured shaders compile on
the bespoke path instead of falling back to Cranelift.

All five verified together: the full in-VM verification
suite (codegen / compile chain / loader cache / pcmap),
the `run-bench.sh` corpus, and **25/25 three-way
differential tests** — including `heavy` and the
register-pressure `heavy4` — pass bit-exact against the
interpreter oracle. The bespoke backend's headline
justification — steady-state hand-written-ARM64 perf — is
now demonstrated, not just asserted.

#### Raising the bar: bespoke vs `clang -O2`

Cranelift did its job — it dragged the bespoke backend up
to fast-tier-JIT parity. But Cranelift isn't a
*native-speed* bar; it trades codegen quality for compile
speed. So the bench's runtime oracle moved to a
hand-written C reference compiled with `clang -O2`
(`verify/native/heavy.c`; `bench_driver --native`). Two
builds: `-ffp-contract=off` (same arithmetic as the
backends) and plain `-O2` (the true ceiling, FMA-fused —
results no longer bit-identical).

Three native references now (`verify/native/{heavy,heavy4,
loop}.c`), in-VM (FreeBSD/aarch64), ns/call:

| shader  | bespoke | Cranelift | clang-O2 strict | clang-O2 fma |
|---------|---------|-----------|-----------------|--------------|
| heavy   | 979     | 989       | 1513            | 1024         |
| heavy4  | 515     | 509       | 1073            | 1017         |
| loop    | 34      | 33        | **0.93**        | 0.98         |

* **`heavy`** (2-accumulator scalar-FP dependency chain) —
  bespoke ≈ Cranelift ≈ clang. No gap. clang's strict
  build is even *slower* (1513) because its auto-vectoriser
  mis-packed the serial loop into 2-lane SIMD with
  per-iteration cross-lane shuffles.
* **`heavy4`** (4 accumulators, more ILP) — bespoke ≈
  Cranelift (515 vs 509), both well ahead of clang. *This
  used to be the weak spot* — bespoke was 1.22× slower
  than Cranelift (658 vs 536). Disasm showed the two
  loops structurally identical except the loop-carried
  Phi move: bespoke emitted `fmov s` (FP-pipe latency),
  Cranelift `mov vd.16b` (the ORR alias — rename-
  eliminated). Switching the Phi-move emission to the
  vector form (`22eea7c`) took heavy4 658 → 515 ns and
  restored parity. The "register pressure" hunch was a
  near-miss: the cost was Phi-move latency, which heavy4's
  denser loop merely exposed.
* **`loop`** (counted integer sum) — bespoke ≈ Cranelift,
  both **~35× slower than clang**. Not a codegen-quality
  gap: `clang -O2` strength-reduces `for i in 0..n: acc+=i`
  to the closed form `n*(n-1)/2` (O(1)); neither fast-tier
  backend does loop-idiom recognition. A whole *category*
  of optimisation, shared-absent in both.

So the honest picture: bespoke is at the `clang -O2` bar
for straight scalar-FP work, at *both* shapes measured
(2- and 4-accumulator loops), and matches Cranelift on
all three. Two large gaps to a full optimiser remain,
both *shared-absent in Cranelift too* — not bespoke
deficiencies: `loop` is loop-idiom recognition (~35×),
and `heavyvec` is NEON vectorisation of vec FP binops
(0.62× native — see the vec-Phi arc and the NEON
vectoriser scoping below; vec Phi landed and unblocked
this measurement). Still unmeasured: texture/sampler-
heavy shaders.

#### Next arc — scoped: vec Phi in the bespoke backend

The vector-heavy loop shape — a `vec4` accumulator carried
across a loop — is the one we most want to measure against
`clang -O2` (clang SIMD-packs it; bespoke lane-walks) but
*can't*: the bespoke Phi pre-pass rejects `Type::Vec2/3/4`
(`"Phi of type … not supported"`), so any vec-carrying
loop falls back to Cranelift wholesale. That's both a
measurement blocker and a real coverage gap — `vec4`
accumulation (lighting, blending) is a common real-shader
pattern.

**Design.** The backend already represents a vector as a
list of per-lane scalar `Value`s (`vectors: HashMap<…,
Vec<Value>>`), each lane registered in `scalars`. So a
vec Phi decomposes cleanly into *N per-lane scalar Phis*:
allocate N never-expired V-regs in the Phi pre-pass,
register them as `vectors[phi_result]`, build per-lane
phi-move lists. The just-landed `mov_v_16b` phi-move and
the opt-#4 coalescing both extend per-lane with no new
ideas. `PhiDest` either grows a `Vec(Vec<Vreg>)` variant
or the pre-pass emits N `PhiDest::Float` entries.

**Risk — register pressure.** A `vec4` Phi reserves 4
never-expired V-regs; two `vec4` accumulators + temps will
exhaust even the 24-reg post-#5 file. There is no
spilling, so an over-pressured vec loop returns
`Unsupported` → Cranelift fallback. That's acceptable
graceful degradation, but it caps how heavy a vec shader
the bespoke path can take.

**Phasing.**
1. **✅ done.** vec Phi in the pre-pass + per-lane phi-move
   emission (no coalescing). `PhiDest` grew a
   `Vec(Vec<Vreg>)` variant; the Phi pre-pass decomposes a
   `Type::Vec2/3/4` result into N never-expired V-regs +
   synthetic per-lane `Value`s registered in `vectors`;
   the Branch-terminator phi-move emits N per-lane
   `mov_v_16b`. Gate met: `three_way_heavyvec` (interp +
   Cranelift + bespoke bit-exact, n=16 vec4 loop) + the
   in-VM `heavyvec` shader, both green on FreeBSD aarch64.
   One bug found+fixed during bring-up: a vector phi arm
   (e.g. a `ConstVec` initial value on the entry edge)
   doesn't own a reg itself — its per-lane scalar values
   do — so `compute_last_use_flat` must extend each
   *lane's* live range to the predecessor terminator, not
   just the composite's. Without it the linear-scan
   recycled a lane's constant reg before the entry→header
   phi-move; HashMap-order-dependent, so it passed on the
   macOS host and failed only in-VM (lane 2 of the vec4
   came out as `bias.x` instead of `v0.z`).
2. **✅ done.** extend opt-#4 coalescing to vec-Phi lanes.
   `emit_fp_binop_poly` now takes the whole `PhiDest`: a
   `PhiDest::Vec` coalesce target makes each lane of an
   FP binop write straight into the Phi's per-lane reg,
   and the back-edge per-lane phi-moves drop out. The
   pre-pass `op_ok` accepts a vec FP binop (`FAdd/FSub/
   FMul/FDiv`) as a coalescable producer; the
   case1/case2 dominance + single-use + no-read-between
   guards are unchanged (they already key on the vec
   value id). Per-lane in-place is hazard-free: the Phi
   lane regs are never-expired and distinct, and
   `make_inst` reads both operands before writing the
   dest. Disasm-confirmed on `heavyvec`: the loop body
   is 9 instructions (4 `fmul` + 4 `fadd` + `add` +
   back-edge `b`) with zero in-loop `mov v.16b`; the
   only vector moves left are the entry-edge inits.
   Gate: full differential 26/26 + in-VM 19/19 still
   bit-exact.
3. **✅ done.** added the `heavyvec` bench shader +
   `native/heavyvec.c` C ref. This is the first shader
   where native clearly beats both fast-tier backends at
   runtime: in-VM `heavyvec` runs bespoke **348.9 ns**,
   Cranelift 348.9 ns, `clang -O2` **216.2 ns** — bespoke
   is **0.62×** native (native ~1.6× faster). The gap is
   exactly the expected one: clang SIMD-packs the four
   independent lanes into one `fmul.4s` + one `fadd.4s`
   per iteration, where both fast-tier backends lane-walk
   four scalar `fmul` + four scalar `fadd`. vec-Phi got
   the bespoke path correct + call-overhead-free and at
   parity with Cranelift; closing the gap to native means
   a NEON vectoriser (recognise that a vec FP binop's
   lanes are independent and emit the `.4s` form) — its
   own future arc, shared with Cranelift, not a bespoke
   deficiency.

#### Next arc — scoped: NEON vectoriser for vec FP binops

`heavyvec` (phase 3 above) is the standing measured gap:
bespoke/Cranelift **0.62×** `clang -O2`, purely because
both lane-walk a `vec4` FP binop as four scalar `.s` ops
where clang emits one `.4s`. Closing it means keeping a
`vecN` in a *single* Q-register and emitting packed NEON.

**Why it's a real arc, not a peephole.** The bespoke
backend models a vector as `vectors: HashMap<ValueId,
Vec<Value>>` — four *independent* scalar `Value`s, each
in its own V-reg. `.4s` ops act on lanes 0..3 of *one*
Q-reg; you cannot `.4s` across four separate registers.
So this needs a parallel *packed* representation, not a
late instruction-swap.

**Missing toolchain piece.** `pptk-codegen-arm64::asm`
has scalar `.s`/`.d` FP ops, `mov_v_16b`, `ins_v_s/_d`
lane inserts, `fmov_w_from_s`/`_s_from_w` — but *no*
`.4s` arithmetic (`fmul/fadd/fsub/fdiv v.4s`), no `ld1`,
no `dup`. Those encodings are phase 0, in pptk.

**Design — hybrid representation (recommended).** Add
`packed: HashMap<ValueId, asm::Vreg>` alongside the
per-lane `vectors`. A value is *packed-eligible* when
every producer and consumer on its chain is pack-friendly
(vec FP binop, vec Phi, whole-vector Store, vec×scalar
broadcast via `dup`); anything lane-addressed (shuffle,
CompositeExtract/Construct, Dot) forces the per-lane
form. A pre-pass classifies each vec `ValueId`; packed
values get one Q-reg + `.4s` ops, and a packed↔lanes
bridge (`mov s, v.s[i]` / `ins v.s[i]`) materialises at
the boundary when a packed value meets a lane-only
consumer. vec Phi and the phase-2 coalescing both carry
over almost verbatim — a packed Phi is *one* never-
expired Q-reg, the coalesced binop writes it in place.

**Phasing.**
0. **✅ done** (`d132773`). pptk: `.4s` FP arith
   (`fadd/fsub/fmul/fdiv_v_4s`) + `dup_v_4s_from_s0/_w`
   encoders, one spot-test each. Constant vec4s build via
   per-lane `ins` (already in pptk); no `ld1`-from-pool
   needed yet.
1. **✅ done.** Packed representation + classifier. Added
   `packed: HashMap<ValueId, Vreg>` alongside the per-lane
   `vectors`; a vec4 ValueId lives in exactly one. The
   classifier seeds `disqualified` from every vec4 touched
   by a non-pack-friendly op (Shuffle/Extract/Dot/Phi/
   Select/Composite/Insert/AccessChain) then fixed-point
   propagates across vec×vec FP-binop cliques. Pack-
   friendly producers: a *pure* `ConstVec` (all lanes are
   genuine `ConstFloat`s — a `CompositeConstruct` of
   computed/extracted scalars stays per-lane, since
   assembling its aliased element regs into a fresh Q-reg
   can clobber a still-live source) and vec×vec FP binops;
   pack-friendly sink: a whole-vector `Store`. No bridge
   needed yet — the classifier is whole-component, so a
   packed value is *only* ever consumed by packed ops.
   `emit_fp_binop_poly` grew a packed path (one `.4s`
   instruction); `ConstVec` builds the Q-reg via per-lane
   `ins`; `Store` of a packed value is one `str q`.
   Disasm-confirmed on `vecarith`: 12 scalar ops + 4 lane
   stores → `fadd.4s` + `fsub.4s` + `fmul.4s` + `str q`.
   Gate met: full differential 26/26 (caught + fixed the
   `CompositeConstruct`-ConstVec aliasing bug — `cextract`
   now correctly stays per-lane) + in-VM 19/19, `vecarith`
   packed and bit-exact on FreeBSD aarch64.
2. **✅ done.** Packed vec Phi + coalescing carry-over.
   `PhiDest` grew a `Packed(Vreg)` variant — one never-
   expired Q-reg, allocated by the Phi pre-pass when the
   classifier marks the Phi result packed. The classifier
   itself now treats a vec4 Phi as a *propagating* clique
   (result + every arm value share a fate), so the
   heavyvec loop's `{v0, v_phi, scaled, v_next, store}`
   resolves to a single packed component. The Phi-move
   emission gained a `PhiDest::Packed` arm (one
   `mov v.16b`); `emit_fp_binop_poly`'s packed path
   honours a `Some(PhiDest::Packed(q))` coalesce target
   (the vec analogue of opt #4). Disasm-confirmed on
   `heavyvec`: the loop body collapsed from 9 scalar/
   per-lane instructions to **4** —
   `fmul v.4s` + `fadd v.4s` + `add` + back-edge `b`,
   with the `fadd` coalesced straight into the packed Phi
   Q-reg. Gate met: `three_way_heavyvec` + full
   differential 26/26 + in-VM 19/19 bit-exact on FreeBSD
   aarch64.
3. **✅ done.** Re-bench. In-VM `heavyvec`:
   bespoke **274 ns** (was 349 ns — **21% faster**),
   Cranelift 331 ns, `clang -O2` 217 ns. The gap to
   native moved from **0.62× → 0.79×** (1.61× behind →
   1.27× behind). Bespoke now also *beats* Cranelift on
   this shape (274 vs 331 ns) — Cranelift still
   lane-walks. The residual ~57 ns gap to native is
   per-call setup (12 `ins` ops materialising 3 ConstVecs
   in the prologue, one entry-edge `mov v.16b`); the loop
   body itself is at parity with the `clang -O2`
   `.4s`-unfused form. A literal-pool path for ConstVec
   prologue would chase it, but that's a separate small
   arc — closing the per-iter NEON gap was the goal here.

**Risk — FMA.** `clang -O2` (non-`-ffp-contract=off`)
fuses `mul`+`add` into `fmla.4s`; that changes results
bit-for-bit (skips the intermediate rounding), so an
`fmla`-emitting bespoke would break the bit-exact
differential. Keep `.4s` arithmetic *unfused* for
correctness parity with the interpreter oracle; the
`native/fma` bench column stays the separate, explicitly
non-bit-exact ceiling.

#### Next arc — scoped: ConstVec literal pool

The NEON-vectoriser arc left a residual ~57 ns gap on
`heavyvec` (bespoke 274 ns vs `clang -O2` 217 ns). The
loop body is at parity; the gap is per-call prologue. A
pack-friendly `ConstVec` currently materialises via 4
`movz/movk w` + 4 `fmov s,w` + 4 `ins v.s[k]` = 12 insts
per vec4, plus extra inst slots for non-trivial bit
patterns. `heavyvec` has 3 such ConstVecs → ~36 prologue
instructions for vector constants alone. `clang -O2`
emits the 16-byte literal once and reads it with a single
`ldr q, [pc-rel]` per use — the obvious cheap path.

**Design.** Append a per-function literal pool *inside*
the same `.text` symbol, after the `ret`. PC-relative
`LDR Qt, label` (imm19, ±1 MiB) reaches it in one
instruction with zero relocs — fits the JIT-emit
constraints already met by the rest of the code. The
pool stays in the same mapped region (`PROT_EXEC`-readable
on FreeBSD/Linux ARM64 and inside macOS `MAP_JIT`), and
the symbol size grows to cover it, so dlopen / mmap of
the blob loads it intact. ConstVec dedup is a free hash;
identical vec4s share a slot.

**Risk — none material.** `ret` precedes the pool so it's
never executed; imm19 ±1 MiB dwarfs realistic function
sizes; rodata-in-.text is conventional for tiny PIC
helpers. The pool only applies to *pack-friendly*
`ConstVec`s (all-`ConstFloat` lanes); a per-lane ConstVec
still wants the existing per-S-reg path.

**Phasing.**
0. **✅ done** (pptk `a234963`). `ldr_q_literal(rt, imm19)`
   encoder + three spot tests (LDR (literal) SIMD&FP,
   opc=10, V=1; zero/positive/negative imm19).
1. **✅ done.** backend: pool pre-pass + dead-ConstFloat
   pre-pass + per-function pool emit/patch. A
   pack-friendly ConstVec lowers to one `ldr q,
   [pc-rel]` placeholder; the pool is 16-byte-aligned
   and laid out after the final `ret` (never executed);
   placeholders are patched with the resolved imm19.
   Dedup is free — identical vec4s share a slot. A
   second pre-pass marks ConstFloats whose every consumer
   is a pool ConstVec as *dead* and skips their emission
   entirely (otherwise the prologue would still drag 4
   `movz/movk + fmov s,w` ops per pool ConstVec for the
   now-unread lane S-regs). Disasm-confirmed on
   `heavyvec`: 3 ConstVecs collapse from ~36 prologue
   ops to 3 `ldr q`. Gate met: full differential 26/26 +
   in-VM 19/19 still bit-exact on FreeBSD aarch64.
2. **bench, partial result.** `vecarith` (prologue-
   dominated, no loop) drops from **2.01 → 1.03 ns**
   in-VM — about half, as expected once the per-lane
   constant build is gone. `heavyvec` (loop-dominated,
   256 iters) stays at native parity within the bench
   noise floor (~300 ns vs ~230 ns native, 0.77× — same
   as pre-pool 0.79×); the residual gap is now the
   entry-edge packed Phi-move + the inherent per-iter
   cost of an unfused `.4s` loop, not the prologue. So
   the arc closes the *categorical* per-call setup cost
   cleanly (proved by `vecarith` + disasm); the
   loop-amortised `heavyvec` shape was never going to
   move much from this lever.

#### Next arc — scoped: texture/sampler support, end to end

The last large unmeasured shader category. The Atrium-Tier-2
IR declares `ImageSampleImplicitLod`, `ImageSampleExplicitLod`,
and `ImageFetch` ops (their doc-comment says they "lower to
calls into atrium-spv-runtime"), but right now **nothing
in the stack implements them**: no frontend lowering from
SPIR-V `OpImage*`, no interpreter, no runtime crate, no
backend codegen — only the IR variants exist. Bespoke
returning `Unsupported` is actually shared-absent across
all three runners; this is a *coverage* gap, not a *perf*
gap, and it blocks the renderer from handling almost any
real-world shader.

**Design — runtime-call lowering.** Sampling lowers to a
C-ABI helper call (per the IR doc-comment), not inline
NEON. Inline NEON for bilinear + wrap-mode + format-decode
is its own substantial perf arc; runtime calls are the
correct first step (and the fast-tier perf bar — clang's
`-O2` on a software texture sampler is itself ~hundreds of
instructions, so a single call site is a comparable
ceiling).

**ABI sketch.**

```c
struct atrium_tex_desc {
    const void *data;           // texel array, row-major
    uint32_t width, height;
    uint32_t stride_bytes;
    uint32_t format;            // RGBA8/BGRA8/R8/RG16F/...
};
struct atrium_sampler_desc {
    uint32_t mag_filter;        // 0=nearest 1=linear
    uint32_t min_filter;
    uint32_t wrap_s, wrap_t;    // 0=clamp 1=repeat 2=mirror
};
void atrium_tex_sample_2d(
    const struct atrium_tex_desc *tex,
    const struct atrium_sampler_desc *samp,
    float u, float v,
    float out_rgba[4]);
void atrium_tex_fetch_2d(
    const struct atrium_tex_desc *tex,
    int32_t x, int32_t y, int32_t lod,
    float out_rgba[4]);
```

The fragment-shader AAPCS64 already passes `uniforms` in
X1; descriptor sets live there as an array of
`{tex_desc*, samp_desc*}` pairs indexed by SPIR-V binding
number. Backends emit one of these as a small load
sequence (descriptor pointer from uniforms[binding*16],
then `bl atrium_tex_sample_2d` with the resolved args).

**Risk — calling convention from JIT-emitted code.** A
`bl <runtime helper>` from a `MAP_JIT` / `PROT_EXEC` blob
to a libc-mapped function needs the loader to resolve the
runtime helper address and either patch a relocation or
load-through-pointer. The JIT-emit path was deliberately
designed reloc-free; the cleanest path is `adrp + ldr` of
a function-pointer slot the loader fills in before mapping
(one slot per helper, baked into each blob's header). This
extends the `atrium-spv-blob` format slightly — a
"runtime imports" table — but the change is bounded.

**Phasing.**
0. **✅ done** (`42fcbe8`). New `atrium-spv-runtime`
   crate. `#[repr(C)]` `TexDesc` / `SamplerDesc` + two
   `extern "C"` entry points (`atrium_tex_sample_2d`,
   `atrium_tex_fetch_2d`). Format support (v1):
   RGBA8Unorm, BGRA8Unorm (matches the Atrium scanout
   buffer), R8Unorm. Filter: Nearest, Linear (bilinear).
   Wrap: ClampToEdge, Repeat, Mirror. 8 unit tests
   covering corner fetches, nearest-at-centre, bilinear
   at the four-texel meeting point, BGRA channel swap,
   R8 replication, and all three wrap modes (positive +
   negative + full period). No dependencies (pure compute
   over raw byte buffers).
1. **✅ done.** Frontend: SPIR-V `OpSampledImage` /
   `OpImageSampleImplicitLod` / `OpImageSampleExplicitLod`
   / `OpImageFetch` lower into IR `Op` variants. Added
   `Op::CombineSampledImage { image, sampler }` (the IR
   was missing the combiner — only the sample/fetch ops
   existed). Added `StorageClass::UniformConstant` for
   image/sampler descriptor variables. The image/sampler
   `OpLoad`s flow through the existing `Op::Load` path
   opaquely — backends resolve descriptor handles from
   the variable identity at the sample call site, not
   from any loaded "value" (the load is a no-op handle
   alias in this model). Two frontend tests
   (`tests/image_sample.rs`): GLSL-style combined
   `sampler2D` shape (single `OpTypeSampledImage`
   variable + `OpLoad` of the sampled-image) and the
   Vulkan-native split shape (separate `image2D` +
   `sampler` variables joined via `OpSampledImage` at
   the use site). Both lower to `Op::ImageSampleImplicitLod`;
   the split form additionally produces
   `Op::CombineSampledImage`. Bindings flow through
   variable identity, not yet through an explicit
   binding-number IR field — phase 4/5 will surface that
   when backends need it.
2. **✅ done.** Interpreter (in `atrium-spv-tests`):
   sampler-aware `OpImageSampleImplicitLod` /
   `OpImageSampleExplicitLod` + `OpSampledImage`
   handlers. `ShaderInputs` grew a `textures:
   Vec<TextureBinding>` field; the interpreter indexes
   `OpDecorate DescriptorSet/Binding` annotations into a
   `var_binding: HashMap<Word, (u32,u32)>` and indexes
   `OpTypeImage` / `OpTypeSampler` / `OpTypeSampledImage`
   into `TypeInfo`. `OpLoad` of an image/sampler/sampled-
   image variable short-circuits the byte-buffer load
   and produces a `ConstantValue::Texture { set, binding }`
   handle; `OpImageSample*` looks up the matching
   `TextureBinding` and calls `atrium_spv_runtime::
   sample_2d` (the safe Rust wrapper added alongside
   so the interpreter crate stays
   `#![forbid(unsafe_code)]`). The interpreter shares the
   *exact* sampler implementation the production backends
   will FFI-call, so the differential checks pipeline
   correctness (frontend + backend codegen), not the
   sampler — the sampler is independently unit-tested in
   the runtime crate. Gate: a new
   `interpreter_bilinear_centre_of_rgbw_checker` test
   builds a 2×2 RGBW texture + a sampler2D shader,
   samples at u=v=0.5, asserts the pixel equals
   `(0.5, 0.5, 0.5, 1.0)` — the four-texel mean. Plus
   regression: all 10 atrium-spv-tests tests, 8 runtime
   tests, 26/26 differential.
3. **Folded into phase 4.** The original plan ("interpreter-
   only differential test") doesn't work standalone:
   `assert_shader_agrees` requires ≥2 successful runners,
   so a one-runner differential just fails the harness.
   The interpreter self-check is already in place
   (`interpreter_bilinear_centre_of_rgbw_checker` —
   phase 2's gate). The real differential gates land
   with phase 4 (interpreter + Cranelift) and phase 5
   (+ bespoke).
4. **✅ done.** Cranelift backend: emits the four-
   instruction descriptor + indirect-call sequence
   against the v1 ABI. `FnTranslator` grew an
   `image_handles: HashMap<ValueId, (u32, u32)>` side-
   table that `Op::ImageHandle` populates and
   `Op::CombineSampledImage` propagates (image binding
   wins; sampler binding ignored — both descriptors live
   in the same v1 slot). `Op::ImageSampleImplicitLod`
   reads the binding off `image_handles`, loads the
   helper fn-pointer + tex/samp descriptor pointers from
   the uniforms buffer at the ABI offsets, allocates a
   16-byte stack slot for the result pixel, builds the
   C-ABI signature `(*const TexDesc, *const SamplerDesc,
   f32, f32, *mut f32) → ()` and calls indirect through
   the loaded fn-ptr. Result lanes load back out as four
   `f32`s into `self.vectors`. **Reloc-free** (no
   external symbol used). New differential test
   `tests/texture_sample.rs::texture_sample_centre_rgbw`
   — 2×2 RGBW + sampler2D shader at u=v=0.5; interpreter
   and Cranelift both report `(0.5, 0.5, 0.5, 1.0)` and
   agree under a 1e-6 absolute tolerance. The whole
   pipeline works end-to-end: frontend → IR Ops →
   Cranelift codegen → object → cc → dlopen → call into
   a host-resident `atrium_tex_sample_2d`.

   **Descriptor ABI (v1, reloc-free)** — how compiled
   shader code finds bound textures + samplers + the
   runtime helpers it calls. The AAPCS64 split passes
   `uniforms` in X1; v1 overlays the buffer's prefix as
   a two-region header:
   * **Bytes 0..16** — runtime-helper function pointers
     (0: `atrium_tex_sample_2d`, 8: `atrium_tex_fetch_2d`).
   * **Bytes 16.. (`UNIFORMS_DESC_BASE`)** — flat
     descriptor table; slot `B` carries
     `(tex_desc*, samp_desc*)` at `UNIFORMS_DESC_BASE +
     B*DESC_SLOT_BYTES`.

   A backend that sees an `ImageSample` at binding `B`
   emits exactly four instructions:
   ```
   ldr  x_fn,   [X1]                       ; helper ptr
   ldr  x_tex,  [X1, #16 + B*16]           ; tex_desc*
   ldr  x_samp, [X1, #16 + B*16 + 8]       ; samp_desc*
   blr  x_fn
   ```

   The deliberate `blr <reg>` keeps the emitted code
   **reloc-free**, so the bespoke JIT-emit blob path
   works unchanged — the host caller patches in helper
   addresses at descriptor-table build time. The
   Cranelift `compile()` (object → cc → dlopen) path
   uses the *same* mechanism rather than going through
   `cc`'s dynamic linker, which keeps the two backends
   on one descriptor-table ABI.

   `atrium-spv-runtime` exposes `descriptor_table_buffer`
   + `write_helper_pointers` + `write_descriptor_slot`;
   the round-trip test `descriptor_table_layout_round_trips`
   pins the byte layout. Multi-set support + a dedicated
   descriptor-table register (X6) land later when a real
   shader needs both descriptors and a regular uniform
   block in the same invocation.
5. **✅ done.** Bespoke backend. Same v1 ABI as Cranelift:
   `image_handles` side table populated by `Op::ImageHandle`
   + propagated by `Op::CombineSampledImage`;
   `Op::ImageSampleImplicitLod` emits the descriptor +
   indirect-call sequence directly (sub sp, str x4, str x30,
   ldr x9/x10/x11, parallel-copy u/v through V2→V0/V1,
   mov x0/x1, add x2,sp,#0, blr x9, ldr lanes from stack,
   restore x4/x30, add sp). Because the descriptor ABI is
   already reloc-free (call-via-pointer), no
   `atrium-spv-blob` runtime-imports table was needed —
   the same byte pattern works for both the
   `compile()`-object path and the JIT-emit blob path.

   One bug found+fixed during bring-up: the bespoke
   backend's existing prologue doesn't save the link
   register (X30) because no prior shader made a function
   call. ImageSampleImplicitLod is the first op that does,
   and `blr` clobbers X30; the eventual `ret` reads it
   back as the return address, so without saving LR the
   function returns to a stale address and segfaults at
   the caller boundary. Fix: save+restore X30 alongside
   X4 in the call's stack frame.

   Gate: `texture_sample_centre_rgbw` now passes with all
   three runners (interpreter, Cranelift, bespoke) bit-
   tolerant agreeing on `(0.5, 0.5, 0.5, 1.0)`. Full diff
   26/26 + bespoke unit 7+4 + new texture diff 1, all
   green.

   *v1 limitation* — V-regs are caller-saved per AAPCS64
   (V16..V31; V8..V15 lower 64 bits callee-saved) and
   `blr` clobbers them. The test shader doesn't have
   values live across the call so the present codegen
   works; a real shader with V-reg values live across an
   `ImageSample` needs proper save/restore.

   **Update (follow-on landed):** the bespoke
   `ImageSampleImplicitLod` codegen now snapshots
   `owners.keys()` before allocating result lanes,
   spills each one as a full 128-bit `str q` to the
   call's stack frame, and reloads with `ldr q` after
   the call. Stack frame grows by `16 * N` where N is
   the live count; SP stays 16-aligned. Conservative
   (saves coord lanes that are consumed by this very
   op) but correct. **`n_spill = 0`** for both
   `texture_sample_centre_rgbw` and the in-VM
   `texsample` so the existing gates pass identically
   on the simple shape.

   A staged test `texture_sample_tinted` (a `sampler2D`
   shader that multiplies the sampled pixel by a tint
   vec4 — tint's V-regs are live across the
   `ImageSample` and exercise `n_spill = 4`) exposed a
   *separate, pre-existing* regalloc bug: a hoisted
   ConstFloat gets its V-reg reallocated to a second
   ConstFloat while the first is still live, so the
   sample's u/v end up reading a clobbered constant.

   **Fix landed.** Root cause: `compute_last_use_flat`'s
   catch-all `_ => {}` arm captured `Op::ImageSample*` /
   `Op::ImageFetch` / `Op::CombineSampledImage`, so the
   coord operand's *per-lane scalars* — the V-regs the
   sample call site actually reads via
   `scalars[coord_lane.id]` — were never marked at the
   ImageSample's index. A ConstFloat that only fed a
   sampler's `uv` (like `c_quarter` shared between
   `uv`'s two lanes) had its `last_use` set only at the
   ConstVec emit, so the linear-scan recycled its V-reg
   on the very next inst, even though `ImageSample`
   downstream still needed it. Added explicit arms for
   the four image ops that mark `coord.id`, propagate to
   `vec_lanes[coord]`, and mark `sampled_image.id` /
   `lod.id` / `image.id` / `sampler.id` where present.
   `texture_sample_tinted` re-enabled and now passes
   5/5 across parallel runs alongside the 26-shader
   three_way diff.
6. **✅ done.** In-VM `texsample` shader added to
   `run-in-vm.sh`. The harness's `texsample` mode builds
   a 2×2 RGBW texture + Nearest/Clamp sampler in C,
   packs the v1 uniforms-buffer prefix (helper pointers
   at bytes 0..16, descriptor slot 0 at 16..32 — the
   sample fn-ptr points at a C-side
   `atrium_tex_sample_2d` baked into the harness binary),
   and invokes the shader. The bespoke-emitted shader
   reads everything out of uniforms and `blr`s through —
   reloc-free. Expected pixel `(1, 0, 0, 1)`: the
   centre of texel (0,0) at u=v=0.25 (post the Vulkan
   `u*w - 0.5` mapping). **Result on FreeBSD aarch64:
   PASS** — bespoke ELF → cc -shared → dlopen → blr
   into the host C sampler → red pixel. 20/20 in-VM
   shaders green. The texture/sampler arc is end-to-end
   complete: every layer (frontend, IR, interpreter,
   Cranelift, bespoke, in-VM gate) handles image-sample
   correctly under the same v1 ABI.

#### Next big arc — scoped: matrix ops + MVP transform

The vertex stage compiles, the uniform-block reads agree
across all three runners — but a real vertex shader's
job is `gl_Position = mvp * vec4(in_pos, 1.0)`, and
`mat4 * vec4` is the missing piece. Everything below
the matrix multiply already exists.

**Design.**

* IR `Type::Mat4(VecElement)` — opaque at the type level
  (the IR doesn't need to know it's 4 columns under the
  hood; the backend lowers).
* IR `Op::MatrixTimesVector { matrix, vector }` —
  matrix-times-column-vector per SPIR-V semantics:
  ```
  result[i] = Σ matrix[j][i] * vector[j]
  ```
  Column-major; the SPIR-V `OpMatrixTimesVector`
  operand order is `(matrix, vector)`.
* Frontend: translate `OpTypeMatrix` → `Type::Mat4`,
  `OpMatrixTimesVector` → the IR op. `OpAccessChain`
  into a uniform struct's `Mat4` member produces a
  `Pointer(Uniform, Mat4)`; the resulting `OpLoad` —
  *here's the wrinkle* — returns a `Mat4` value the
  backend has to materialise as 4 column vec4s.
* Backend lowering: `MatrixTimesVector` desugars to
  ```
  c0 = column[0] * vector.x
  c1 = column[1] * vector.y
  c2 = column[2] * vector.z
  c3 = column[3] * vector.w
  result = c0 + c1 + c2 + c3
  ```
  = 4 vec×scalar broadcasts (`FMul`) + 3 vec+vec adds
  (`FAdd`). All ops already exist + are tested. Cranelift
  + bespoke can both lower this generically; the NEON
  pack classifier should pick `result` up as packed (4
  vec×vec FMul/FAdd on a closed component) and the
  bespoke backend should emit `.4s` ops automatically.
* Interpreter: a single `Op::MatrixTimesVector` handler
  evaluating the same dot-product expression.

**Phasing.**
0. **✅ done.** IR: added `Type::Mat4(VecElement)` +
   `Op::MatrixTimesVector { matrix, vector }`. All
   downstream crates rebuild cleanly (existing matches
   use `_` arms / specific variants — new variants don't
   break dispatch).
1. **✅ done (frontend).** Translate `OpTypeMatrix` →
   `Type::Mat4` with v1 validation (4-column / vec4
   element only — mat2/mat3 wait for a real shader that
   needs them). Translate `OpMatrixTimesVector` → the
   IR op via the existing `emit_binop_float` plumbing
   (operand order: matrix-then-vector, per SPIR-V
   `M *cv v`).

   *Still pending in phase 1:* `OpAccessChain` stepping
   into a Mat4 member + `OpLoad` materialising a matrix
   value. Those come once the test exercises the path
   end-to-end (i.e., once interpreter + backends handle
   `Op::MatrixTimesVector` and we wire a uniform-mat4
   shader).
2. **✅ done.** Interpreter. `TypeInfo::Matrix { column,
   count }` indexed from `OpTypeMatrix`; `load_from_storage`
   handles matrix types by loading `count` consecutive
   columns at 16-byte stride into a `ConstantValue::Vec`-
   of-`Vec`s. `Op::MatrixTimesVector` handler walks the
   column-major dot products. Two new tests
   (`atrium-spv-tests/tests/vertex_matrix.rs`):
   * `interpreter_mvp_transforms_position` — uniform
     translation matrix applied to a vec3 input attribute,
     expects `pos + (tx, ty, tz)`.
   * `interpreter_mvp_scale_matrix` — uniform diagonal
     scale matrix, expects each lane scaled.

   First mat4 × vec4 transform working end-to-end (host-
   side, oracle level).
3. Cranelift: lower `Op::MatrixTimesVector` to 4
   broadcasts + 3 adds + 1 add of the four columns.
   No new code-generator surface — reuses existing
   FMul/FAdd paths.
4. Bespoke: same lowering. Verify the NEON pack
   classifier sees the chain and emits `.4s` ops.
5. Differential `three_way_mvp_transform` — uniform
   mat4 + vec3 attribute → gl_Position. All three
   runners agree.
6. In-VM gate: a `vertex_mvp` shader added to
   `run-in-vm.sh` driven by the existing
   `vertex_harness`.

**Risk — Mat4 as a value type.** The IR's pointer-
indexing model (`Op::AccessChain { base, byte_offset }`)
makes column access trivial (offset += 16 * index). But
loading a Mat4 as a *value* (without indexing) needs the
backend to materialise it as 4 column vec4s — multiple
V-regs / packed Q-regs at once. The simplest model:
forbid `OpLoad` of a Mat4 directly; require shaders to
do `OpMatrixTimesVector` (which feeds straight from the
pointer with column-access desugared). GLSL/HLSL→SPIR-V
producers don't typically emit "load a whole matrix then
use it" — they pass matrix pointers around and only
materialise at use sites.

#### Next big arc — scoped: vertex stage

Texture/sampler closes the fragment-shader functionality
story. The renderer's largest remaining feature gap is
the **vertex stage** — without it, no pipeline; without
a pipeline, no rasterized triangles. Scoping it here
before commitment so the path is visible.

**What exists.** Scaffolding-level support across the
stack: `ShaderStage::Vertex`, frontend translates
`ExecutionModel::Vertex`, both backends export
`atrium_vs_main` as the symbol, and the Cranelift
backend already declares a full Vertex `Signature`
(per the spec'd AAPCS64 layout):

```
atrium_vs_main(
    in_attributes:    *const u8,    // X0
    in_attr_strides:  *const u8,    // X1
    uniforms:         *const u8,    // X2
    push_constants:   *const u8,    // X3
    vertex_index:     u32,          // W4
    instance_index:   u32,          // W5
    out_position:     *mut f32,     // X6 (vec4)
    out_varyings:     *mut u8,      // X7
    out_clip_distance:*mut f32,     // X8 (struct-return slot)
)
```

The bespoke backend's `resolve_pointer_param` does NOT
yet have a Vertex mapping (only Fragment does); same
for the in-VM harness (`harness.c` only loads
`atrium_fs_main`). No vertex differential test exists.
So the door is open; the work is wiring through.

**Phasing.**
0. **✅ done.** Frontend smoke gate
   (`tests/passthrough_vertex.rs`). Hand-built SPIR-V
   vertex shader: vec3 `Location=0` attribute → loads
   `pos`, extracts x/y/z, constructs `vec4(pos, 1.0)`,
   stores it via OpAccessChain into the gl_PerVertex
   block's `gl_Position` member. The frontend produces
   a single function with `stage = Vertex`, one entry
   point, and one vertex input at location 0 of
   `Vec3(F32)`. One minor wrinkle: the gl_PerVertex
   struct member needs an `Offset` annotation alongside
   the `BuiltIn Position` one — the frontend's
   AccessChain walker keys struct member layouts on
   `OpMemberDecorate Offset`, and SPIR-V producers
   that target Vulkan typically emit Offset for
   gl_PerVertex anyway, so this isn't a real
   compatibility constraint, just an assertion in the
   test setup.
1. **✅ partial.** Interpreter vertex path. Added
   `Interpreter::run_vertex(inputs)` parallel to
   `run_fragment`; `VertexOutputs { positions: Vec<[f32;4]> }`;
   `ShaderInputs.vertex_attributes_per_invocation:
   Vec<Vec<u8>>`; `vertex_entry: Option<Word>` populated
   from `OpEntryPoint`. The block walker is a near-copy
   of the fragment one (block-stepper + Phi resolver +
   eval_inst per non-terminator); the post-execution
   output extraction scans `storage` for any 4-lane
   `ConstantValue::Vec` — the gl_Position store via
   `OpAccessChain` lands there. Two new tests
   (`tests/vertex_constant.rs`):
   `interpreter_run_vertex_constant_position` (writes
   `[0.25, 0.5, 0.75, 1.0]` to gl_Position, asserts
   exact match) and
   `interpreter_run_vertex_one_invocation_per_attribute_entry`
   (three attribute entries → three identical positions).

   **Phase 1b (✅ done).** `OpLoad` from
   `StorageClass::Input` + invocation-index threading.
   `eval_inst` + `load_from_storage` gained an
   `inv_idx: usize` parameter; `eval_fragment_invocation`
   + `eval_vertex_invocation` both thread the loop
   counter through, and `load_from_storage`'s Input arm
   reads from
   `inputs.vertex_attributes_per_invocation[inv_idx]`.
   Two new tests (`tests/vertex_passthrough.rs`):
   `interpreter_passthrough_vertex_single_invocation`
   (vec3 (0.25, 0.5, -0.75) → vec4(..., 1.0)) and
   `interpreter_passthrough_vertex_three_vertices`
   (distinct attribute per vertex → distinct positions
   per invocation; gates the inv_idx threading).
2. **✅ done.** Cranelift vertex codegen. Existing
   scaffolding (build_signature, resolve_pointer_param
   Input/Uniform/PushConstant mappings) was nearly
   complete — only the Output mapping needed
   adjustment: `(Vertex, Output) → params[6]`
   (`out_position`), not `params[7]` (`out_varyings`).
   The v1 mapping assumes the shader only writes
   gl_Position; a real shader with both gl_Position and
   Location-decorated varyings needs richer dispatch
   (look at the variable's `BuiltIn` vs `Location`
   decoration). Queued for phase 4+.

   Two new tests (`atrium-spv-differential/tests/
   vertex_constant.rs`):
   * `cranelift_constant_position_vertex` —
     `gl_Position = constant_composite vec4(0.25, 0.5,
     0.75, 1.0)`, compiled through Cranelift,
     `cc -shared`, `dlopen`, `atrium_vs_main(...)`
     invoked with all-null args + a host stack slot
     for `out_position`. Result matches both the
     interpreter and the literal expected.
   * `cranelift_passthrough_vertex` — the real
     passthrough: `vec3 location=0` attribute
     `(0.25, -0.5, 0.75)` packed as 12 bytes, fed via
     `in_attributes`. The shader OpLoad's the vec3,
     `composite_construct(x, y, z, 1.0)`, stores into
     gl_Position. Cranelift's output matches the
     interpreter's bit-tolerant.

   **First-ever vertex shader compiled through a
   backend and producing correct gl_Position on host.**
3. **✅ done.** Bespoke vertex codegen. Three small
   changes:
   * Lift the `func.stage != Fragment` early-return at
     `emit_function`'s top to allow `Vertex` too.
   * Make `x_out` (the primary-output register) stage-
     dependent: Fragment → `X4` (out_color), Vertex →
     `X6` (out_position).
   * Extend `resolve_or_make_pointer` to take
     `stage: ShaderStage` and dispatch on `(stage,
     storage_class)`, matching the AAPCS64 splits per
     `docs/spec/tier2-renderer.md` §4.1 (Vertex:
     X0/X1=in_attributes/strides, X2=uniforms,
     X3=push_constants, X6=out_position).
   * Same v1 single-Output-mapping limitation as
     Cranelift — gl_Position only; mixed varyings need
     phase 4+ richer dispatch.

   Both `bespoke_constant_position_vertex` and
   `bespoke_passthrough_vertex` pass alongside their
   Cranelift twins in `tests/vertex_constant.rs` (4
   tests total now: 2 backends × 2 shapes). The in-VM
   suite stays at 20/20 — bespoke vertex codegen
   doesn't disturb fragment paths.

   **First vertex shader compiled through the bespoke
   backend producing correct gl_Position on host.**
4. **✅ effectively covered.** Per-backend differential
   (interpreter vs Cranelift, interpreter vs bespoke)
   is already in `atrium-spv-differential/tests/
   vertex_constant.rs` — 4 tests, 2 backends × 2 shapes.
   The transitive guarantee (Cranelift ≡ interpreter ≡
   bespoke ⇒ Cranelift ≡ bespoke) is the same as what
   a single 3-way `assert_shader_agrees` would give.
   The fragment-shaped `assert_shader_agrees` helper
   itself needs a vertex-aware sibling (the `ShaderRunner`
   trait is fragment-shaped) — landing one in
   `atrium-spv-tests/src/harness.rs` is small but only
   pays off when the vertex test corpus grows past a
   handful of shapes; deferred until then.
5b. **✅ done.** Fragment-varying-load — the natural
   follow-on after vertex stage landed. The interpreter's
   `load_from_storage` Input arm now dispatches by
   stage: vertex → `vertex_attributes_per_invocation`,
   fragment → `varyings_per_invocation` (an `is_vertex`
   flag threaded through `eval_inst`). The differential
   harness's `run_via_dlopen` now passes
   `varyings_per_invocation[i].as_ptr()` as the
   `in_varyings` X0 argument per invocation; backends'
   existing `(Fragment, Input) → X0` mapping reads
   through.

   New tests: `atrium-spv-tests/tests/fragment_varying.rs::
   interpreter_fragment_passthrough_varying` (three
   varyings → three pixels via the interpreter alone) and
   `atrium-spv-differential/tests/fragment_varying.rs::
   three_way_fragment_passthrough_varying` (interpreter +
   Cranelift + bespoke all agree). First test that
   actually consumes per-pixel varying data — the real
   shape of inter-stage data flow once a rasterizer is
   wired.

5. **✅ done.** In-VM vertex harness on FreeBSD aarch64.
   `verify/vertex_harness.c` — dlopen + dlsym
   `atrium_vs_main`, packs three argv-floats into a
   12-byte attribute buffer, calls vs_main per the
   AAPCS64 vertex ABI, prints `out_position` as four
   f32s with `%.9g`. `run-in-vm.sh` gained `verify_vertex
   <label> <x> <y> <z> <emit-args...>` parallel to the
   fragment `verify()`, plus an extra scp/cc step to
   ship + build the harness in the VM. The
   `vertex_passthrough` kind in `emit_freebsd_obj.rs`
   emits the passthrough shader (vec3 location=0 →
   gl_Position = vec4(in.xyz, 1.0)); harness diffs
   against the expected `[0.25, -0.5, 0.75, 1.0]` for
   attr `(0.25, -0.5, 0.75)`. **Result on FreeBSD
   aarch64: PASS.** 21/21 in-VM shaders green (20
   fragment + 1 vertex). First vertex shader running on
   the target.
6. Rasterizer integration (the bridge that turns a
   vertex-stage + fragment-stage pair into pixel
   output): out of scope for this arc — a separate
   substantial arc once both stages individually pass
   their gates.

**Risk — matrix types + per-vertex inputs.** Real
vertex shaders need `mat4 * vec4` and per-vertex
attribute interpolation. Matrix types are vec4×4 with
column-major layout in SPIR-V; lowering them sanely is
its own sub-arc. Phase 0 limits the smoke shader to a
passthrough (no matrix) so the ABI plumbing can land
first; matrix support comes in a follow-on.

#### `heavyvec` tail inquiry — loop rotation is the lever

Disassembled `clang -O2 -ffp-contract=off` of
`native/heavyvec.c` to see exactly what shape we're
chasing. The body:

```
loop:
  fmul v0.4s, v0.4s, v1.4s
  subs w8, w8, #1            ; combined sub + flag-set
  fadd v0.4s, v0.4s, v2.4s
  b.ne loop
```

**4 instructions per iteration.** Bespoke's loop body is
6:

```
  cmp  w13, w15              ; top-tested compare-against-n
  b.lt body
  b    exit
body:
  fmul v.4s
  fadd v.4s
  add  w13, w13, #1          ; increment counter
  b    loop                  ; back-edge
```

Three structural differences:
1. **Bottom-tested vs top-tested.** clang puts the branch
   at the end of the body; bespoke puts the compare at
   the top + an unconditional back-edge. Two-versus-one
   branch per iter, plus the loop-entry skip-check (bespoke
   even adds an unconditional `b exit` after the `b.lt`,
   though that one runs only once).
2. **`subs` instead of `add` + `cmp`.** clang decrements
   the counter *to zero* and uses the flag side-effect of
   `subs` to drive `b.ne`. Bespoke increments and
   separately compares against `n`. Saves one instruction
   per iter.
3. **No 9-NOP prologue.** clang's preamble has 0 NOPs
   because it knows it uses no callee-saved regs. Bespoke
   reserves the prologue NOP slots before knowing whether
   any callee-saved bank will get touched; for `heavyvec`
   (V16-V19 only — all caller-saved) the 9 NOPs stay as
   NOPs, ~9 free-but-not-free cycles per call.

The 2-instruction-per-iter delta × 256 iters explains the
gap. So the residual `heavyvec` tail is **loop rotation +
loop-counter strength-reduction to a `subs`-driven
countdown**. Not a peephole — a structural codegen
change. Shared-absent in Cranelift too (bespoke matches
its 6-inst body), so closing this is another category-
shared optimisation, like loop-idiom recognition. Scoped
as a *separate* arc when it becomes the bottleneck on a
real shader workload, not chased speculatively.

#### Compile-pipeline phase breakdown — `cc` is the cost

With the backend ~4.7× faster, the question came up: is
compile now fast enough to do in-memory every run
(llvmpipe-style) and drop the on-disk `.so` cache?
`atrium-spv-compile` was instrumented to split its wall
clock into frontend / backend / link (the G7 metrics line
gained `frontend_us` / `backend_us` / `link_us`). Measured
over the full corpus, host and in-VM:

| phase             | typical time      | share |
|-------------------|-------------------|-------|
| frontend (SPIR-V→IR) | ~50–210 µs     | ~0.4% |
| backend (IR→object)  | ~30–100 µs (bespoke); ~0.8–1.4 ms (Cranelift fallback) | ~0.2% |
| **link (`cc`→`.so`)** | **~36–43 ms**   | **~99.5%** |

`cc` is essentially the *entire* compile cost — a process
spawn + full linker run. The bespoke backend's 4.7× win
moved ~70 µs inside a ~41 ms pipeline; invisible
end-to-end.

So the architecture conclusion is **not** "compile every
run":
* The jailed compile sub-process (D3) stays regardless —
  it's a security boundary, not a speed knob; compiling
  in-process llvmpipe-style would forfeit it.
* The persistent content-hash cache stays — cross-run
  reuse is real on a desktop platform; "recompile every
  launch" discards free work.
* **The lever is dropping `cc`.** A JIT-emit path —
  backend emits a flat relocatable code blob instead of an
  ELF object, the loader `mmap`s it `PROT_EXEC` instead of
  `cc -shared` + `dlopen` — would cut compile from ~41 ms
  to ~0.3 ms (~130×) *and* remove the `cc` toolchain
  dependency from the runtime. The jail and the cache are
  kept; the cache just stores the blob. Trade-off: loses
  the dynamic linker's symbol info for debuggers/profilers
  — which is exactly what the `.pcmap` sidecar already
  mitigates. This is the high-value follow-up; "compile
  every run" is not.

#### JIT-emit path — scoped design

Goal: replace `object → cc -shared → .so → dlopen` with
`object → flat code blob → mmap PROT_EXEC`. Target ~41 ms
→ ~0.3 ms compile, and no `cc` in the runtime dependency
set. **Unchanged:** the jailed `atrium-spv-compile`
sub-process (D3), the SHA-256 content-hash cache, the
`v{N}/` ABI versioning, the bespoke-first/Cranelift
backend selection, the `.pcmap` sidecar.

**Why this is mostly cheap for the bespoke backend.** Its
`.text` is already self-contained, position-independent
machine code: branches are PC-relative and patched
*inside* `compile()` (the `branch_relocs` /
`cond_branch_relocs` passes), float constants are
materialised inline (`mov`/`movk`+`fmov`, no constant
pool), and the fragment ABI is entirely
register/pointer — **no external relocations, no rodata,
no `.data`**. `compile()` already has the raw bytes in its
`asm::Asm` buffer *before* it wraps them in an
`object::write::Object`; a `compile_blob()` is strictly
less work than the object path. The entry point is offset
0 (the function starts with the prologue).

**The blob format** (`atrium-spv-blob` crate, or fold into
`atrium-spv-pcmap`):
`[magic | version | arch | flags | code_len | entry table | code …]`.
The entry table names which of `atrium_fs_main` /
`atrium_vs_main` / `atrium_cs_main` are present and their
byte offsets. A flat AAPCS64 code blob is **OS-agnostic**
(only arch-specific) — a nice simplification over the
ELF-vs-Mach-O object split. Reloc/rodata fields are
reserved but unused for bespoke; see open question 1.

**Per-component changes:**
* `atrium-spv-backend-bespoke` — add `compile_blob()`
  returning `(code: Vec<u8>, entry_offsets, pcmap)`
  alongside the existing `compile()`. Nearly free.
* `atrium-spv-backend-cranelift` — needs the same, but
  Cranelift's blob output is the real unknown (open Q1).
* `atrium-spv-compile` — drop `link_to_shared_lib`; write
  `<hash>.afblob` + `<hash>.pcmap`. Backend selection
  unchanged.
* `atrium-spv-loader` — new `mmap`-load path beside
  `dlopen.rs` (same local `allow(unsafe_code)` island):
  `mmap` anon RW → copy code → (apply relocs) →
  **flush the icache** → `mprotect` RX → entry pointers =
  `base + offset`. `LoadedShader` swaps its
  `libloading::Library` field for the mapping handle
  (drop = `munmap`); same outward shape.

**Phasing** (each phase gated by the differential + in-VM
suites):
1. Blob format crate + `compile_blob()` on the bespoke
   backend (no relocs — the easy 80%). Unit-test round-trip.
2. Loader `mmap`-load path incl. the icache flush. Host
   unit test: bespoke → blob → mmap → call a `const`
   shader.
3. Wire `atrium-spv-compile` to emit blobs and
   `atrium-spv-loader` to load them; bump the cache `v{N}`.
   Run the full differential + in-VM + bench suites — the
   bench should now show the ~130× compile win.
4. Cranelift blob output (open Q1) — or, interim, keep
   Cranelift-compiled shaders on a `cc` fallback path so
   the common (bespoke) case still gets the win.

**Open questions / risks:**
1. **Cranelift relocations** — does `cranelift-object`'s
   output carry rodata-relative or other relocs? If so the
   blob format needs a reloc table the loader applies, or
   Cranelift must be driven via its lower-level
   code+relocs API (it's built for JIT — `cranelift-jit`
   exists — so feasible). This decides whether phase 4 is
   small or a sub-arc.
2. **ARM64 icache flush** — writing code then executing it
   *requires* an explicit i-cache invalidation; `dlopen`
   does this for us today. Must not forget — stale-icache
   bugs are nondeterministic and brutal.
3. **`PROT_EXEC` mmap in the daemon's environment** — the
   loader runs in the daemon (not the jail); confirm no
   MAC policy / jail cap blocks anonymous executable
   mappings on the FreeBSD target.
4. **Debugger / profiler visibility** — a raw mmap'd blob
   has no dynamic-linker symbol; backtraces show a bare
   address. `.pcmap` covers crash-triage source
   attribution; live `perf` would later want a jitdump
   registration. Acceptable, noted.
5. **Multi-function modules** — if the frontend ever keeps
   `OpFunctionCall` (separate IR functions rather than
   inlining), the blob needs intra-blob call relocation.
   Current shaders are single-function; confirm the
   frontend's policy before relying on "no relocs".

#### JIT-emit path — status

* **Phase 1 ✅** (`8bdef55`). New `atrium-spv-blob` crate:
  48-byte header + flat PIC code, `to_bytes`/`from_bytes`
  with full validation (9 tests). Bespoke `compile_blob()`
  — strictly less work than `compile()`, since the
  backend's `.text` is already self-contained.
* **Phase 2 ✅** (`135bbf5`). `atrium-spv-loader::jitmap` —
  `map_blob()` does `mmap` → copy → **icache flush**
  (`__clear_cache` on FreeBSD/Linux, `sys_icache_invalidate`
  on macOS) → `mprotect` RX, with a `MAP_JIT` path for the
  macOS host build. Host unit test maps + *runs* a real
  const shader.
* **Phase 3 ✅** (`417524a`). Wired through: `atrium-spv-compile`
  bespoke path writes `<hash>.afblob` (no `cc`);
  `atrium-spv-loader` prefers the blob, `mmap`s it, falls
  back to `dlopen` for Cranelift `.so`s. `LoadedShader`'s
  backing is now a `CodeBacking` enum. **Measured: heavy
  shader compile `~41 ms → ~1 ms`** (`link_us` 41000 → 0);
  the 292-byte `.afblob` replaces a ~16 KB `.so`. 25/25
  differential + 3/3 in-VM scripts pass — the FreeBSD
  `__clear_cache` + `mprotect` path executes correctly
  on-target. (`run-e2e-in-vm.sh` retired —
  `run-loader-e2e-in-vm.sh` drives the real loader and
  supersedes it.)
* **Phase 4 ✅** (`2bef9c5`). Open question 1 answered
  empirically: dumped the Cranelift object's relocations
  across the whole corpus — *zero* relocations, *zero*
  rodata, every shader. Cranelift's aarch64 lowering
  materialises constants inline just like the bespoke
  backend. So phase 4 was the small version: the Cranelift
  backend gained `compile_blob()` (build the object, then
  re-parse it to lift the flat `.text` + entry offsets,
  with a loud relocation guard), and `atrium-spv-compile`
  dropped `Artifact::Object`, `link_to_shared_lib`, and
  the `cc` invocation entirely. **`cc` is now completely
  gone from the production compile pipeline** — both
  backends emit flat blobs. The `unordcmp`
  Cranelift-fallback shader compiles in ~1 ms to a
  112-byte `.afblob` (was ~41 ms).

The JIT-emit arc is **complete**: `cc` eliminated, compile
~41 ms → ~1 ms across both backends, the loader `mmap`s
flat blobs `PROT_EXEC`. The jailed compile sub-process,
the content-hash cache, and the `.pcmap` sidecar are all
unchanged. `dlopen.rs` stays as the legacy reader for any
pre-existing `.so` cache entries; nothing produces `.so`
anymore. 25/25 differential + 3/3 in-VM scripts pass on
the FreeBSD target throughout.

#### Next big arc — scoped: tier-2 rasterizer

The shader-compile pipeline is complete (frontend, both
backends, blob, mmap, .pcmap, JIT-emit).  The matrix arc
gave us a real `gl_Position = mvp * vec4(in_pos, 1.0)`
vertex shader.  The piece still missing between a
*compiled vertex+fragment shader pair* and *pixels on the
target image* is the rasterizer — the driver that walks a
draw call, calls the VS per vertex, assembles primitives,
clips, rasterises, and calls the FS per fragment.

Currently `Tier2Backend::submit_frame`
(`aqueduct-gpu-host/src/tier2_backend.rs`) just calls
`Tier2Registry::fill_image_fragment` once per pass — a
fullscreen `for y in 0..h { for x in 0..w { fs_main(...) } }`
loop.  Geometry is ignored entirely.  This is fine for
the fullscreen-quad demos used to validate the FS
codegen, but no real Vulkan draw can ride it.

Spec reference: `docs/spec/tier2-renderer.md` §8.

**Phasing.**  Eight sub-phases, in the order each unlocks
the next one's tests.  Each is gated on the differential +
in-VM suites staying green.

1. **R.1 — hello triangle.**
   * Index/draw walker (just 3 indices for now).
   * Vertex shading: call `atrium_vs_main` 3× with each
     vertex's attribute buffer; collect 3 `gl_Position`s.
   * Triangle setup: assume positions already in NDC
     (skip perspective divide), map NDC → screen via
     viewport, compute Pineda edge-function coefficients
     for the 3 edges.
   * Pixel loop over the screen-space bbox: pixel inside
     iff all three edge functions have the same sign
     (front-face winding); on inside, call `atrium_fs_main`
     with a constant set of varying values (R.2's job to
     interpolate them).
   * No depth, no blend, no clipping (positions must be
     in-bounds), no varyings.
   * Gates the entire pipeline shape.  Pixels written by
     the bespoke and Cranelift FS through real geometry.

2. **R.2 — perspective-correct varying interpolation.**
   * Add `1/w` per-vertex, interpolate `attr/w` and `1/w`
     barycentrically, recover `attr = (attr/w)/(1/w)` per
     pixel.
   * Perspective divide and viewport mapping go here.
   * Test: pass `vec3 colour` as a varying — gradient
     across the triangle.

3. **R.3 — depth buffer.**
   * Allocate a per-image `Vec<f32>` depth buffer parallel
     to the colour buffer.
   * Interpolate `gl_FragCoord.z`; depth test (default
     `LESS`); depth write.
   * Test: two overlapping triangles, the nearer one wins
     in the overlap region.

4. **R.4 — clipping.**
   * Guard-band clipping or full Sutherland-Hodgman against
     the 6 view-frustum planes.
   * Test: triangle with one vertex outside the NDC cube.

5. **R.5 — blending + colour mask.**
   * Alpha-over, alpha-blend factors, fixed-pipeline blend
     state matching Vulkan's `VkPipelineColorBlendAttachmentState`.
   * Test: red triangle with `α=0.5` over a green background.

6. **R.6 — tiled raster.**
   * 8×8 or 16×16 tiles; per-tile bbox cull; iterate inside-
     bbox pixels.
   * Single-threaded first; no perf change expected.
   * Test: existing tests still pass.

7. **R.7 — multi-thread tiles.**
   * Each tile a `rayon` (or hand-rolled) task.
   * Test: same tests, faster.  Add a stress-test scene
     and assert linear-ish scaling up to memory bandwidth.

8. **R.8 — SIMD pixel quads.**
   * Process 4 (or 8) pixels per FS call using `std::simd`.
   * Bespoke + Cranelift fragment shaders need a SIMD entry
     point variant; deferred work in the FS ABI.
   * Aspirational, blocked on the FS-ABI side.

**Sub-phase boundaries vs ABI changes.**  R.1-R.3 need
zero ABI changes — they fit inside the existing
`atrium_vs_main` / `atrium_fs_main` signatures.  R.5
needs the host to know blend state (a `RasterState`
struct).  R.8 needs an FS-side ABI change (vectorised
entry point).  R.4 and R.6/R.7 are internal-only.

**Where the code goes.**  A new
`aqueduct-gpu-host/src/tier2_rasterizer.rs` mirroring
the design in spec §7 (`Rasterizer` struct).  `Tier2Backend`
gains a `rasterizer: Mutex<Rasterizer>` field; `submit_frame`
walks the frame buffer for `FrameOp::Draw`, builds a
`DrawCall`, and dispatches through the rasterizer instead
of jumping to `fill_image_fragment`.  `fill_image_fragment`
stays for the fullscreen-quad case as a fast path (and
for the existing FS-validation tests).

**Status: scoped.**

#### Tier-2 rasterizer — implementation status (2026-05-21)

| Sub-phase | Status                | Commit |
|-----------|-----------------------|--------|
| R.1 hello triangle | ✅ landed | c76357e |
| R.2 perspective-correct varying interpolation | ✅ landed | 96f5e82 |
| R.3 depth buffer (LESS + write) | ✅ landed | 1739d55 |
| R.4 near/far Sutherland-Hodgman clip | ✅ landed | 0f3f8cd |
| R.4 v2 side-plane clip (left/right/top/bottom) | landed   | clip-space Sutherland-Hodgman, +2 tests |
| R.5 blending + colour write mask | ✅ landed | 06a7667 |
| R.6 tiled pixel loop (8×8) | ✅ landed | bb9348e |
| R.6 v2 per-tile edge-function trivial reject | landed   | tile-corner edge test before per-pixel loop |
| R.7 per-stripe parallelism (rayon) | ✅ landed | b72da8a |
| **R.8 SIMD pixel quads** | **deferred — scoped in rasterize_stripe docstring** | — |

The arc is functionally complete at R.7.  The deferred items
(R.4 v2 side-plane clip, R.6 v2 trivial reject, R.8 SIMD) are
real perf / robustness wins but each has its own arc-shape and
none block the next stage (the draw-walker integration that
finally wires Tier2Backend::submit_frame through
fill_image_triangle).

##### R.8 — why deferred

R.8 requires changes well outside the rasterizer itself.  Doing
it properly is bigger than R.1–R.7 combined.  Dependencies, in
likely landing order:

1. **A new vectorised FS ABI.**  `atrium_fs_main_q4(...)` taking
   4-wide arrays of `FragCoord` / varyings / `out_color`, plus a
   flag in `atrium-spv-blob`'s header saying whether the shader
   has this entry point alongside the scalar one.
2. **Vectorised codegen — Cranelift.**  Every scalar IR op
   (`FAdd`, `FMul`, `Dot`, `MatrixTimesVector`, `Load`, `Store`,
   the comparison family, the BranchCond mask path, ...) needs
   a 4-wide variant lowering to NEON / SSE intrinsics.  Mostly
   mechanical but a lot of surface area.
3. **Vectorised codegen — bespoke.**  Same, hand-emitting NEON
   `.4s` (or `.8h` for half-pre cision experiments) on values
   the pack classifier doesn't yet promote.
4. **SPIR-V → vectorised IR.**  Either a second IR-rewrite pass
   (scalar → SIMD lifting) or per-backend scalar → SIMD lifting.
   Need to handle vertex-position-derived divergent control
   flow with a mask register.
5. **Rasterizer pixel-quad gather.**  Process 2×2 pixel blocks
   aligned to even `(px, py)`, with a per-lane mask for partial
   coverage at triangle edges (so a fragment on the outside of
   any edge becomes a masked-off lane that still feeds the FS
   for derivative-correctness but doesn't write pixels).

Each item is a real arc.  Scoping any one of them properly
needs a benchmark-driven motivation — "is the FS the bottleneck
on workloads we actually care about?"  Right now (no real Vulkan
apps running on tier-2 yet) the answer is unknown.  Defer until
profiling on the integrated draw-walker path shows the per-pixel
FS call dominating.

##### What's next after R.7

The natural follow-up is **the draw-walker integration**:
`Tier2Backend::submit_frame` currently still calls
`fill_image_fragment` (whole-image FS fill, no geometry) — the
new `fill_image_triangle` API exists but is only exercised by
unit tests.  Wire `FrameOp::Draw` opcodes through to per-
primitive `fill_image_triangle` calls; plumb vertex / index
buffers from the wire protocol; respect the bound pipeline's
viewport + scissor + raster / depth / blend state.

#### Next big arc — scoped: tier-2 draw-walker integration

Today `Tier2Backend::submit_frame` walks the frame stream
looking only for `FrameOp::BindPipeline` and treats every
matched pass as a "fullscreen FS fill" via
`fill_image_fragment`.  Geometry opcodes (`BindVertexBuf`,
`BindIndexBuf`, `Draw`, `DrawIndexed`, `SetViewport`) are
ignored — their wire bodies aren't even *defined* yet (the
`FrameBuilder::push(FrameOp::Draw, &[0xCC; 16])` tests in
`aqueduct-gpu/src/frame.rs` only round-trip arbitrary bytes).

Tier-1's `SoftwareBackend` is in the same boat: it returns
`UnsupportedFrameOp` for everything beyond `BeginRenderPass /
EndRenderPass / BindPipeline / PushConstants / SetScissor /
Draw`.  Tier-1's `Draw` is a "clear-the-image-with-the-bound-
colour" hack — no vertices, no rasterization.

So this arc has TWO consumers (tier-1 and tier-2) and the
wire-format work is **the contract** between guest, daemon,
and renderers.  Scoping carefully so the format choices are
forwards-compatible with the real Vulkan-shape we want
long-term.

**Phasing.**

* **D.1 — Wire body layouts.**  Define typed structs for
  `BindVertexBuf` (binding, buffer_id, offset), `BindIndexBuf`
  (buffer_id, offset, index_type), `Draw` (vertex_count,
  instance_count, first_vertex, first_instance) and
  `DrawIndexed` (index_count, instance_count, first_index,
  vertex_offset, first_instance), plus `SetViewport`
  (x, y, width, height, min_depth, max_depth).  Each gets
  `to_bytes`/`from_bytes` helpers in `aqueduct-gpu/src/frame.rs`
  + round-trip tests.  No execution wiring; just the contract.
* **D.2 — Buffer storage on `Tier2Backend`.**  Mirror the
  existing `image_created` / `image_destroyed` pattern for
  vertex / index buffers.  A `buffers: Mutex<HashMap<ResourceId,
  Vec<u8>>>` field; hooks into the existing
  `OP_GPU_BUFFER_CREATE` / `OP_GPU_BUFFER_WRITE` session
  opcodes.
* **D.3 — Frame-walker state machine.**  `submit_frame` walks
  the decoded ops and maintains per-pass state: bound pipeline,
  bound vertex buffer slots (with offsets), bound index buffer,
  viewport, scissor.  On `Draw` / `DrawIndexed`, the assembled
  state plus the bound `Tier2ShaderId`-pair fires
  `fill_image_triangle` per primitive.
* **D.4 — Vertex input layout from the pipeline.**  Each
  `Tier2Pipeline` carries a `VertexInputState` describing the
  per-binding stride + attribute (location, format, offset)
  table.  D.3's frame-walker slices the bound vertex buffers
  into per-vertex attribute bytes per the layout.
* **D.5 — Hello-triangle through the wire.**  End-to-end test:
  build a SPIR-V VS + FS pair, register via `Tier2Registry`,
  create vertex buffer + pipeline via wire opcodes, drive a
  3-vertex `Draw` through `submit_frame`, read back the pixel
  buffer.  This is what proves the integration works for a
  real guest path (atrium-vk-icd + frescod) without unit-test
  scaffolding.
* **D.6 — Pipeline state plumbing.**  Map the pipeline's
  raster state (depth test on/off, blend state, write mask)
  into `DrawTriangle.depth_buffer` and `DrawTriangle.blend_state`.
  Optional `depth_buffer` allocation per render-target.
* **D.7 — Multi-primitive draws.**  D.5's hello-triangle is
  one primitive (3 verts).  Real meshes do `vertex_count = N*3`
  for a triangle list.  Iterate primitives within `Draw`;
  same for `DrawIndexed`.
* **D.8 — DrawIndexed.**  Wire the index buffer slicing + per-
  primitive vertex gather.

Roughly D.1-D.5 are the path to a first end-to-end pixel; D.6
through D.8 round out the Vulkan-shape draw semantics.  After
D.8, tier-2 is genuinely a Vulkan executor for the subset of
opcodes we've defined.

**Status: D.1-D.8 landed (commits b464167..D.8).**  The
tier-2 software renderer is now a Vulkan-shape draw executor
for the wire opcodes we've defined: typed bodies, buffer
storage, frame-walker state machine, vertex-input layout +
assembly, hello-triangle through the wire, depth + blend
from pipeline state, multi-primitive Draws, and DrawIndexed
(uint16 + uint32, with `vertex_offset`).  Aqueduct-gpu lib
has 28 tests; the host's tier2_backend integration test
file is 19.  Next steps belong to a different arc: tier-2 R
deferred items (R.4 v2 side-plane clipping, R.6 v2 tile
trivial reject, R.8 SIMD pixel quads), or atrium-vk-icd
migration to consume this wire shape.
