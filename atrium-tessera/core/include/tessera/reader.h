/*
 * tessera/reader.h — read-only path/content reader over a block_io.
 *
 * Orchestrates the read side (path walk -> inode -> manifest -> chunks ->
 * bytes; directory enumeration) on top of the volume / btree / manifest /
 * pack primitives, with no dependency on a kernel VFS or a userland libc
 * beyond malloc/free/memcpy. This is what the FreeBSD loader's Tessera
 * fs_ops is built on (stand/libsa), and what tools use to read a volume
 * without mounting it.
 *
 * Read-only + hash-free: blobs are located by hash COMPARISON (memcmp),
 * never re-hashed, so a volume of any content-hash algorithm (SHA-256 or
 * BLAKE3) is readable without linking a hash implementation.
 */
#ifndef TESSERA_READER_H_
#define TESSERA_READER_H_

#include "tessera/btree.h"   /* tessera_block_io_t */

#ifdef __cplusplus
extern "C" {
#endif

typedef struct tessera_reader tessera_reader_t;

/* Optional bulk-read callback: read `count` contiguous 4 KiB sectors from
 * `sector` into `out`. Returns 0 on success. A backend that can satisfy a
 * whole pack extent in one I/O (e.g. a multi-block dv_strategy) makes large
 * reads much faster than one round-trip per sector. read_run falls back to
 * per-sector read_block if a bulk read fails, so correctness never depends
 * on it. */
typedef int (*tessera_read_blocks_fn)(void *ctx, uint64_t sector,
                                       uint32_t count, uint8_t *out);

/* Open a volume for reading. `io` needs a working read_block; alloc/free
 * are never used. Returns NULL on a bad/unreadable superblock. */
tessera_reader_t *tessera_reader_open(const tessera_block_io_t *io);

/* As tessera_reader_open, plus an optional bulk-read fast path. */
tessera_reader_t *tessera_reader_open_ex(const tessera_block_io_t *io,
                                         tessera_read_blocks_fn read_blocks);

void              tessera_reader_close(tessera_reader_t *);

/* The root directory inode number. */
uint32_t tessera_reader_root_ino(const tessera_reader_t *);

/* Fetch an inode's core fields. Returns 0, or TESSERA_ENOENT. */
int tessera_reader_stat_ino(tessera_reader_t *, uint32_t ino,
                            uint32_t *out_mode, uint64_t *out_size);

/* Resolve an absolute path ("/boot/kernel") to an inode. Follows the
 * directory tree from the root; no symlink following (v1). Fills any
 * non-NULL out. Returns 0, TESSERA_ENOENT, or TESSERA_EIO. */
int tessera_reader_lookup(tessera_reader_t *, const char *path,
                          uint32_t *out_ino, uint32_t *out_mode,
                          uint64_t *out_size);

/* Read up to `len` bytes of regular-file inode `ino` at byte offset `off`.
 * Short at EOF. *out_read gets the byte count. Returns 0 or TESSERA_EIO. */
int tessera_reader_pread(tessera_reader_t *, uint32_t ino, uint64_t off,
                         void *buf, size_t len, size_t *out_read);

/* Enumerate directory `dir_ino`: fetch the idx-th entry (0-based) into
 * `name_out` (NUL-terminated, capped at name_cap) with its child inode and
 * mode. Returns 0, TESSERA_ENOENT past the end, or TESSERA_EIO. Entry
 * order is the manifest's stored order; stable within one open. */
int tessera_reader_readdir(tessera_reader_t *, uint32_t dir_ino, uint32_t idx,
                           char *name_out, size_t name_cap,
                           uint64_t *out_child_ino, uint32_t *out_child_mode);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_READER_H_ */
