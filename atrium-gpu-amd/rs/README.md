# atrium-gpu-amd: the Rust front-end

`atrium_gpu_amd.ko` is now part Rust, part C. Rust is the default for new kernel code
(decision D2, `kmod-rust-spike/README.md`). This directory holds the part that has moved: the
layer that faces clients. The hardware half stays C for now.

## The split

| Rust (`rs/`) | C (`../*.c`) |
|---|---|
| ioctl dispatch for the client node and the ctl node, per-open client state, and every check on submit's arguments (`ioctl.rs`; the command stream itself is not parsed: app GPU code is confined by its VMID, F6 Option A) | PCI attach, firmware, GMC, IH, MES, the CP rings, VRAM, reset, power gating |
| GPU address spaces as fds: the VA arena and the VM's lifetime (`vm.rs`) | the hardware side of a VM: VMID, page tables, PTE writes (`vm.c`) |
| buffer objects as fds: binding into VMs, unbinding, deferred free (`bo.rs`) | DMA pages and VRAM backing, scanout export (`bo.c`) |
| timeline syncobjs: wait, kqueue, the display's watcher (`syncobj.rs`) | |
| the completions each queue owes, retiring them on EOP, failing them on a stop or fault, and the hang clock (`pending.rs`; guarded by sc->lock through an `ScLocked` token, since the base ISR holds that C lock) | the EOP and VM-fault cause handlers the base ISR calls (`irq.c`), the counters and log lines (`queue.c`) |
| hardware queues: each queue's state behind its own lock, ownership, submit, QUEUE_MAP, the doorbell mmap, waiting for another VM's work, the hang check, reset (`queue.rs`) | queue registers, programming and stopping a queue (`cp.c`), and the small helpers, timers and sysctls (`queue.c`) |
| doorbell mapping grants: the device pager's fault handler, the grant's lifetime, revoke (`doorbell.rs`, on `kpi::pager`) | |
| deferred BO frees: the list and the reaper (`defer.rs`); a BO stays a Rust `Box` until it is freed, C never owns it | the reaper's and watchdog's timers (`queue.c`) |
| per-uid BO caps, kmod audit G2: the accounts, charge at BO_ALLOC, give back at free (`quota.rs`) | the cap sysctls and the caller's uid (`queue.c`, `glue.c`) |
| the single place Rust calls C (`backend.rs`) | |

C reaches the Rust objects through a few `#[no_mangle]` exports: `amd_syncobj_*`,
`amd_bo_fget`, `amd_rs_reap`/`amd_rs_dev_init`/`amd_rs_dev_fini`, `amd_rs_watchdog`, `amd_rs_eop`/`amd_rs_q_faulted_locked` (from the ISR, sc->lock held), and the
queue entry points that keep their C names (`amd_hwq_submit`, `_map`, `_mmap_single`,
`_client_close`, `_vm_gone`, `_reset_prepare`/`_restore`). VM and BO are `#[repr(C)]`, and their
first field is the C backend struct, so a C pointer and a Rust pointer to the same object are
the same address. Syncobjs and clients are opaque to C. The softc is opaque to Rust and is
reached only through the `amd_rs_*` helpers in `../glue.c` and `../queue.c`. The cdevsw tables are also in
`glue.c`, and they point at the Rust entry points.

The display module used to read a syncobj's fields directly. It now asks through the backend
op `syncobj_ready`.

## Where `unsafe` is

- **`backend.rs`**: every FFI call into C. Its methods are safe because they take objects the
  front-end owns. Each depends on the contract written next to its op in `atrium_gpu_amd.h`.
  That trust in the C backend is where the safety argument ends.
- **The `extern "C"` entry points**: the d_* functions in `ioctl.rs`, the kqueue callbacks in
  `syncobj.rs`, and the exports for C. Each one turns a raw pointer back into a reference.
- **`bo.rs`**: handing a BO to C's deferred-free list, where ownership passes to C.

Everything else is safe Rust. That covers lifetimes (a BO holds a `FileRef<Vm>` for each
binding, so a VM cannot go away under a bound BO), locking, and every bounds check on
user-supplied sizes and offsets.

## Audit bugs

G1 (`docs/reviews/2026-11-07-kmod-audit.md`) was two concurrent `VM_BIND`s on one BO writing
past the binding table. The table now lives inside an `SxLock`, so code cannot touch it without
the lock: the type rules the race out, rather than a convention. V1 to V5 are in the
virtio-gpu driver, which this port leaves alone.

## Build

```sh
. /home/user/work/env.sh          # SRC=<fork's tree> OBJ=<objdir>
sh scripts/gpu-kmod-rust.sh prepare   # once: kpi-rs + kmod-rust-spike/kpi-patches into $SRC/sys/rust
sh scripts/gpu-kmod-rust.sh build     # KERNCONF=ATRIUM-NODEBUG by default
```

The modules go to `atrium-gpu-amd/out/<target>-<kernconf>/`. The script defaults to amd64,
which needs LLVM 19. It needs KPI patches 0006 to 0010 (file objects, kqueue, vmem, sleep,
cdevpriv, and per-module bindgen). `gpu/Makefile` sets `RUST_SRCS`, `RUST_BINDGEN_HDR` and
`RUST_BINDGEN_ALLOW`. The bindgen input is `rs/sys.h`.

## Results (gpusim, amd64, QEMU TCG, 2026-11-11)

| kernel | atrium_gpu_test | test_gpu_isolation |
|---|---|---|
| ATRIUM-NODEBUG | ALL OK | 71/71, ALL OK (part F: 9216 MiB VRAM churn, no ENOMEM, no scrub failure) |
| ATRIUM-KASAN (KASAN + INVARIANTS + WITNESS) | ALL OK | 71/71, ALL OK; no KASAN report, no WITNESS report from the driver |

The first KASAN run panicked in `amd_rs_open`. The cause was not the driver: KPI patch 0004
had set the KASAN shadow offset for arm64 only, so on amd64 every instrumented Rust function
faulted on its first stack poison. Patch 0008 fixes it.

WITNESS also flagged the C reset path (`amd_hwq_reset_prepare` takes every queue lock, in
index order). The queue locks are now `SX_DUPOK`.

The doorbell grants moved later the same day; both kernels gave the same results after that,
and `grants_live` returned to 0.

The all-C module from before the port gives the same results on the same kernel (re-run the same day). Neither version's PCI module unloads (EBUSY), so reload means reboot, as before. The module
grows from 78 KB to 189 KB, mostly Rust's libcore pieces.

## Next

- R9700 runs through Remote Control on the Mac, when the GPU is quiet.
- PCI, MSI-X and firmware(9) bindings are deferred until bring-up moves too. Bindings that
  nothing calls would be untested code.
