# tessera-core Makefile (FreeBSD make).
#
# Builds libtessera_core.a (static) and libtessera_core.so.1 (shared).
#   make            build the library
#   make check      build + run the full Phase-1 test suite
#   make bench-sha  run the SHA-256 hardware-dispatch microbench

# Build in-tree (not under /usr/obj). The 9p share that exposes this
# tree to the macOS host is one-way (host → VM), so keeping the .a
# file inside the source tree is what lets the host's cross-compiled
# Rust crates find it via TESSERA_CORE_LIB.
MK_AUTO_OBJ=no

LIB=        tessera_core
SHLIB_MAJOR= 1

SRCS=       error.c hash.c crc.c codec.c cdc.c btree.c \
            manifest.c pack.c journal.c extent.c gc.c volume.c quota.c
INCS=       include/tessera/tessera.h \
            include/tessera/error.h \
            include/tessera/format.h \
            include/tessera/hash.h \
            include/tessera/crc.h \
            include/tessera/codec.h \
            include/tessera/cdc.h \
            include/tessera/btree.h \
            include/tessera/manifest.h \
            include/tessera/pack.h \
            include/tessera/journal.h \
            include/tessera/extent.h \
            include/tessera/gc.h \
            include/tessera/volume.h \
            include/tessera/quota.h
INCSDIR=    /usr/local/include

WARNS?=     6
CFLAGS+=    -O2 -fno-strict-aliasing
CFLAGS+=    -I${.CURDIR}/include
CFLAGS+=    -I${.CURDIR}/src

# libmd provides SHA-256 in userspace (auto-dispatches HW SHA-2 / SHA-NI).
LIBADD=     md

.PATH: ${.CURDIR}/src

# ── Test suite ──────────────────────────────────────────────────
# Each test is a self-contained binary linking against libtessera_core.a.
TEST_NAMES= \
    test_smoke \
    test_crc \
    test_codec \
    test_cdc \
    test_extent \
    test_btree \
    test_manifest \
    test_pack \
    test_journal \
    test_volume \
    test_quota

TEST_BINS= ${TEST_NAMES:S,^,${.OBJDIR}/,}

# Pattern: ${.OBJDIR}/test_X depends on tests/test_X.c + libtessera_core.a.
.for t in ${TEST_NAMES}
${.OBJDIR}/${t}: lib${LIB}.a ${.CURDIR}/tests/${t}.c
	${CC} ${CFLAGS} -o ${.TARGET} ${.CURDIR}/tests/${t}.c \
	    ${.OBJDIR}/libtessera_core.a -lmd
.endfor

check: ${TEST_BINS}
	@for t in ${TEST_BINS}; do \
	    printf '\n== %s ==\n' $$(basename $$t) ; \
	    $$t || exit 1 ; \
	done ; \
	echo ; \
	echo "all tests passed"

# ── SHA-256 hardware-dispatch bench ─────────────────────────────
BENCH_SHA_BIN= ${.OBJDIR}/bench_sha256

bench-sha: ${BENCH_SHA_BIN}
	${BENCH_SHA_BIN}

${BENCH_SHA_BIN}: lib${LIB}.a ${.CURDIR}/tests/bench_sha256.c
	${CC} ${CFLAGS} -o ${.TARGET} ${.CURDIR}/tests/bench_sha256.c \
	    ${.OBJDIR}/libtessera_core.a -lmd

CLEANFILES+= ${TEST_BINS} ${BENCH_SHA_BIN}

.include <bsd.lib.mk>
