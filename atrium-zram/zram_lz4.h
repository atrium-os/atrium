/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * LZ4 block codec for atrium_zram (zram_lz4.c).
 */
#ifndef _ZRAM_LZ4_H_
#define	_ZRAM_LZ4_H_

/* Bytes of hash table the caller provides to zram_lz4_compress(). */
size_t	zram_lz4_ctxsize(void);
/* Compressed size, or 0 if it does not fit in `osize` bytes. */
int	zram_lz4_compress(void *ctx, const void *src, void *dst, int isize,
	    int osize);
/* Decompressed size, or < 0 on corrupt input. Never writes past `osize`. */
int	zram_lz4_decompress(const void *src, void *dst, int isize, int osize);

#endif
