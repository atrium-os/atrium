# tessera_fs kernel module Makefile (FreeBSD bsd.kmod.mk).
#
# Phase 4 round 1: real file-backed mount; root vnode synthesized.
# Subsequent rounds add vop_lookup over the real inode tree, vop_read
# over manifest+pack walk, mutation paths, and the journal-replay-on-
# mount step.

KMOD=       tessera_fs

# Sources: kmod glue + the tessera-core primitives that don't allocate
# heap memory (so they build cleanly in -nostdinc kernel mode without
# further refactoring).
.PATH:      ${.CURDIR} ${.CURDIR}/../core/src

SRCS=       tessera_fs.c \
            error.c \
            crc.c \
            codec.c \
            btree.c \
            manifest.c \
            hash.c \
            extent.c \
            pack.c \
            journal.c \
            vnode_if.h

CFLAGS+=    -I${.CURDIR}/../core/include
CFLAGS+=    -I${.CURDIR}/../core/src
CFLAGS+=    -DTESSERA_KERNEL=1

.include <bsd.kmod.mk>
