# Carillon guest kmod. Build in the FreeBSD VM (or cross against the
# FreeBSD sysroot) — not on the macOS host:
#
#   (in VM)  cd /mnt/host/carillon-kmod && make && kldload ./carillon.ko
#
# Pairs with the host endpoint in aqueduct-gpu-host (IvshmemServer) and
# QEMU launched via `scripts/run-vm.sh --carillon`. See docs/spec/carillon.md.
KMOD=   carillon
SRCS=   carillon.c
SRCS+=  device_if.h bus_if.h pci_if.h

CWARNFLAGS+= -Wno-unused-parameter

.include <bsd.kmod.mk>
