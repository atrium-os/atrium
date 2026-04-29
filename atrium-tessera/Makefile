# atrium-tessera top-level orchestrator (BSD make).
#
# Builds the C core + kmod + Rust workspace, runs smoke tests.
# Each subsystem has its own Makefile; this just delegates.

CORE_DIR=   ${.CURDIR}/core
KMOD_DIR=   ${.CURDIR}/kmod
RS_DIR=     ${.CURDIR}/rs

.PHONY: all core kmod rs check check-core check-rs clean

all: core kmod rs

core:
	${MAKE} -C ${CORE_DIR}

kmod: core
	${MAKE} -C ${KMOD_DIR}

rs:
	cd ${RS_DIR} && \
	    TESSERA_CORE_LIB=${CORE_DIR} \
	    TESSERA_CORE_INCLUDE=${CORE_DIR}/include \
	    cargo build --release

check: check-core check-rs

check-core:
	${MAKE} -C ${CORE_DIR} check

check-rs:
	cd ${RS_DIR} && cargo test --workspace

clean:
	${MAKE} -C ${CORE_DIR} clean
	${MAKE} -C ${KMOD_DIR} clean
	cd ${RS_DIR} && cargo clean
