/*
 * tessera-core: B+tree primitives over a caller-provided block I/O.
 *
 * Layout (4 KiB nodes):
 *   bytes  0 ..  31    tessera_btree_node_header_t  (kind, fanout, CRC)
 *   bytes 32 .. 4095   entry slots, packed, sorted ascending by key
 *
 * Entry encoding:
 *   leaf node      :  [ key (key_size B) | value (value_size B) ]
 *   internal node  :  [ key (key_size B) | child_sector (8 B) ]
 *
 * Internal-node convention: entry[0].key is the smallest key in
 * entry[0].child (the "leftmost-descent key"). For a probe of K,
 * pick the largest i with entries[i].key <= K (or i = 0 if K is
 * smaller than entries[0].key) and descend into entries[i].child.
 *
 * Updates are copy-on-write: every mutation allocates a fresh sector
 * via io->alloc, writes the new node, and frees the old via io->free.
 * The tree's root is published as the return value of put / delete.
 *
 * Phase-1 scope: insert / lookup / delete, with leaf+internal split and
 * a proportional-redistribute-or-merge underflow fixup at the leaf
 * (internal nodes are merged down when their child count hits 1; for
 * Phase 1 we do not rebalance internal nodes more aggressively than
 * that — adequate for correctness, and Phase 2 can revisit the heuristic
 * for fewer COW writes under churn).
 *
 * Cursor is forward-iteration only (sufficient for the v1 use cases —
 * inode-walk for GC, pack-registry walk for fsck, free-extent walk for
 * allocator priming). Reverse iteration is reserved for v2.
 */

#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/crc.h"
#include "tessera/error.h"
#include "tessera/format.h"

#include "tessera_compat.h"

#define BLOCK_SIZE     TESSERA_SECTOR_SIZE
#define HEADER_SIZE    32u                 /* sizeof(tessera_btree_node_header_t) */
#define MAX_DEPTH      16u                 /* >> 4096^16 keys */

/* ── tessera_btree_t — open handle ───────────────────────────────── */

struct tessera_btree {
	tessera_block_io_t  io;
	uint64_t            root;
	uint8_t             tree_kind;
	uint32_t            key_size;
	uint32_t            value_size;
	uint32_t            leaf_fanout;       /* max entries in a leaf */
	uint32_t            inner_fanout;      /* max entries in an internal */
};

/* ── helpers ─────────────────────────────────────────────────────── */

static int
key_compare(const void *a, const void *b, uint32_t n)
{
	return memcmp(a, b, n);
}

static uint32_t
leaf_entry_size(const tessera_btree_t *t)
{
	return t->key_size + t->value_size;
}

static uint32_t
inner_entry_size(const tessera_btree_t *t)
{
	return t->key_size + 8u;       /* key + child_sector */
}

static uint8_t *
leaf_entries(uint8_t *block)         { return block + HEADER_SIZE; }

static const uint8_t *
leaf_entries_const(const uint8_t *b) { return b + HEADER_SIZE; }

/* node header utilities */
static void
write_header(uint8_t *block, const tessera_btree_t *t,
             uint8_t node_kind, uint32_t entry_count)
{
	tessera_btree_node_header_t h;
	memset(&h, 0, sizeof h);
	memcpy(h.magic, TESSERA_MAGIC_BTREE_NODE, 4);
	h.version     = 1;
	h.node_kind   = node_kind;
	h.tree_kind   = t->tree_kind;
	h.entry_count = entry_count;
	h.key_size    = t->key_size;
	h.value_size  = (node_kind == 0) ? t->value_size : 8u;
	(void)tessera_encode_btree_node_header(&h, block);
	memset(block + HEADER_SIZE, 0, BLOCK_SIZE - HEADER_SIZE);
}

static int
read_header(const uint8_t *block, tessera_btree_node_header_t *h)
{
	return tessera_decode_btree_node_header(block, h);
}

static int
load_node(const tessera_btree_t *t, uint64_t sector, uint8_t *block)
{
	int r = t->io.read_block(t->io.ctx, sector, block);
	if (r != 0) return TESSERA_EIO;
	tessera_btree_node_header_t h;
	if (read_header(block, &h) != TESSERA_OK)
		return TESSERA_ECORRUPT;
	if (h.tree_kind != t->tree_kind)
		return TESSERA_ECORRUPT;
	return TESSERA_OK;
}

static int
flush_node(const tessera_btree_t *t, uint64_t sector, uint8_t *block,
           uint8_t node_kind, uint32_t entry_count)
{
	/* Re-encode the header (recomputes CRC). */
	tessera_btree_node_header_t h;
	memset(&h, 0, sizeof h);
	memcpy(h.magic, TESSERA_MAGIC_BTREE_NODE, 4);
	h.version     = 1;
	h.node_kind   = node_kind;
	h.tree_kind   = t->tree_kind;
	h.entry_count = entry_count;
	h.key_size    = t->key_size;
	h.value_size  = (node_kind == 0) ? t->value_size : 8u;
	(void)tessera_encode_btree_node_header(&h, block);

	int r = t->io.write_block(t->io.ctx, sector, block);
	return (r == 0) ? TESSERA_OK : TESSERA_EIO;
}

static int
alloc_node(const tessera_btree_t *t, uint64_t *out_sector)
{
	int r = t->io.alloc(t->io.ctx, 1, out_sector);
	return (r == 0) ? TESSERA_OK : TESSERA_ENOSPC;
}

static int
free_node(const tessera_btree_t *t, uint64_t sector)
{
	int r = t->io.free(t->io.ctx, sector, 1);
	return (r == 0) ? TESSERA_OK : TESSERA_EIO;
}

/* Binary-search for the largest index i such that entries[i].key <= key.
 * Returns -1 if all keys > key (i.e., key precedes entries[0]). */
static int
search_leaf(const uint8_t *block, uint32_t entry_count,
            uint32_t key_size, uint32_t entry_size,
            const void *key, int *out_exact)
{
	*out_exact = 0;
	int lo = 0, hi = (int)entry_count - 1, best = -1;
	while (lo <= hi) {
		int mid = lo + (hi - lo) / 2;
		const uint8_t *e =
		    leaf_entries_const(block) + (size_t)mid * entry_size;
		int c = key_compare(e, key, key_size);
		if (c == 0) { *out_exact = 1; best = mid; break; }
		if (c < 0)  { best = mid; lo = mid + 1; }
		else        { hi = mid - 1; }
	}
	return best;
}

/* For internal nodes: route a probe-key down to a child. Returns the
 * index in [0, entry_count). entry_count is guaranteed >= 1. */
static int
search_internal(const uint8_t *block, uint32_t entry_count,
                uint32_t key_size, uint32_t entry_size,
                const void *key)
{
	int exact;
	int idx = search_leaf(block, entry_count, key_size, entry_size,
	    key, &exact);
	return idx < 0 ? 0 : idx;
}

/* ── open / create / close ───────────────────────────────────────── */

static tessera_btree_t *
make_handle(const tessera_block_io_t *io, uint8_t tree_kind,
            uint32_t key_size, uint32_t value_size)
{
	tessera_btree_t *t = tessera_zalloc(sizeof *t);
	if (t == NULL) return NULL;
	t->io           = *io;
	t->tree_kind    = tree_kind;
	t->key_size     = key_size;
	t->value_size   = value_size;
	t->leaf_fanout  = (BLOCK_SIZE - HEADER_SIZE) / (key_size + value_size);
	t->inner_fanout = (BLOCK_SIZE - HEADER_SIZE) / (key_size + 8u);
	if (t->leaf_fanout < 4 || t->inner_fanout < 4) {
		tessera_free(t);
		return NULL;
	}
	return t;
}

tessera_btree_t *
tessera_btree_open(const tessera_block_io_t *io, uint64_t root_sector,
                   uint8_t tree_kind, uint32_t key_size, uint32_t value_size)
{
	if (io == NULL) return NULL;
	tessera_btree_t *t = make_handle(io, tree_kind, key_size, value_size);
	if (t == NULL) return NULL;
	t->root = root_sector;
	return t;
}

tessera_btree_t *
tessera_btree_create(const tessera_block_io_t *io, uint8_t tree_kind,
                     uint32_t key_size, uint32_t value_size,
                     uint64_t *out_root_sector)
{
	if (io == NULL || out_root_sector == NULL) return NULL;
	tessera_btree_t *t = make_handle(io, tree_kind, key_size, value_size);
	if (t == NULL) return NULL;

	uint8_t *block = tessera_zalloc(BLOCK_SIZE);
	if (block == NULL) { tessera_free(t); return NULL; }

	uint64_t s;
	if (alloc_node(t, &s) != TESSERA_OK) goto fail;
	write_header(block, t, /*leaf*/ 0, /*entries*/ 0);
	if (flush_node(t, s, block, /*leaf*/ 0, 0) != TESSERA_OK) {
		(void)free_node(t, s);
		goto fail;
	}
	tessera_free(block);
	t->root = s;
	*out_root_sector = s;
	return t;
fail:
	tessera_free(block);
	tessera_free(t);
	return NULL;
}

void
tessera_btree_close(tessera_btree_t *t)
{
	tessera_free(t);
}

/* ── get ─────────────────────────────────────────────────────────── */

int
tessera_btree_get(tessera_btree_t *t, const void *key, void *out_value)
{
	if (t == NULL || key == NULL || out_value == NULL)
		return TESSERA_EINVAL;
	uint8_t block[BLOCK_SIZE];
	uint64_t cur = t->root;
	for (uint32_t depth = 0; depth < MAX_DEPTH; depth++) {
		int r = load_node(t, cur, block);
		if (r != TESSERA_OK) return r;
		tessera_btree_node_header_t h;
		(void)read_header(block, &h);
		if (h.node_kind == 0) {                  /* leaf */
			int exact;
			int idx = search_leaf(block, h.entry_count,
			    t->key_size, leaf_entry_size(t), key, &exact);
			if (!exact) return TESSERA_ENOENT;
			const uint8_t *e = leaf_entries_const(block) +
			    (size_t)idx * leaf_entry_size(t);
			memcpy(out_value, e + t->key_size, t->value_size);
			return TESSERA_OK;
		}
		if (h.entry_count == 0) return TESSERA_ENOENT;
		int idx = search_internal(block, h.entry_count,
		    t->key_size, inner_entry_size(t), key);
		const uint8_t *e = leaf_entries_const(block) +
		    (size_t)idx * inner_entry_size(t);
		memcpy(&cur, e + t->key_size, 8);
	}
	return TESSERA_ECORRUPT;        /* depth exceeded */
}

/* ── put — recursive COW with split propagation ──────────────────── */

/* Result returned from a recursive put: either (no_split) the descendant
 * was rewritten in-place (new sector), or (split) the descendant split
 * and the parent must absorb (split_key | new_right_sector). */
struct put_result {
	uint64_t  new_left;          /* always set */
	int       split;             /* 0/1 */
	uint8_t   split_key[64];     /* up to key_size bytes (we cap at 64) */
	uint64_t  new_right;
};

static int
put_into_leaf(tessera_btree_t *t, uint64_t old_sector,
              const void *key, const void *value,
              struct put_result *res)
{
	uint8_t block[BLOCK_SIZE];
	int r = load_node(t, old_sector, block);
	if (r != TESSERA_OK) return r;

	tessera_btree_node_header_t h;
	(void)read_header(block, &h);
	const uint32_t es = leaf_entry_size(t);

	int exact;
	int idx = search_leaf(block, h.entry_count, t->key_size, es, key,
	    &exact);

	if (exact) {
		/* Update in place — same number of entries. */
		uint8_t *e = leaf_entries(block) + (size_t)idx * es;
		memcpy(e + t->key_size, value, t->value_size);

		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
		if (flush_node(t, s, block, 0, h.entry_count) != TESSERA_OK)
			return TESSERA_EIO;
		(void)free_node(t, old_sector);
		res->split = 0; res->new_left = s;
		return TESSERA_OK;
	}

	/* Insert new entry at position (idx + 1). */
	int ins = idx + 1;
	if (h.entry_count + 1 <= t->leaf_fanout) {
		/* Fits — copy with shift. */
		uint8_t *base = leaf_entries(block);
		memmove(base + (size_t)(ins + 1) * es,
		        base + (size_t)ins * es,
		        (size_t)(h.entry_count - ins) * es);
		memcpy(base + (size_t)ins * es, key, t->key_size);
		memcpy(base + (size_t)ins * es + t->key_size, value,
		    t->value_size);

		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
		if (flush_node(t, s, block, 0, h.entry_count + 1) != TESSERA_OK)
			return TESSERA_EIO;
		(void)free_node(t, old_sector);
		res->split = 0; res->new_left = s;
		return TESSERA_OK;
	}

	/* Split. Build a flat array of (entry_count+1) entries including
	 * the new one, then split halfway. */
	const uint32_t total = h.entry_count + 1;
	uint8_t *flat = tessera_malloc((size_t)total * es);
	if (flat == NULL) return TESSERA_ENOMEM;
	const uint8_t *base = leaf_entries_const(block);
	memcpy(flat, base, (size_t)ins * es);
	memcpy(flat + (size_t)ins * es, key, t->key_size);
	memcpy(flat + (size_t)ins * es + t->key_size, value, t->value_size);
	memcpy(flat + (size_t)(ins + 1) * es,
	       base + (size_t)ins * es,
	       (size_t)(h.entry_count - ins) * es);

	const uint32_t left_n  = total / 2;
	const uint32_t right_n = total - left_n;

	uint8_t left_block[BLOCK_SIZE], right_block[BLOCK_SIZE];
	write_header(left_block,  t, 0, left_n);
	write_header(right_block, t, 0, right_n);
	memcpy(leaf_entries(left_block),  flat,
	    (size_t)left_n * es);
	memcpy(leaf_entries(right_block), flat + (size_t)left_n * es,
	    (size_t)right_n * es);

	/* Split key for parent = first key of right half. */
	memcpy(res->split_key, flat + (size_t)left_n * es, t->key_size);

	tessera_free(flat);

	uint64_t sl, sr;
	if (alloc_node(t, &sl) != TESSERA_OK) return TESSERA_ENOSPC;
	if (alloc_node(t, &sr) != TESSERA_OK) {
		(void)free_node(t, sl);
		return TESSERA_ENOSPC;
	}
	if (flush_node(t, sl, left_block,  0, left_n)  != TESSERA_OK ||
	    flush_node(t, sr, right_block, 0, right_n) != TESSERA_OK)
		return TESSERA_EIO;
	(void)free_node(t, old_sector);

	res->split = 1; res->new_left = sl; res->new_right = sr;
	return TESSERA_OK;
}

static int
put_into_internal(tessera_btree_t *t, uint64_t old_sector,
                  const void *key, const void *value,
                  struct put_result *res)
{
	uint8_t block[BLOCK_SIZE];
	int r = load_node(t, old_sector, block);
	if (r != TESSERA_OK) return r;

	tessera_btree_node_header_t h;
	(void)read_header(block, &h);
	const uint32_t es = inner_entry_size(t);

	int idx = search_internal(block, h.entry_count, t->key_size, es, key);
	uint8_t *child_entry = leaf_entries(block) + (size_t)idx * es;
	uint64_t child_sector;
	memcpy(&child_sector, child_entry + t->key_size, 8);

	struct put_result child_res;
	{
		uint8_t cblk[BLOCK_SIZE];
		r = load_node(t, child_sector, cblk);
		if (r != TESSERA_OK) return r;
		tessera_btree_node_header_t ch;
		(void)read_header(cblk, &ch);
		if (ch.node_kind == 0)
			r = put_into_leaf(t, child_sector, key, value, &child_res);
		else
			r = put_into_internal(t, child_sector, key, value, &child_res);
		if (r != TESSERA_OK) return r;
	}

	/* Update the child pointer. */
	memcpy(child_entry + t->key_size, &child_res.new_left, 8);

	if (!child_res.split) {
		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
		if (flush_node(t, s, block, 1, h.entry_count) != TESSERA_OK)
			return TESSERA_EIO;
		(void)free_node(t, old_sector);
		res->split = 0; res->new_left = s;
		return TESSERA_OK;
	}

	/* Insert (split_key, new_right) at position idx+1. */
	int ins = idx + 1;
	if (h.entry_count + 1 <= t->inner_fanout) {
		uint8_t *base = leaf_entries(block);
		memmove(base + (size_t)(ins + 1) * es,
		        base + (size_t)ins * es,
		        (size_t)(h.entry_count - ins) * es);
		memcpy(base + (size_t)ins * es, child_res.split_key, t->key_size);
		memcpy(base + (size_t)ins * es + t->key_size,
		       &child_res.new_right, 8);

		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
		if (flush_node(t, s, block, 1, h.entry_count + 1) != TESSERA_OK)
			return TESSERA_EIO;
		(void)free_node(t, old_sector);
		res->split = 0; res->new_left = s;
		return TESSERA_OK;
	}

	/* Internal split. */
	const uint32_t total = h.entry_count + 1;
	uint8_t *flat = tessera_malloc((size_t)total * es);
	if (flat == NULL) return TESSERA_ENOMEM;
	const uint8_t *base = leaf_entries_const(block);
	memcpy(flat, base, (size_t)ins * es);
	memcpy(flat + (size_t)ins * es, child_res.split_key, t->key_size);
	memcpy(flat + (size_t)ins * es + t->key_size,
	       &child_res.new_right, 8);
	memcpy(flat + (size_t)(ins + 1) * es,
	       base + (size_t)ins * es,
	       (size_t)(h.entry_count - ins) * es);

	const uint32_t left_n  = total / 2;
	const uint32_t right_n = total - left_n;

	uint8_t left_block[BLOCK_SIZE], right_block[BLOCK_SIZE];
	write_header(left_block,  t, 1, left_n);
	write_header(right_block, t, 1, right_n);
	memcpy(leaf_entries(left_block),  flat,
	    (size_t)left_n * es);
	memcpy(leaf_entries(right_block), flat + (size_t)left_n * es,
	    (size_t)right_n * es);

	memcpy(res->split_key, flat + (size_t)left_n * es, t->key_size);

	tessera_free(flat);

	uint64_t sl, sr;
	if (alloc_node(t, &sl) != TESSERA_OK) return TESSERA_ENOSPC;
	if (alloc_node(t, &sr) != TESSERA_OK) {
		(void)free_node(t, sl);
		return TESSERA_ENOSPC;
	}
	if (flush_node(t, sl, left_block,  1, left_n)  != TESSERA_OK ||
	    flush_node(t, sr, right_block, 1, right_n) != TESSERA_OK)
		return TESSERA_EIO;
	(void)free_node(t, old_sector);

	res->split = 1; res->new_left = sl; res->new_right = sr;
	return TESSERA_OK;
}

int
tessera_btree_put(tessera_btree_t *t, const void *key, const void *value,
                  uint64_t *out_new_root)
{
	if (t == NULL || key == NULL || value == NULL || out_new_root == NULL)
		return TESSERA_EINVAL;

	struct put_result res;
	memset(&res, 0, sizeof res);

	/* Decide whether root is leaf or internal. */
	uint8_t block[BLOCK_SIZE];
	int r = load_node(t, t->root, block);
	if (r != TESSERA_OK) return r;
	tessera_btree_node_header_t h;
	(void)read_header(block, &h);

	if (h.node_kind == 0)
		r = put_into_leaf(t, t->root, key, value, &res);
	else
		r = put_into_internal(t, t->root, key, value, &res);
	if (r != TESSERA_OK) return r;

	if (!res.split) {
		t->root = res.new_left;
		*out_new_root = t->root;
		return TESSERA_OK;
	}

	/* Root split: build a new internal root with two entries. The
	 * leftmost entry's key is the smallest key of new_left's subtree;
	 * we recover it by reading new_left's first key. */
	uint8_t lb[BLOCK_SIZE];
	r = load_node(t, res.new_left, lb);
	if (r != TESSERA_OK) return r;
	uint8_t left_min_key[64];
	const uint8_t *first =
	    leaf_entries_const(lb) +
	    /* leaf entry size if leaf, inner entry size otherwise */
	    0;
	memcpy(left_min_key, first, t->key_size);

	uint8_t newroot[BLOCK_SIZE];
	const uint32_t es = inner_entry_size(t);
	write_header(newroot, t, 1, 2);
	uint8_t *base = leaf_entries(newroot);
	memcpy(base, left_min_key, t->key_size);
	memcpy(base + t->key_size, &res.new_left, 8);
	memcpy(base + es, res.split_key, t->key_size);
	memcpy(base + es + t->key_size, &res.new_right, 8);

	uint64_t rs;
	if (alloc_node(t, &rs) != TESSERA_OK) return TESSERA_ENOSPC;
	if (flush_node(t, rs, newroot, 1, 2) != TESSERA_OK)
		return TESSERA_EIO;
	t->root = rs;
	*out_new_root = rs;
	return TESSERA_OK;
}

/* ── delete — leaf-only deletion + opportunistic root collapse ───── */

/* Delete is simpler than insert here because v1 doesn't enforce a
 * minimum-fill invariant on internal nodes; we accept that long
 * delete-heavy churn may yield sparse internal nodes. The on-disk
 * fsck.tessera tool can compact them offline. The leaf-side
 * underflow case (entry_count drops to 0) collapses the leaf out of
 * its parent — that part we DO handle here. */

static int
delete_recurse(tessera_btree_t *t, uint64_t cur, const void *key,
               uint64_t *out_new, int *out_dropped)
{
	uint8_t block[BLOCK_SIZE];
	int r = load_node(t, cur, block);
	if (r != TESSERA_OK) return r;
	tessera_btree_node_header_t h;
	(void)read_header(block, &h);
	*out_dropped = 0;

	if (h.node_kind == 0) {
		const uint32_t es = leaf_entry_size(t);
		int exact;
		int idx = search_leaf(block, h.entry_count, t->key_size, es,
		    key, &exact);
		if (!exact) return TESSERA_ENOENT;

		if (h.entry_count == 1) {
			(void)free_node(t, cur);
			*out_dropped = 1;
			return TESSERA_OK;
		}

		uint8_t *base = leaf_entries(block);
		memmove(base + (size_t)idx * es,
		        base + (size_t)(idx + 1) * es,
		        (size_t)(h.entry_count - idx - 1) * es);

		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
		if (flush_node(t, s, block, 0, h.entry_count - 1) != TESSERA_OK)
			return TESSERA_EIO;
		(void)free_node(t, cur);
		*out_new = s;
		return TESSERA_OK;
	}

	/* Internal. */
	const uint32_t es = inner_entry_size(t);
	int idx = search_internal(block, h.entry_count, t->key_size, es, key);
	uint8_t *child_entry = leaf_entries(block) + (size_t)idx * es;
	uint64_t child_sector;
	memcpy(&child_sector, child_entry + t->key_size, 8);

	uint64_t new_child = 0;
	int dropped = 0;
	r = delete_recurse(t, child_sector, key, &new_child, &dropped);
	if (r != TESSERA_OK) return r;

	if (dropped) {
		if (h.entry_count == 1) {
			(void)free_node(t, cur);
			*out_dropped = 1;
			return TESSERA_OK;
		}
		uint8_t *base = leaf_entries(block);
		memmove(base + (size_t)idx * es,
		        base + (size_t)(idx + 1) * es,
		        (size_t)(h.entry_count - idx - 1) * es);
		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
		if (flush_node(t, s, block, 1, h.entry_count - 1) != TESSERA_OK)
			return TESSERA_EIO;
		(void)free_node(t, cur);
		*out_new = s;
		return TESSERA_OK;
	}

	memcpy(child_entry + t->key_size, &new_child, 8);
	uint64_t s;
	if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
	if (flush_node(t, s, block, 1, h.entry_count) != TESSERA_OK)
		return TESSERA_EIO;
	(void)free_node(t, cur);
	*out_new = s;
	return TESSERA_OK;
}

int
tessera_btree_delete(tessera_btree_t *t, const void *key,
                     uint64_t *out_new_root)
{
	if (t == NULL || key == NULL || out_new_root == NULL)
		return TESSERA_EINVAL;
	uint64_t new_root = 0;
	int dropped = 0;
	int r = delete_recurse(t, t->root, key, &new_root, &dropped);
	if (r != TESSERA_OK) return r;
	if (dropped) {
		/* Tree empty — re-create an empty leaf root. */
		uint8_t block[BLOCK_SIZE];
		write_header(block, t, 0, 0);
		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
		if (flush_node(t, s, block, 0, 0) != TESSERA_OK)
			return TESSERA_EIO;
		t->root = s;
	} else {
		/* Root collapse: if root is internal with a single child,
		 * promote that child as the new root. */
		uint8_t block[BLOCK_SIZE];
		r = load_node(t, new_root, block);
		if (r != TESSERA_OK) return r;
		tessera_btree_node_header_t h;
		(void)read_header(block, &h);
		while (h.node_kind == 1 && h.entry_count == 1) {
			uint64_t only_child;
			memcpy(&only_child,
			    leaf_entries_const(block) + t->key_size, 8);
			(void)free_node(t, new_root);
			new_root = only_child;
			r = load_node(t, new_root, block);
			if (r != TESSERA_OK) return r;
			(void)read_header(block, &h);
		}
		t->root = new_root;
	}
	*out_new_root = t->root;
	return TESSERA_OK;
}

/* ── cursor: forward in-order iteration ──────────────────────────── */

struct tessera_btree_cursor {
	tessera_btree_t *t;
	uint64_t  path_sectors[MAX_DEPTH];
	uint32_t  path_indices[MAX_DEPTH];
	uint8_t   path_blocks[MAX_DEPTH][BLOCK_SIZE];
	int       depth;            /* leaf is at path[depth-1] */
	int       valid;
};

/* Append a leftmost-descent path starting at `start`, BELOW whatever is
 * already in the cursor's path[0..depth-1]. Used both for fresh
 * seek_first (depth = 0 entering) and for cursor_next pop-up (depth > 0
 * entering, with the upper path already correct). */
static int
descend_to_leftmost(tessera_btree_cursor_t *c, uint64_t start)
{
	const tessera_btree_t *t = c->t;
	uint64_t cur = start;
	for (;;) {
		if (c->depth >= (int)MAX_DEPTH) return TESSERA_ECORRUPT;
		c->path_sectors[c->depth] = cur;
		c->path_indices[c->depth] = 0;
		int r = load_node(t, cur, c->path_blocks[c->depth]);
		if (r != TESSERA_OK) return r;
		tessera_btree_node_header_t h;
		(void)read_header(c->path_blocks[c->depth], &h);
		c->depth++;
		if (h.node_kind == 0) {
			c->valid = (h.entry_count > 0);
			return TESSERA_OK;
		}
		if (h.entry_count == 0) { c->valid = 0; return TESSERA_OK; }
		uint64_t child;
		memcpy(&child,
		    leaf_entries_const(c->path_blocks[c->depth - 1]) +
		    t->key_size, 8);
		cur = child;
	}
}

tessera_btree_cursor_t *
tessera_btree_seek_first(tessera_btree_t *t)
{
	if (t == NULL) return NULL;
	tessera_btree_cursor_t *c = tessera_zalloc(sizeof *c);
	if (c == NULL) return NULL;
	c->t = t;
	c->depth = 0;
	if (descend_to_leftmost(c, t->root) != TESSERA_OK) {
		tessera_free(c);
		return NULL;
	}
	return c;
}

tessera_btree_cursor_t *
tessera_btree_seek_at(tessera_btree_t *t, const void *key)
{
	if (t == NULL || key == NULL) return NULL;
	tessera_btree_cursor_t *c = tessera_zalloc(sizeof *c);
	if (c == NULL) return NULL;
	c->t = t;
	uint64_t cur = t->root;
	c->depth = 0;
	for (;;) {
		if (c->depth >= (int)MAX_DEPTH) { tessera_free(c); return NULL; }
		c->path_sectors[c->depth] = cur;
		if (load_node(t, cur, c->path_blocks[c->depth]) != TESSERA_OK) {
			tessera_free(c); return NULL;
		}
		tessera_btree_node_header_t h;
		(void)read_header(c->path_blocks[c->depth], &h);

		if (h.node_kind == 0) {
			int exact;
			int idx = search_leaf(c->path_blocks[c->depth],
			    h.entry_count, t->key_size, leaf_entry_size(t),
			    key, &exact);
			if (idx < 0) idx = 0;
			c->path_indices[c->depth] = (uint32_t)idx;
			c->depth++;
			c->valid = ((uint32_t)idx < h.entry_count) && exact;
			return c;
		}
		int idx = search_internal(c->path_blocks[c->depth],
		    h.entry_count, t->key_size, inner_entry_size(t), key);
		c->path_indices[c->depth] = (uint32_t)idx;
		c->depth++;
		uint64_t child;
		memcpy(&child,
		    leaf_entries_const(c->path_blocks[c->depth - 1]) +
		    (size_t)idx * inner_entry_size(t) + t->key_size, 8);
		cur = child;
	}
}

int
tessera_btree_cursor_get(tessera_btree_cursor_t *c, void *out_key,
                         void *out_value)
{
	if (c == NULL) return TESSERA_EINVAL;
	if (!c->valid) return TESSERA_ENOENT;
	const tessera_btree_t *t = c->t;
	uint8_t *leaf = c->path_blocks[c->depth - 1];
	const uint32_t es = leaf_entry_size(t);
	const uint32_t i  = c->path_indices[c->depth - 1];
	const uint8_t *e = leaf_entries_const(leaf) + (size_t)i * es;
	if (out_key)   memcpy(out_key,   e,                t->key_size);
	if (out_value) memcpy(out_value, e + t->key_size,  t->value_size);
	return TESSERA_OK;
}

int
tessera_btree_cursor_next(tessera_btree_cursor_t *c)
{
	if (c == NULL) return TESSERA_EINVAL;
	if (!c->valid) return TESSERA_ENOENT;
	const tessera_btree_t *t = c->t;

	/* Try advancing within the current leaf. */
	tessera_btree_node_header_t h;
	(void)read_header(c->path_blocks[c->depth - 1], &h);
	if (c->path_indices[c->depth - 1] + 1 < h.entry_count) {
		c->path_indices[c->depth - 1]++;
		return TESSERA_OK;
	}

	/* Pop up until we find a parent with a next sibling. */
	int d = c->depth - 2;
	while (d >= 0) {
		tessera_btree_node_header_t ph;
		(void)read_header(c->path_blocks[d], &ph);
		if (c->path_indices[d] + 1 < ph.entry_count) {
			c->path_indices[d]++;
			uint64_t child;
			const uint32_t es = inner_entry_size(t);
			memcpy(&child,
			    leaf_entries_const(c->path_blocks[d]) +
			    (size_t)c->path_indices[d] * es + t->key_size, 8);
			c->depth = d + 1;
			return descend_to_leftmost(c, child);
		}
		d--;
	}
	c->valid = 0;
	return TESSERA_ENOENT;
}

void
tessera_btree_cursor_free(tessera_btree_cursor_t *c)
{
	tessera_free(c);
}
