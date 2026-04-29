# tessera-core Makefile (BSD make).
#
# Builds libtessera_core.a (static) and libtessera_core.so.1 (shared).
# Run `make check` to build and run the smoke test.

LIB=        tessera_core
SHLIB_MAJOR= 1

SRCS=       error.c hash.c crc.c codec.c cdc.c btree.c \
            manifest.c pack.c journal.c extent.c gc.c
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
            include/tessera/gc.h
INCSDIR=    /usr/local/include

WARNS?=     6
CFLAGS+=    -O2 -fno-strict-aliasing
CFLAGS+=    -I${.CURDIR}/include
CFLAGS+=    -I${.CURDIR}/src

# libmd provides SHA-256 in userspace (auto-dispatches HW SHA-2 / SHA-NI).
LIBADD=     md

.PATH: ${.CURDIR}/src

# ── Test target ──────────────────────────────────────────────────
TEST_SRCS=  tests/test_smoke.c
TEST_BIN=   ${.OBJDIR}/test_smoke

check: ${TEST_BIN}
	${TEST_BIN}

${TEST_BIN}: lib${LIB}.a ${.CURDIR}/tests/test_smoke.c
	${CC} ${CFLAGS} -o ${.TARGET} ${.CURDIR}/tests/test_smoke.c \
	    ${.OBJDIR}/libtessera_core.a -lmd

# ── SHA-256 hardware-dispatch bench ─────────────────────────────
BENCH_SHA_BIN= ${.OBJDIR}/bench_sha256

bench-sha: ${BENCH_SHA_BIN}
	${BENCH_SHA_BIN}

${BENCH_SHA_BIN}: lib${LIB}.a ${.CURDIR}/tests/bench_sha256.c
	${CC} ${CFLAGS} -o ${.TARGET} ${.CURDIR}/tests/bench_sha256.c \
	    ${.OBJDIR}/libtessera_core.a -lmd

CLEANFILES+= ${TEST_BIN} ${BENCH_SHA_BIN}

.include <bsd.lib.mk>
