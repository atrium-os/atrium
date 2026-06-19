# atrium_gpu_amd_pci.ko — §4.1 base module: PCI bring-up (newbus attach to the
# real `pci` bus), BAR mapping, MSI-X allocation, the shared softc, and the two
# child devices the gpu + display modules attach to. Loaded first.
KMOD=	atrium_gpu_amd_pci
SRCS=	pci.c
SRCS+=	device_if.h bus_if.h pci_if.h

.PATH:	${.CURDIR}/..
CFLAGS+= -I${.CURDIR}/..

.include <bsd.kmod.mk>
