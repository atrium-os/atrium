# libfresco — userspace C library for the Fresco scenegraph protocol.
#
# Native build (FreeBSD):    make
# Cross-compile from macOS:  bmake TARGET=aarch64-unknown-freebsd16.0 \
#                                  SYSROOT=${PWD}/../sysroot
#
# Outputs: libfresco.a, libfresco.so, examples/smoke
# BSD make. (On macOS, install bmake via `brew install bmake`.)

CC?=    cc
AR?=    ar

CFLAGS+=        -O2 -Wall -Wextra -fPIC -std=c11 \
                -Iinclude -Isrc -I../fresco-kmod
LIBS=           -lmd

.if defined(TARGET) && !empty(TARGET)
XFLAGS=         --target=${TARGET} --sysroot=${SYSROOT} -fuse-ld=lld
CFLAGS+=        ${XFLAGS}
LDFLAGS+=       ${XFLAGS}
.endif

OBJS=   src/transport.o src/cas.o src/blob.o src/slot.o src/event.o

EXAMPLES= examples/smoke examples/upload_blob examples/hello_rect examples/event_loop

all: libfresco.a libfresco.so ${EXAMPLES}

libfresco.a: ${OBJS}
	${AR} rcs ${.TARGET} ${OBJS}

libfresco.so: ${OBJS}
	${CC} ${LDFLAGS} -shared -o ${.TARGET} ${OBJS} ${LIBS}

examples/smoke: examples/smoke.c libfresco.a
	${CC} ${CFLAGS} ${LDFLAGS} -o ${.TARGET} examples/smoke.c libfresco.a ${LIBS}

examples/upload_blob: examples/upload_blob.c libfresco.a
	${CC} ${CFLAGS} ${LDFLAGS} -o ${.TARGET} examples/upload_blob.c libfresco.a ${LIBS}

examples/hello_rect: examples/hello_rect.c libfresco.a
	${CC} ${CFLAGS} ${LDFLAGS} -o ${.TARGET} examples/hello_rect.c libfresco.a ${LIBS}

examples/event_loop: examples/event_loop.c libfresco.a
	${CC} ${CFLAGS} ${LDFLAGS} -o ${.TARGET} examples/event_loop.c libfresco.a ${LIBS}

clean:
	rm -f ${OBJS} libfresco.a libfresco.so ${EXAMPLES}

.PHONY: all clean
