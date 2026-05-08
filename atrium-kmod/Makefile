KMOD=   atrium_virtio_gpu
SRCS=   atrium_virtio_gpu.c atrium_trace_kmod.c
SRCS+=  device_if.h bus_if.h pci_if.h vnode_if.h

CWARNFLAGS+= -Wno-unused-parameter

.include <bsd.kmod.mk>
