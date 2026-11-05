# atrium_gpu_amd_display.ko — §4.1 display module: the display engine (DCN),
# owns /dev/atrium-display0. Attaches to the "atrium_gpu_amd_display" child the
# base (pci) module creates; reaches the shared softc via
# device_get_softc(device_get_parent(dev)). Depends on the base module,
# independent of the GPU module.
KMOD=	atrium_gpu_amd_display
SRCS=	display_dev.c edid.c sink.c
SRCS+=	device_if.h bus_if.h pci_if.h

# edid.c / sink.c: the EDID parser and connector probe every display driver
# shares (atrium-display/).
.PATH:	${.CURDIR}/.. ${.CURDIR}/../../atrium-display
CFLAGS+= -I${.CURDIR}/.. -I${.CURDIR}/../../include -I${.CURDIR}/../../atrium-display

.include <bsd.kmod.mk>
