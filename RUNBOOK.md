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
- **9p share** — host's `~/src/bsd/` is the guest's `/mnt/host/`. Easiest: cross-compile binaries into `~/src/bsd/<crate>/target/...` on host, run them in the VM at `/mnt/host/<crate>/target/...`.
- **scp over the SSH forward** —
  ```sh
  scp -i ~/.ssh/fresco_bsd_ed25519 -P 2222 some-file root@localhost:/root/
  ```

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

#### Booting with the venus paravirt Vulkan stack (D1.x V5+)

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

### MSI-X is broken on FreeBSD/aarch64/qemu+HVF — we use polling instead
- `pci_alloc_msix` returns ENXIO for late-attaching PCI devices (and even some early ones — virtio-blk uses INTx in the boot dmesg). Root cause: qemu's IORT for the `virt` machine is empty (84 bytes header only), so FreeBSD has no PCI→MSI routing. With `gic-version=3` it routes via ITS but IORT misses our requestor; with `gic-version=2` GICv2m attaches but the same IORT issue prevents MSI mapping.
- **`acpi=off` is not a fix** — FreeBSD aarch64 boot panics under FDT mode on qemu virt (`No usable event timer found`, no PSCI). Recovery via `recover-acpi.exp`.
- **Fix:** the kernel module skips `pci_alloc_msix` entirely and runs a 1 ms `callout` (`FRESCO_POLL_HZ`) that reads `comp_write` and `input_write` directly from the shmem control region. When the head pointer changes, KNOTE the kqueue list. Cost: ~0.1% CPU at idle. Mirrors the host-side workaround for the same failing ivshmem doorbell delivery.
- karythra-os doesn't hit this because it bypasses FreeBSD's MSI framework entirely (it's not running FreeBSD) — it programs MSI-X table entries directly to `GICV2M_SETSPI_NS` and configures the GIC distributor by hand. We won't replicate that hack here; polling is fine and matches the "pure FreeBSD" discipline.

### `-machine virt,gic-version=`
- We use `gic-version=2` (matches karythra-os, smallest interrupt model). `gic-version=3` works equally for our purposes since we don't use MSI-X anyway.

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
| `atrium-spv-compile` | Standalone compile binary the loader shells out to. |

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
