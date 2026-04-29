KMOD=   fresco
SRCS=   fresco.c
SRCS+=  device_if.h bus_if.h pci_if.h

# Quiet WITNESS / debug-build warnings about unused vars in macro expansions.
CWARNFLAGS+= -Wno-unused-parameter

.include <bsd.kmod.mk>
