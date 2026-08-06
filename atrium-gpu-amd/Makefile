# atrium-gpu-amd — §4.1 three-module split. CROSS-BUILD ON THE HOST; the VM is
# for running only. Sources stay in this directory; each subdir is just a
# Makefile that .PATH's back here.
#
#   FBSD_SRC=$PWD/freebsd-src/usr/src OBJDIR=$PWD/.bootstrap-state/fbsdobj
#   (cd $FBSD_SRC && env MAKEOBJDIRPREFIX=$OBJDIR python3 tools/build/make.py \
#       TARGET=arm64 TARGET_ARCH=aarch64 buildenv \
#       BUILDENV_SHELL="/bin/sh -c 'cd <this dir> && make -j8 SYSDIR=$FBSD_SRC/sys'")
#
# ★ This header used to say "built IN the FreeBSD VM ... never cross-compile".
# That has been false since the host cross path landed, and it actively costs
# time: it sends you to the slower path that also has the stale-9p-build trap
# (RUNBOOK §"Stale 9p builds"). All three modules cross-build in one pass, same
# buildenv the tessera kmod uses. KBI still binds a module to its kernel — so
# build against the SAME freebsd-src the running kernel came from, which the
# recipe above does by construction.
#
#   base/    atrium_gpu_amd_pci.ko     — PCI bring-up + BARs + MSI-X + shared softc
#   gpu/     atrium_gpu_amd.ko         — compute/render, /dev/atrium-gpu0  (DEPEND pci)
#   display/ atrium_gpu_amd_display.ko — display engine, /dev/atrium-display0 (DEPEND pci)
#
#   cd /mnt/host/atrium-gpu-amd && make
#   kldload base/atrium_gpu_amd_pci.ko gpu/atrium_gpu_amd.ko \
#           display/atrium_gpu_amd_display.ko
#
# Requires the device present: boot the VM with `run-vm.sh --gpusim`.

SUBDIR=	base gpu display

.include <bsd.subdir.mk>
