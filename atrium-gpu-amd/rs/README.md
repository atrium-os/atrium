# atrium-gpu-amd: the Rust front-end

`atrium_gpu_amd.ko` is now part Rust, part C. Rust is the default for new kernel code
(decision D2, `kmod-rust-spike/README.md`). This directory holds the part that has moved: the
layer that faces clients. The hardware half stays C for now.

## The split

| Rust (`rs/`) | C (`../*.c`) |
|---|---|
| ioctl dispatch for the client node and the ctl node, and per-open client state (`ioctl.rs`) | PCI attach, firmware, GMC, IH, MES, the CP rings, VRAM, reset, power gating |
| GPU address spaces as fds: the VA arena and the VM's lifetime (`vm.rs`) | the hardware side of a VM: VMID, page tables, PTE writes (`vm.c`) |
| buffer objects as fds: binding into VMs, unbinding, deferred free (`bo.rs`) | DMA pages and VRAM backing, scanout export (`bo.c`) |
| timeline syncobjs: wait, kqueue, the display's watcher (`syncobj.rs`) | the EOP interrupt, which signals or fails a syncobj (`irq.c`) |
| doorbell mapping grants: the device pager's fault handler, the grant's lifetime, revoke (`doorbell.rs`, on `kpi::pager`) | queues: ownership, programming, the hang watchdog, and which doorbell a client may map and when it is revoked (`queue.c`) |
| the single place Rust calls C (`backend.rs`) | |

C reaches the Rust objects through a few `#[no_mangle]` exports: `amd_syncobj_*`,
`amd_bo_fget`, `amd_bo_gpu_va`, `amd_bo_free_struct`, and `amd_db_grant_create`/`amd_db_grant_revoke`
(called by `queue.c` under the queue's lock). VM and BO are `#[repr(C)]`, and their
first field is the C backend struct, so a C pointer and a Rust pointer to the same object are
the same address. Syncobjs and clients are opaque to C. The softc is opaque to Rust and is
reached only through the `amd_rs_*` helpers in `../glue.c`. The cdevsw tables are also in
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
which needs LLVM 19. It needs KPI patches 0006 to 0009 (file objects, kqueue, vmem, sleep,
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

- Submit validation, moved into Rust.
- R9700 runs through Remote Control on the Mac, when the GPU is quiet.
- PCI, MSI-X and firmware(9) bindings are deferred until bring-up moves too. Bindings that
  nothing calls would be untested code.
