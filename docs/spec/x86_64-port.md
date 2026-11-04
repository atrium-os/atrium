# Atrium on x86_64: port scope

**Status:** scope, 2026-11-03. Phase A is done (commit `2280670`). The rest is a plan.
**Why:** the RDNA4 backend (`atrium-rdna4-vulkan-backend.md`) is checked against the
project's R9700, which sits in an x86_64 box running Ubuntu with IPMI. Tier-2 cross-checks
on that box needed Tier-2 to generate x86_64 code (Phase A). Running Atrium itself on the
box, which milestone M5 needs, is the rest of this document.

## 0. Summary

Atrium's code is far less arm64-bound than "only tested on arm64" suggests. A sweep for
`aarch64`, `arm64`, `__aarch64__`, inline asm and cache maintenance outside the Tier-2
compiler turns up build plumbing (triples, sysroots, QEMU scripts), one kernel SIMD path
(Tessera's BLAKE3 NEON), and manifest `arches` lists. The kernel modules use portable
FreeBSD KPIs (`bus_dma`, `bus_setup_intr`, `BUS_DMA_COHERENT`). Most of the port is plumbing.

The big job is not an architecture problem. It is **`atrium-gpu-amd` on real silicon.**
The driver has only ever run against gpusim, which models the PSP loading CP microcode
rather than doing it. A real R9700 needs PSP/SMU firmware bring-up, IP discovery, real
MES firmware, DCN display init, MSI-X, the IOMMU (Intel VT-d on this box) and resizable BAR. That work is the
same on any CPU. x86_64 is just where the card is.

| Phase | What | State |
|---|---|---|
| A | Tier-2 generates and runs x86_64 code | **Done** |
| B | Userspace oracle stack on Ubuntu x86_64 (the R9700 box) | Nearly done |
| C | FreeBSD amd64 VM: kernel modules, cross-build, scripts | Plumbing, ~1–2 weeks of threads |
| D | `atrium-gpu-amd` on the real R9700 (VFIO passthrough first) | The big one |
| E | Native boot on the box over IPMI | After D is stable |

## 1. Phase A — Tier-2 on x86_64 (done)

The Tier-2 compiler had two backends. `atrium-spv-compile` tries the **bespoke** backend
first. It emits AArch64 directly through `pptk-codegen-arm64` and stays AArch64-only by
design. **Cranelift** is the fallback when bespoke returns `Unsupported`. On x86_64 hosts,
Cranelift is now the only backend:

- `Target::X86_64Linux` (new) and `X86_64FreeBSD`. Both ISAs are built in on every host.
- x86-64-v3 baseline flags (SSE4.1, AVX2, FMA, BMI1/2, LZCNT). Without them Cranelift
  lowers `floor`/`ceil`/`trunc`/`nearest`/`fma` to libcalls. Libcalls are relocations,
  and the flat blob format rejects relocations.
- Blob `arch` tag `ARCH_X86_64 = 1`. The loader refuses a blob built for another arch.
- `PackHalf2x16` / `UnpackHalf2x16` now exist in Cranelift (integer IR). They used to be
  bespoke-only, so shaders that used them failed on x86. They are checked exhaustively
  against IEEE `f16`.

Verified on x86_64 Linux: spv-differential 53/53 (interpreter vs Cranelift), the compute
differential 110/110, `aqueduct-gpu-host` tier2_backend/rasterizer/registry (53), and
vk-icd tier2_compute (23). On x86_64 the bespoke-vs-Cranelift comparison drops out, and
the interpreter plus each test's pinned values carry the check. The arm64 Mac still runs
all three.

**Deferred: a bespoke x86_64 backend.** Bespoke exists for speed: ~90% of hand-written
code against Cranelift's ~70%. That matters once frescod renders through Tier-2 on an x86
desktop, not for oracle runs. Benchmark Cranelift-x86 with `bench_fs_codegen` on the
R9700 box first, then decide. The bespoke backend is about 7.4k lines on top of pptk's
arm64 assembler. An x86 port would need an x86 encoder in pptk, too.

## 2. Phase B — userspace on Ubuntu x86_64

Already working: the spv crates, `aqueduct-gpu-host` (it now links on Linux, after the
mach/dyld calls were gated to macOS), and the vk-icd Tier-2 tests.

Remaining:
- `aqueduct-gpu-host` lib tests `include_bytes!` SPIR-V from `bundles/*/build.sh`, which
  needs `slangc`. Install the Slang release on the box. (The cloud container can't reach
  GitHub releases.)
- Two vk-icd failures that predate this work and are not x86-specific:
  `enumerate_instance_extensions_lists_atrium_surface` expects 4 extensions and gets 5,
  and `loader_contract` can't find the cdylib. Cargo's new build-dir layout no longer
  copies it next to test binaries.
- `pptk` must sit beside the repo (`../pptk`) for any crate that depends on the bespoke
  backend, even on x86 where it never runs. Either clone it on the box, or make bespoke
  an optional dependency (Cargo resolves path dependencies even when they're optional,
  so this needs a feature *and* a vendored stub, or a git dependency).

## 3. Phase C — FreeBSD amd64

### 3.1 Toolchain and cross-build
- `x86_64-unknown-freebsd` is a Rust **Tier-2** target with prebuilt std, so it needs no
  `build-std`. Add a `[target.x86_64-unknown-freebsd]` section to `.cargo/config.toml`
  (linker, sysroot, CRT objects, like the aarch64 one). Keep `build-std` limited to the
  aarch64 target.
- Sysroot: pull it from an amd64 guest the way `cloud-vm.sh` does for arm64.
- C dependencies (`ring` in navigator-fetch, libfresco): `CC_x86_64_unknown_freebsd`,
  following the same pattern as aarch64.
- `scripts/cross-build-core.sh`: `TRIPLE` already accepts an override. Default it from `ARCH`.

### 3.2 Kernel modules
All of them build with the stock FreeBSD kmod framework, `TARGET=amd64`. Known items:
- **Tessera**: BLAKE3 NEON is aarch64-only. amd64 falls back to the portable C path, which
  works, just slower. Later, add the SSE4.1/AVX2 BLAKE3 kernels with `fpu_kern_enter`.
  The `#ifdef __aarch64__` FPU bench is arm64-only. The Makefile's stack-overflow note is
  arm64-specific. amd64 has a guard page.
- `atrium-kmod/atrium_virtio_gpu.c` uses `pmap_kextract`. It's legacy ('G' ABI), so it
  can stay arm64-only or be dropped.
- `atrium-gpu-amd`, `fresco-kmod`, `carillon-kmod`, `atrium-zram`, `memfed`: no
  arch-specific code was found. Build them for amd64 and run their existing VM tests.

### 3.3 VM scripts
- `run-vm.sh` and `cloud-vm.sh` hard-code `qemu-system-aarch64`, edk2-aarch64 and the
  arm64 image. Add `ARCH=amd64`: `qemu-system-x86_64 -M q35`, OVMF, the FreeBSD amd64
  snapshot. Most of the ~20 `vm-*.sh` soak and crash scripts only ssh into the guest and
  should work as they are.
- **gpusim**'s QEMU side is a thin pure-C PCI device talking to the Rust model over a Unix
  socket. It is arch-neutral, so build it into `qemu-system-x86_64` as well.
- Speed: the cloud container has no KVM, so an amd64 guest there is TCG too (functional
  tests only, like arm64 today). **The R9700 box has KVM**, which makes it the fast amd64
  dev VM.

### 3.4 Packaging and manifests
- Portcullis manifests list `arches = ["aarch64-freebsd", ...]`. Add `x86_64-freebsd` to
  the schema's known set and to the bundles that should ship on x86.
- Insula/pkg artifact names and the jail-tree distribution: check that they carry the
  arch. Same for the `/var/atrium/shaders/v{N}` cache: blobs carry an arch tag now, but
  the cache key does not. One machine runs one arch, so that's fine, but a shared cache
  across arches would mix them.

### 3.5 Status (2026-11-04)
Working in the cloud amd64 VM (`ARCH=amd64 sh scripts/cloud-vm.sh up`):
- **Guest CPU**: under TCG use `-cpu Nehalem`. With `-cpu max` the stock kernel panics in
  `zfs_sha256_transform_avx2`.
- **Atrium kernel**: the fork's amd64 GENERIC (it already has `SCHED_LAMINAR`) cross-builds on
  Linux with `tools/build/make.py TARGET=amd64`. It boots, and `kern.sched.name="Laminar"` is
  the default. It needed two amd64 build fixes in the fork, now on the fork's atrium-os branch (204a080df):
  - `thread_va_ok` uses `VIRT_IN_DMAP`, because amd64 has no `DMAP_{MIN,MAX}_ADDRESS`.
  - `sched_laminar.c` includes `<vm/pmap.h>` for `kva_layout`.
- **Modules from the host cross-build need LLVM 19** (`--cross-bindir=/usr/lib/llvm-19/bin`).
  With clang 18, `__stack_chk_guard` is reached through GOTPCRELX relocations (type 42), which
  the kernel linker rejects. zfs.ko then fails to load and root mount fails.
- **Tessera EFI loader**: build it on the host, not in the TCG guest, where it takes 30+ minutes
  and still hadn't finished. On the host it takes about 2 minutes: inside `make.py buildenv`,
  build `stand/{libsa,liblua,efi/libefi,efi/loader_lua}` with
  `LOADER_TESSERA_SUPPORT=yes TESSERA_SRCTOP=... WITHOUT_LOADER_IA32=yes`.
- **Tessera root on amd64 works**:
  - Build the disk with `scripts/build-tessera-devroot.sh` (D=vtbd1, EXCLUDE_OBJ=1).
    tessera-fsck reports CLEAN.
  - Boot it with `TESSERA_ROOT=1 ARCH=amd64 sh scripts/cloud-vm.sh up`. / mounts as tessera,
    the BLAKE3 self-test passes on the portable path, and the scheduler is Laminar.
  - A 64 MiB write and sync works, with no WITNESS lock-order reports.
- Portcullis cross-builds (`TARGET=x86_64-unknown-freebsd`). All 6 kmod trees build in the guest.
- **Navigator jail e2e passes on amd64 on the Tessera root**: 1 document, 1 jail, no leaks, all
  refusals intact. It took two fixes, both now in `build-tessera-devroot.sh`:
  - Filesystem modules (nullfs, unionfs, p9fs, ...) must come from the fork tree. Stock
    modules fail with "version mismatch" and jails die at mount_nullfs.
  - The devroot keeps the source rc.conf's Portcullis settings (jaild enables, devfs_rulesets,
    pf). Without them, jaild is down and devfs ruleset 22 is empty.
- **kmods on amd64** (built in the guest against the fork tree):
  - zram, fresco, carillon and gpu-amd (pci/gpu/display) all load and unload cleanly under
    WITNESS.
  - `kern.zram.selftest` is OK for both zstd and lz4.
  - fresco and carillon need their ivshmem host endpoints, and gpu-amd needs a device (gpusim or
    the R9700), so none of those attach yet.
- WITNESS reported one LOR during the jail e2e: tessera -> tmpfs at tmpfs_unmount's vflush,
  then tmpfs -> tessera at unionfs_nodeget -> null_lock on exec. This is the generic cross-mount
  unionfs pattern (tmpfs upper over nullfs-of-root lower). On a ZFS root it would read
  zfs -> tmpfs, so it isn't Tessera-specific. Worth confirming against an arm64 WITNESS run.
- Open: `pfctl -s rules` prints `DIOCOSFPGET: Operation not supported`. This looks like an
  ioctl mismatch between the stock 16-CURRENT userland and the fork kernel. Building world
  from the fork would settle it.
- Still to do: kmod selftests on amd64, navigator e2e on amd64, and gpusim in
  `qemu-system-x86_64` (atrium-gpu-amd has no device to attach to until then).

## 4. Phase D — `atrium-gpu-amd` on the real R9700

**Recommended path: PCI passthrough (VFIO) into a FreeBSD amd64 VM on the Ubuntu box,
before any native boot.** The card goes to the guest, while Ubuntu, the IPMI fallback and
the other users of the box stay untouched. A driver hang costs a VM restart, not a remote
power cycle. This needs IOMMU on in firmware and on the kernel command line, and the card
bound to `vfio-pci` while Atrium uses it. The box is shared, so that is the user's call.

What the driver has to grow, roughly in bring-up order (the gpusim model abstracts most of it):
1. IP discovery table, which gives real register bases instead of gpusim's fixed map.
2. PSP bootloader and signed firmware loading (SOS, then TA, SMU, RLC, CP, MES, SDMA,
   from linux-firmware's `gc_12_*` / `psp_14_*` blobs). gpusim only models this.
3. SMU: clocks, power-up, the power-gating messages (`pgate.c` exists against the model).
4. GMC/VM on real hardware: the VRAM aperture, ReBAR for 32 GB, page-table formats checked
   against silicon.
5. MES with real firmware (`cp.c`'s queue model), interrupts through the IH ring on MSI-X.
6. DCN display. Once IPMI's BMC VGA isn't the console, DCN bring-up gives scanout on 'D'.
7. IOMMU: the box is Intel (Xeon E-2288G), so VT-d, which FreeBSD drives with `dmar(4)` (`hw.dmar.enable=1`). ABI v2.1 relies on the GPU MMU plus the IOMMU for DMA isolation, and the
   review's five isolation breaks have to be fixed before this runs on shared hardware.

Each step feeds back into gpusim: whatever silicon does differently from the model becomes
a gpusim fix and a regression test. The RDNA4 backend spec already plans this loop.

## 5. Phase E — native boot over IPMI

The box has a single disk that Ubuntu owns, so the native boot never touches it:
**netboot FreeBSD amd64 over PXE (iPXE) with an in-memory root** (`mfsroot`, or root on
NFS). Scratch data goes to the project's S3 storage. Power-cycling over IPMI, or a
one-shot PXE boot flag in the BMC, returns the box to Ubuntu from its disk. That beats
copying the disk off to S3 and restoring it: there's nothing to restore, and no
failure mode where the box is left without Ubuntu. Console over IPMI SOL. Then run M5:
frescod compositing on the R9700 through `tier3_rdna4`.

## 6. Box facts and open questions

- CPU: Xeon E-2288G (Coffee Lake, 8 cores). AVX2, FMA, BMI and LZCNT are present, so
  Phase A's x86-64-v3 baseline holds. No AVX-512, which nothing assumes.
- Disk: one, Ubuntu's. Native boot is PXE plus an in-memory root (§5); scratch data goes to S3.
- IOMMU/VFIO (Phase D): **ready** (checked 2026-11-04). DMAR is active with interrupt
  remapping, `/dev/kvm` exists, and IOMMU group 1 holds exactly the R9700's own functions:
  its internal switch ports `01:00.0`/`02:00.0`, the GPU `03:00.0` [1002:7551] and HDMI
  audio `03:00.1` [1002:ab40]. The BMC's ASPEED VGA (`08:00.0`) stays Ubuntu's console.
  Binding group 1 to `vfio-pci` takes the card from amdgpu, so Linux oracle runs and the
  FreeBSD guest take turns (rebind through sysfs or `driverctl`, no reboot). Unknown:
  whether RDNA4 resets cleanly between guest runs.
