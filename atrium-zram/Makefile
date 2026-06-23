# atrium-zram — compressed-RAM swap codec core. Built IN the VM (KBI-bound to the
# running kernel). Calls the kernel's in-tree zstd (ZSTD_* global symbols, ZSTDIO).
#
#   cd /root/zram-build && make && kldload ./atrium_zram.ko
#   sysctl kern.zram.selftest

KMOD=	atrium_zram
SRCS=	atrium_zram.c

.include <bsd.kmod.mk>
