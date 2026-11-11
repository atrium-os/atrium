# atrium_gpu_amd.ko — §4.1 GPU module: compute/render driver, owns
# /dev/atrium-gpu0. Attaches to the "atrium_gpu" child the base (pci) module
# creates; reaches the shared softc via device_get_softc(device_get_parent(dev)).
# Depends on the base module.
KMOD=	atrium_gpu_amd
# The display engine is its own module now (../display/) — see the §4.1 split.
SRCS=	module.c firmware.c gmc.c bo.c cp.c irq.c vm.c glue.c pgate.c queue.c clear.c
SRCS+=	device_if.h bus_if.h pci_if.h

.PATH:	${.CURDIR}/..
CFLAGS+= -I${.CURDIR}/..

# The front-end (client node ioctls, VM/BO/syncobj fd objects) is Rust
# (../rs/README.md, kernel security review 2026-11-07 D2): amd_frontend.rs is
# the crate root, compiled by freebsd-kpi-rs's kmod.rust.mk (sys/rust in the
# source tree; our patches are kmod-rust-spike/kpi-patches;
# scripts/gpu-kmod-rust.sh sets it up and builds). bindgen turns ../rs/sys.h
# into the crate's C declarations, only these items; the softc is opaque
# (declared in ../rs/sys.rs).
RUST_SRCS=	amd_frontend.rs
RUST_BINDGEN_HDR= ../rs/sys.h
RUST_BINDGEN_ALLOW= atrium_amd_(bo|vm|dma_page|syncobj|client)|atrium_gpu_.*|ATRIUM_GPU_.*|ATRIUM_AMD_(BO_MAX_PAGES|BO_MAX_BIND|NHWQ|BO_VA_BASE|VM_MAX_BO|VM_NUM_PT)|atrium_rs_ioc|ATRIUM_RS_IOC_.*|amd_rs_(backend|irqs|syncobj_count|grant_count|cdev_softc)|amd_pending_scrub|amd_hwq_(busy_snapshot|defer_bo|client_close)

# Build identity for the gpu-isolation attestation gate (gpu-isolation.md,
# "Attestation freshness"): the last commit that touched the driver tree, with
# -dirty when it has uncommitted changes, so an attestation can never vouch for
# a build it did not test, and commits elsewhere in the repo don't make it
# stale. Override with `make ATRIUM_BUILD_ID=...` where there is no git (the
# cloud VM builds from a tarball, so pass the host's value).
ATRIUM_BUILD_ID?= ${:!sh ${.CURDIR}/../build-id.sh!}
CFLAGS+= -DATRIUM_BUILD_ID=\"${ATRIUM_BUILD_ID}\"

.include <bsd.kmod.mk>
