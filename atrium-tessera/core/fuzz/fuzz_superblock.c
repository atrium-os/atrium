/* Fuzz the superblock path: tessera_volume_open() and every accessor on the
 * handle it returns.
 *
 * WHY THIS TARGET. The superblock is the root of trust. Every mount begins by
 * reading sectors 0 and 1, choosing between them, and believing the result;
 * from there the roots it names are followed into the btrees. If a corrupt
 * superblock can be accepted, everything downstream inherits the corruption —
 * and #115 and #123 were both, at bottom, a superblock naming a root that was
 * no longer what it claimed.
 *
 * tessera_volume_open() is fuzzed rather than tessera_decode_superblock()
 * directly, because the decoder is only the first half. The half that has
 * actual LOGIC is what follows: the A/B generation comparison that picks which
 * of two valid superblocks wins, and the version / sector_size / incompat /
 * hash_alg gauntlet — including the cross-check that a non-default hash
 * algorithm must carry its incompat bit. That is real decision-making on
 * untrusted fields, and it is what this exercises.
 *
 * BOTH SUPERBLOCK SECTORS ARE STAMPED with magic + a recomputed CRC32, for the
 * reason spelled out in fuzz_btree.c: the decoder gates on both, a mutator
 * cannot produce a valid CRC over bytes it is mutating, and an unstamped run
 * would fail at the gate every time while LOOKING like a clean pass. Whether
 * each sector is stamped is itself driven by a control byte, so the fuzzer can
 * still reach the one-valid, other-invalid, and neither-valid branches of the
 * A/B selection — those branches are the point of having two copies.
 */
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/volume.h"
#include "tessera/crc.h"

#define SECT TESSERA_SECTOR_SIZE

struct img { const uint8_t *base; uint64_t sectors; };

static int
img_read(void *ctx, uint64_t sector, uint8_t *out)
{
	struct img *m = ctx;
	if (sector >= m->sectors) return -1;
	memcpy(out, m->base + sector * SECT, SECT);
	return 0;
}
static int img_write(void *c, uint64_t s, const uint8_t *b)
{ (void)c; (void)s; (void)b; return -1; }
static int img_alloc(void *c, uint64_t n, uint64_t *o)
{ (void)c; (void)n; (void)o; return -1; }
static int img_free(void *c, uint64_t s, uint64_t n)
{ (void)c; (void)s; (void)n; return -1; }

static void
stamp_sb(uint8_t *sb)
{
	const size_t crc_off = offsetof(tessera_superblock_t, crc32);
	memcpy(sb, TESSERA_MAGIC_SUPERBLOCK, 8);
	uint32_t c = tessera_crc32(sb, crc_off);
	memcpy(sb + crc_off, &c, 4);
}

int
LLVMFuzzerTestOneInput(const uint8_t *data, size_t len)
{
	/* 1 control byte + the two superblock sectors. */
	if (len < 1 + SECT * 2) return 0;

	const uint8_t ctl = data[0];
	const uint8_t *body = data + 1;
	size_t body_len = len - 1;

	uint64_t sectors = body_len / SECT;
	if (sectors < 2) return 0;
	if (sectors > 8) sectors = 8;

	uint8_t *img = malloc((size_t)sectors * SECT);
	if (img == NULL) return 0;
	memcpy(img, body, (size_t)sectors * SECT);

	/* Bits 0 and 1 decide which copies are made structurally valid, so all
	 * four A/B combinations are reachable — including neither, which must
	 * be rejected, and both, which exercises the generation comparison. */
	if (ctl & 1) stamp_sb(img);
	if (ctl & 2) stamp_sb(img + SECT);

	struct img m = { img, sectors };
	tessera_block_io_t io = { img_read, img_write, img_alloc, img_free, &m };

	tessera_volume_t *v = NULL;
	if (tessera_volume_open(&io, &v) == TESSERA_OK && v != NULL) {
		/* Read every field the mount path goes on to trust. */
		(void)tessera_volume_total_sectors(v);
		(void)tessera_volume_pack_registry_root(v);
		(void)tessera_volume_meta_reserve_length(v);
		tessera_volume_close(v);
	}
	free(img);
	return 0;
}
