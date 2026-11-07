# Kernel hardening (step 0)

**Status:** built 2026-11-07. Step 0 of
[2026-11-07-freebsd-kernel-security-rust.md](../reviews/2026-11-07-freebsd-kernel-security-rust.md)
(D4). Kernel side in atrium-os/freebsd-src (branch `atrium-os`), host side in this repo.

## 1. What it is

Three parts, each checked by `scripts/kernel-hardening.sh check` on a running guest:

1. **A stripped kernel config.** `sys/conf/std.atrium` holds what every Atrium kernel
   removes; `sys/{arm64,amd64}/conf/ATRIUM` include GENERIC, then `std.atrium`, then drop
   the drivers that platform does not need. Variants: `ATRIUM-NODEBUG` (the one to ship and
   measure), `ATRIUM-KASAN`, `ATRIUM-SYZKALLER` (KASAN + KCOV).
2. **Zero-initialised stack variables**, `makeoptions INIT_ALL=zero` (clang
   `-ftrivial-auto-var-init=zero`). This removes the uninitialised-stack class, 9 kernel
   advisories since 2020. Heap memory still needs `M_ZERO`.
3. **Hardening sysctls**, `etc/sysctl.atrium.conf`, installed as `/etc/sysctl.conf.local`.

## 2. What the config removes, and why

Each line in `std.atrium` names the advisory rows it removes (the review's data file). In
short:

| removed | why |
|---------|-----|
| COMPAT_FREEBSD32 | second copy of every syscall's argument marshalling; Atrium is 64-bit only |
| SYSVSHM, SYSVMSG, SYSVSEM | unused; jails deny them anyway |
| SCTP_SUPPORT, `sctp` module | unused; repeated memory-safety advisories |
| KERN_TLS, TCP_OFFLOAD | TLS is rustls in user space |
| IPSEC_SUPPORT, IPSEC_OFFLOAD | not used by any Atrium component |
| NFS client and server, NFS_ROOT, krpc, kgssapi | unused, large, remote-reachable |
| GEOM_RAID | parses on-disk metadata from any attached disk |
| evdev, uinput, EVDEV_SUPPORT | input is Tactus in user space (atrium-input.md) |
| netmap | raw NIC access, unused |
| linux, linuxkpi modules, fusefs, smbfs, ipfw, netgraph, ctl, cryptodev, vmm, hyperv, mlx | not loadable at all (`WITHOUT_MODULES`), so `kldload` cannot bring them back |

ZFS stays: the dev VM's root is ZFS. amd64 also drops COMPAT_FREEBSD4–10, Xen, legacy ISA and
parallel-port devices, old HBAs, NICs and sound cards (see the file for the list kept).

## 3. Zero-init on amd64

FreeBSD marks `INIT_ALL=zero` broken on amd64 (PR 251083): with it, clang calls `memset` in the
prologue of functions with large locals, and `memset` is an ifunc that `hammer_time()` uses
before `link_elf_ireloc()` resolves it. Our fork fixes this in `amd64/amd64/machdep.c`: when
`KERN_INIT_ALL` is defined (by `kern.mk` whenever INIT_ALL is not `none`), `memset`, `memcpy`
and `memmove` are ordinary functions that branch on `cpu_stdext_feature & CPUID_STDEXT_ERMS`.
That field is zero until CPU identification, so early calls take the `_std` versions, which
work on any CPU. The cost is one load and a predictable branch per call. This is a candidate
to offer upstream once it has booted on real hardware.

## 4. The sysctls

| sysctl | value | effect |
|--------|-------|--------|
| security.bsd.unprivileged_proc_debug | 0 | ptrace, procfs and ktrace of another process are root only |
| kern.elf64.allow_wx | 0 | no mapping is writable and executable at once (the shader JIT maps RW, then RX) |
| security.bsd.see_other_uids / see_other_gids | 0 | a process sees only its own uid's and gid's processes and sockets |
| security.bsd.see_jail_proc | 0 | jailed processes are invisible to unprivileged processes outside |
| security.bsd.unprivileged_read_msgbuf | 0 | dmesg (which can carry kernel addresses) is root only |
| security.bsd.hardlink_check_uid / gid | 1 | no hard links to files the caller does not own |
| kern.randompid | 1 | random PIDs |
| kern.elf64.aslr.* , kern.elf64.nxstack | 1 | already the default; set so `check` catches a regression |
| kern.ipc.tls.enable | 0 | KTLS off on a kernel that has it |

## 5. KASLR, PAC, BTI (checked 2026-11-07)

- **Kernel KASLR: none.** FreeBSD links the kernel at a fixed `KERNBASE` on both arm64 and
  amd64. `unprivileged_read_msgbuf=0` and the absence of `/dev/kmem` in jails limit address
  leaks, but a kernel info-leak bug still gives an attacker the layout. Implementing kernel
  KASLR is a large, separate project; not started.
- **arm64 PAC: on.** The kernel builds with `-mbranch-protection=standard` and enables
  pointer authentication at boot (`hw.pac.enable`), for the kernel and for user space.
- **arm64 BTI: on** for the kernel (same flag). Rust kernel code built through the patched
  `kern.rust.mk` now gets `-Zbranch-protection=bti,pac-ret`; upstream freebsd-kpi-rs had BTI
  only.
- **User space ASLR: on** by default for 64-bit binaries.

## 6. How to use it

```sh
# build on the Linux host (RUNBOOK §13), never in the VM
cd ~/freebsd-src && tools/build/make.py ... TARGET=arm64 TARGET_ARCH=aarch64 \
    KERNCONF=ATRIUM-NODEBUG buildkernel
sh scripts/kernel-hardening.sh install
sh scripts/kernel-hardening.sh check
```
