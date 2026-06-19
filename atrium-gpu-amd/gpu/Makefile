# atrium_gpu_amd.ko — §4.1 GPU module: compute/render driver, owns
# /dev/atrium-gpu0. Attaches to the "atrium_gpu" child the base (pci) module
# creates; reaches the shared softc via device_get_softc(device_get_parent(dev)).
# Depends on the base module.
KMOD=	atrium_gpu_amd
# display.c stays here for now: ioctl.c's bring-up DISPLAY_* ioctls on
# /dev/atrium-gpu0 still call it. Increment B moves display.c (and those ioctls)
# into atrium_gpu_amd_display.ko.
SRCS=	module.c firmware.c gmc.c bo.c cp.c irq.c sync.c vm.c ioctl.c pgate.c display.c
SRCS+=	device_if.h bus_if.h pci_if.h

.PATH:	${.CURDIR}/..
CFLAGS+= -I${.CURDIR}/..

.include <bsd.kmod.mk>
