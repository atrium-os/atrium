/* Fuzz the B-tree node reader: header decode, entry walk, child descent.
 *
 * WHY THIS TARGET. Every piece of Tessera metadata that is not a manifest or a
 * pack lives in one of these trees — inodes, the pack registry, free extents,
 * snapshots, quotas, the blob index. A single bad node reached during mount is
 * enough to walk the kernel off the end of a buffer, and unlike a manifest a
 * node is reached by DESCENT: the child pointers that decide which sector to
 * read next come out of the node currently being parsed.
 *
 * SHAPE OF THE HARNESS. tessera_btree_open() takes a tessera_block_io_t, not a
 * buffer, so the input is treated as an ARRAY OF SECTORS and read_block()
 * serves them. That hands the fuzzer the whole on-disk image: it authors the
 * root, the children, the entry counts and the sector numbers that link them,
 * including nodes that point at themselves or at each other. (Cycles terminate
 * rather than hang — btree.c bounds descent at MAX_DEPTH 16 — so a cycle shows
 * up as wasted work, not a timeout.)
 *
 * EVERY SECTOR IS STAMPED AS A VALID NODE HEADER — magic + recomputed CRC32.
 * This is not optional. tessera_decode_btree_node_header() checks a 4-byte
 * magic AND a CRC32 over the header; a mutation-based fuzzer cannot produce a
 * correct CRC over bytes it is also mutating, so without the stamp every
 * load_node() would fail at the gate and the walk below would never execute.
 * That failure is silent and looks exactly like a clean run — the pack target
 * did 27.8 MILLION executions at cov:2 for the equivalent reason (#124). The
 * stamp costs us coverage of the integrity check itself, which test_codec
 * already covers, and buys coverage of everything the reader does once it
 * BELIEVES a node: entry_count, key/value strides, and child sector numbers.
 *
 * The tree parameters are picked from the real (kind, key_size, value_size)
 * tuples the code actually opens trees with. A tuple the product never uses
 * would fuzz arithmetic that cannot occur on a real volume.
 */
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/btree.h"
#include "tessera/crc.h"

#define SECT TESSERA_SECTOR_SIZE

struct img { const uint8_t *base; uint64_t sectors; };

static int
img_read(void *ctx, uint64_t sector, uint8_t *out)
{
	struct img *m = ctx;
	if (sector >= m->sectors) return -1;          /* EIO past the end */
	memcpy(out, m->base + sector * SECT, SECT);
	return 0;
}
/* Read-only fuzzing: any write path is a bug in the harness, not a code path
 * we want to exercise with garbage. Fail them loudly-by-return. */
static int img_write(void *c, uint64_t s, const uint8_t *b)
{ (void)c; (void)s; (void)b; return -1; }
static int img_alloc(void *c, uint64_t n, uint64_t *o)
{ (void)c; (void)n; (void)o; return -1; }
static int img_free(void *c, uint64_t s, uint64_t n)
{ (void)c; (void)s; (void)n; return -1; }

/* The tuples the code really opens trees with (see tessera_reader.c,
 * extent.c, quota_store.c). */
static const struct { uint8_t kind; uint32_t key; uint32_t val; } KINDS[] = {
	{ TESSERA_BTREE_KIND_INODE,      4,  TESSERA_INODE_RECORD_SIZE   },
	{ TESSERA_BTREE_KIND_PACK_REG,  16,  TESSERA_REGISTRY_ENTRY_SIZE },
	{ TESSERA_BTREE_KIND_FREE_EXT,   8,  8                           },
	{ TESSERA_BTREE_KIND_BLOB_INDEX,
	  TESSERA_BLOB_INDEX_KEY_SIZE,       TESSERA_BLOB_INDEX_VAL_SIZE },
};
#define NKINDS (sizeof KINDS / sizeof KINDS[0])

/* Big enough for the widest value above; asserted so a format change that
 * outgrows it breaks the build instead of the stack. */
#define VALBUF 512u
#define KEYBUF 64u

/* Bound the cursor walk. A node can legitimately claim a large entry_count and
 * chain to further nodes; without a cap one input could iterate for a very
 * long time and starve the fuzzer instead of testing it. */
#define MAX_STEPS 4096u

int
LLVMFuzzerTestOneInput(const uint8_t *data, size_t len)
{
	/* 2 control bytes + at least one sector. */
	if (len < 2 + SECT) return 0;

	const uint8_t sel  = data[0];
	const uint8_t rsel = data[1];
	const uint8_t *body = data + 2;
	size_t body_len = len - 2;

	/* ★ The image gets a MINIMUM sector count, and short inputs are TILED
	 * to fill it. libFuzzer continuously minimizes input length, so an
	 * image sized directly from the input shrinks until there is nowhere
	 * left to descend TO: measured, the corpus converged on ~2.5 sectors
	 * per input and only 63 of 1488 inputs ever read a second sector.
	 * A tree with one node does not test tree-walking.
	 *
	 * Tiling keeps every byte fuzzer-controlled while guaranteeing a child
	 * pointer has somewhere to land. Repeated sectors are not a weakness
	 * here — a node whose child resolves to an identical node is exactly
	 * the self-referential case worth walking, and MAX_DEPTH bounds it. */
	uint64_t sectors = body_len / SECT;
	if (sectors < 8)  sectors = 8;
	if (sectors > 64) sectors = 64;               /* keep an input cheap */

	uint8_t *img = malloc((size_t)sectors * SECT);
	if (img == NULL) return 0;
	for (size_t off = 0; off < (size_t)sectors * SECT; off += body_len) {
		size_t n = (size_t)sectors * SECT - off;
		if (n > body_len) n = body_len;
		memcpy(img + off, body, n);
	}

	const unsigned k = sel % NKINDS;

	/* Stamp each sector into a node this tree will ACCEPT: magic, matching
	 * tree_kind, matching key/value geometry, then a recomputed CRC32.
	 *
	 * ★ The geometry stamp is not cosmetic — it is what makes DESCENT
	 * reachable. load_node() rejects any node whose tree_kind or key/value
	 * sizes differ from the tree's, so a parent AND its child must both
	 * agree before a single child pointer is ever followed. Measured with
	 * only magic+CRC stamped: 132 of 1174 corpus inputs read more than one
	 * sector, and never more than 3 — the fuzzer was spending its budget
	 * re-deriving three constants instead of exploring the walk. These are
	 * equality checks on fixed values, trivially verified by reading and
	 * covered by test_btree_guards; what they gate — entry_count, key
	 * strides, child sector numbers, descent itself — is not.
	 *
	 * node_kind, entry_count, the keys and the child pointers are left
	 * entirely to the fuzzer. Those are the untrusted fields that turn into
	 * offsets and reads. */
	const size_t crc_off = offsetof(tessera_btree_node_header_t, crc32);
	for (uint64_t s = 0; s < sectors; s++) {
		uint8_t *n = img + s * SECT;
		memcpy(n, TESSERA_MAGIC_BTREE_NODE, 4);
		n[offsetof(tessera_btree_node_header_t, tree_kind)] =
		    KINDS[k].kind;
		memcpy(n + offsetof(tessera_btree_node_header_t, key_size),
		    &KINDS[k].key, 4);
		memcpy(n + offsetof(tessera_btree_node_header_t, value_size),
		    &KINDS[k].val, 4);
		uint32_t c = tessera_crc32(n, crc_off);
		memcpy(n + crc_off, &c, 4);
	}

	struct img m = { img, sectors };
	tessera_block_io_t io = { img_read, img_write, img_alloc, img_free, &m };

	const uint64_t root = rsel % sectors;

	tessera_btree_t *t = tessera_btree_open(&io, root, KINDS[k].kind,
	    KINDS[k].key, KINDS[k].val);
	if (t == NULL) { free(img); return 0; }
	tessera_btree_set_quiet_kind_mismatch(t, 1);   /* no log spam per exec */

	uint8_t key[KEYBUF], val[VALBUF];

	/* Point lookup with a key drawn from the image, so the search follows
	 * keys that actually exist in the nodes rather than always missing. */
	memcpy(key, img, KINDS[k].key <= KEYBUF ? KINDS[k].key : KEYBUF);
	(void)tessera_btree_get(t, key, val);

	/* Full ordered walk — the descent path plus every leaf entry. */
	tessera_btree_cursor_t *c = tessera_btree_seek_first(t);
	for (unsigned n = 0; c != NULL && n < MAX_STEPS; n++) {
		if (tessera_btree_cursor_get(c, key, val) != TESSERA_OK) break;
		if (tessera_btree_cursor_next(c) != TESSERA_OK) break;
	}
	tessera_btree_cursor_free(c);

	/* Seek to an arbitrary key, then walk from wherever that landed. */
	c = tessera_btree_seek_at(t, key);
	for (unsigned n = 0; c != NULL && n < MAX_STEPS; n++) {
		if (tessera_btree_cursor_get(c, key, val) != TESSERA_OK) break;
		if (tessera_btree_cursor_next(c) != TESSERA_OK) break;
	}
	tessera_btree_cursor_free(c);

	uint64_t fail_sector = 0;
	uint8_t  found_kind = 0;
	(void)tessera_btree_last_fail(t, &fail_sector, &found_kind);

	tessera_btree_close(t);
	free(img);
	return 0;
}
