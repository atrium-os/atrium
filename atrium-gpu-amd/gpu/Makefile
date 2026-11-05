# atrium_gpu_amd.ko — §4.1 GPU module: compute/render driver, owns
# /dev/atrium-gpu0. Attaches to the "atrium_gpu" child the base (pci) module
# creates; reaches the shared softc via device_get_softc(device_get_parent(dev)).
# Depends on the base module.
KMOD=	atrium_gpu_amd
# The display engine is its own module now (../display/) — see the §4.1 split.
SRCS=	module.c firmware.c gmc.c bo.c cp.c irq.c sync.c vm.c ioctl.c pgate.c queue.c clear.c
SRCS+=	device_if.h bus_if.h pci_if.h

.PATH:	${.CURDIR}/..
CFLAGS+= -I${.CURDIR}/..

# Build identity for the gpu-isolation attestation gate (gpu-isolation.md,
# "Attestation freshness"): the last commit that touched the driver tree, with
# -dirty when it has uncommitted changes, so an attestation can never vouch for
# a build it did not test, and commits elsewhere in the repo don't make it
# stale. Override with `make ATRIUM_BUILD_ID=...` where there is no git (the
# cloud VM builds from a tarball, so pass the host's value).
ATRIUM_BUILD_ID?= ${:!sh ${.CURDIR}/../build-id.sh!}
CFLAGS+= -DATRIUM_BUILD_ID=\"${ATRIUM_BUILD_ID}\"

.include <bsd.kmod.mk>
