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
#include "tessera_rs.h"

#define BLOCK_SIZE     TESSERA_SECTOR_SIZE
#define HEADER_SIZE    64u                 /* sizeof(tessera_btree_node_header_t) */
#define MAX_DEPTH      16u                 /* >> 4096^16 keys */

/* ── tessera_btree_t — open handle ───────────────────────────────── */

struct tessera_btree {
	tessera_block_io_t  io;
	uint64_t            root;
	uint8_t             tree_kind;
	/* When set, load_node suppresses the tree_kind-mismatch debug line
	 * (still returns ECORRUPT). Used by the pin-bitmap scan's speculative
	 * walk of RETAINED-snapshot roots, which can be legitimately recycled
	 * to a live tree under create+retire churn — a benign, expected miss,
	 * not corruption. Off for every ordinary (live-tree) handle. */
	uint8_t             quiet_kind_mismatch;
	/*
	 * ★ #102: why the last load_node failed. Three very different
	 * conditions used to collapse into ECORRUPT, and a caller could not
	 * tell them apart:
	 *
	 *   IO      the device read failed — transient, says nothing about
	 *           the contents
	 *   HEADER  the sector is not a btree node at all (bad magic/CRC)
	 *   KIND    it IS a valid btree node, of a DIFFERENT tree — which is
	 *           positive proof the sector was freed and reused
	 *
	 * The GC needs KIND specifically: it means a retained snapshot's root
	 * is genuinely gone, not merely unreadable this instant. Acting on the
	 * undifferentiated failure retired healthy snapshots and lost data
	 * once already, so the distinction is load-bearing, not cosmetic.
	 */
	uint8_t             last_fail;          /* tessera_btree_fail_t */
	uint8_t             last_fail_found_kind;
	uint64_t            last_fail_sector;
	uint32_t            key_size;
	uint32_t            value_size;
	uint32_t            leaf_fanout;       /* max entries in a leaf */
	uint32_t            inner_fanout;      /* max entries in an internal */
	/*
	 * ★ Per-operation free/alloc log (see op_begin/op_end). A mutation
	 * frees the nodes it supersedes only once it has SUCCEEDED; until
	 * then free_node records them here. On failure those records are
	 * discarded and only the nodes this operation allocated are released
	 * — nothing outside the operation can reference them.
	 */
	uint32_t            op_depth;
	uint32_t            op_log_lost;       /* an append could not grow */
	uint64_t           *op_frees;
	uint32_t            op_frees_n, op_frees_cap;
	uint64_t           *op_allocs;
	uint32_t            op_allocs_n, op_allocs_cap;
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

/* Human names for TESSERA_BTREE_KIND_*. Used only in diagnostics, so an
 * unknown value returns a printable placeholder rather than asserting. */
static const char *
tessera_btree_kind_name(uint8_t kind)
{
	switch (kind) {
	case TESSERA_BTREE_KIND_INODE:      return "inode";
	case TESSERA_BTREE_KIND_PACK_REG:   return "pack-registry";
	case TESSERA_BTREE_KIND_FREE_EXT:   return "free-extent";
	case TESSERA_BTREE_KIND_SNAPSHOT:   return "snapshot";
	case TESSERA_BTREE_KIND_QUOTA:      return "quota";
	case TESSERA_BTREE_KIND_BLOB_INDEX: return "blob-index";
	case TESSERA_BTREE_KIND_DEAD_EXT:   return "dead-extent";
	default:                            return "UNKNOWN-KIND";
	}
}

/* load_node, also handing back the decoded header of an accepted node, so
 * a caller does not decode (and CRC) it a second time. */
static int
load_node_h(tessera_btree_t *t, uint64_t sector, uint8_t *block,
    tessera_btree_node_header_t *out_h)
{
	int r = t->io.read_block(t->io.ctx, sector, block);
	if (r != 0) {
		t->last_fail = TESSERA_BTREE_FAIL_IO;
		t->last_fail_sector = sector;
		return TESSERA_EIO;
	}
#ifdef TESSERA_RUST_DECODE
	/* D3: every check below, in Rust (rs/tessera-decode/src/btree.rs).
	 * Only the diagnostics stay here. */
	tessera_btree_node_header_t h;
	memset(&h, 0, sizeof h);
	int fail = TESSERA_BTREE_FAIL_NONE;
	uint8_t found_kind = 0;
	if (tessera_rs_btree_node_check(block, BLOCK_SIZE, t->tree_kind,
	    t->key_size, t->value_size, t->leaf_fanout, t->inner_fanout,
	    &h, &fail, &found_kind) != TESSERA_OK) {
		if (fail == TESSERA_BTREE_FAIL_KIND) {
			if (!t->quiet_kind_mismatch)
				tessera_debugf("btree load_node: sector %llu holds a "
				    "%s node but was reached as %s — that root is "
				    "STALE and its tree's contents are LOST. Run "
				    "tessera-fsck on an UNMOUNTED volume.\n",
				    (unsigned long long)sector,
				    tessera_btree_kind_name(found_kind),
				    tessera_btree_kind_name(t->tree_kind));
			t->last_fail_found_kind = found_kind;
		} else if (h.magic[0] != 0) {
			/* The header decoded; the count or geometry did not fit. */
			tessera_debugf("btree load_node: sector %llu refused: %s "
			    "node claims %u entries, key=%u val=%u; this %s tree "
			    "holds at most %u/%u entries, key=%u val=%u\n",
			    (unsigned long long)sector,
			    h.node_kind == 0 ? "leaf" : "internal",
			    (unsigned)h.entry_count, (unsigned)h.key_size,
			    (unsigned)h.value_size,
			    tessera_btree_kind_name(t->tree_kind),
			    (unsigned)t->leaf_fanout, (unsigned)t->inner_fanout,
			    (unsigned)t->key_size, (unsigned)t->value_size);
		}
		t->last_fail = (tessera_btree_fail_t)fail;
		t->last_fail_sector = sector;
		return TESSERA_ECORRUPT;
	}
#else
	tessera_btree_node_header_t h;
	if (read_header(block, &h) != TESSERA_OK) {
		t->last_fail = TESSERA_BTREE_FAIL_HEADER;
		t->last_fail_sector = sector;
		return TESSERA_ECORRUPT;
	}
	if (h.tree_kind != t->tree_kind) {
		if (!t->quiet_kind_mismatch)
			/* ★ #115: name the trees and state the consequence.
			 *
			 * This message used to read "sector 325 kind=5
			 * expected=4" — two bare numbers that identify neither
			 * tree and sound transient. It was the ONLY symptom of
			 * a quota root recycled out from under the superblock
			 * (fixed by pinning it, 0e27ab8), and it went
			 * unexplained across many mounts precisely because it
			 * did not say what had been lost. A diagnostic nobody
			 * can act on is barely a diagnostic. */
			tessera_debugf("btree load_node: sector %llu holds a "
			    "%s node but was reached as %s — that root is "
			    "STALE and its tree's contents are LOST. Run "
			    "tessera-fsck on an UNMOUNTED volume.\n",
			    (unsigned long long)sector,
			    tessera_btree_kind_name(h.tree_kind),
			    tessera_btree_kind_name(t->tree_kind));
		t->last_fail = TESSERA_BTREE_FAIL_KIND;
		t->last_fail_found_kind = h.tree_kind;
		t->last_fail_sector = sector;
		return TESSERA_ECORRUPT;
	}
	/*
	 * ★ Bound entry_count by what a node can PHYSICALLY hold.
	 *
	 * Every reader below walks [0, entry_count) computing
	 * block + HEADER_SIZE + i * entry_size. `block` is one 4 KiB
	 * sector, so an entry_count larger than the fanout walks straight
	 * off the end of the buffer — and since #114 moved that buffer to
	 * the heap, off the end of a kernel heap allocation. entry_count is
	 * uint32: the overread is bounded only by 4 billion * entry_size.
	 *
	 * The INSERT path has always checked this ("entry_count + 1 <=
	 * leaf_fanout"); the READ path never did. Neither magic nor CRC
	 * catches it — the CRC covers the header, so a header claiming
	 * 4,000,000 entries is perfectly self-consistent.
	 *
	 * load_node is the single choke point for every node read (10 call
	 * sites), which is why the check belongs here and not in the
	 * decoder: the decoder is handed 32 bytes and does not know the
	 * tree's geometry, while `t` does.
	 */
	uint32_t cap = (h.node_kind == 0) ? t->leaf_fanout : t->inner_fanout;
	if (h.entry_count > cap) {
		tessera_debugf("btree load_node: sector %llu claims %u entries "
		    "but a %s node of this %s tree holds at most %u — refusing "
		    "to read past the block.\n",
		    (unsigned long long)sector, (unsigned)h.entry_count,
		    h.node_kind == 0 ? "leaf" : "internal",
		    tessera_btree_kind_name(t->tree_kind), (unsigned)cap);
		t->last_fail = TESSERA_BTREE_FAIL_HEADER;
		t->last_fail_sector = sector;
		return TESSERA_ECORRUPT;
	}
	/*
	 * Geometry must match too: a node whose key/value sizes differ from
	 * the tree's would be indexed with the wrong stride, reading real
	 * bytes at meaningless offsets. Only checked when nonzero so that
	 * any node predating these fields still loads.
	 */
	uint32_t want_vs = (h.node_kind == 0) ? t->value_size : 8u;
	if ((h.key_size != 0 && h.key_size != t->key_size) ||
	    (h.value_size != 0 && h.value_size != want_vs)) {
		tessera_debugf("btree load_node: sector %llu has geometry "
		    "key=%u val=%u but this %s tree is key=%u val=%u\n",
		    (unsigned long long)sector, (unsigned)h.key_size,
		    (unsigned)h.value_size,
		    tessera_btree_kind_name(t->tree_kind),
		    (unsigned)t->key_size, (unsigned)want_vs);
		t->last_fail = TESSERA_BTREE_FAIL_HEADER;
		t->last_fail_sector = sector;
		return TESSERA_ECORRUPT;
	}
#endif
	t->last_fail = TESSERA_BTREE_FAIL_NONE;
	*out_h = h;
	return TESSERA_OK;
}

static int
load_node(tessera_btree_t *t, uint64_t sector, uint8_t *block)
{
	tessera_btree_node_header_t h;
	return load_node_h(t, sector, block, &h);
}

tessera_btree_fail_t
tessera_btree_last_fail(const tessera_btree_t *t, uint64_t *out_sector,
                        uint8_t *out_found_kind)
{
	if (t == NULL) return TESSERA_BTREE_FAIL_NONE;
	if (out_sector != NULL) *out_sector = t->last_fail_sector;
	if (out_found_kind != NULL) *out_found_kind = t->last_fail_found_kind;
	return (tessera_btree_fail_t)t->last_fail;
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

/* Append to a per-op sector log, growing it. Returns 0, or -1 when it
 * cannot grow (the caller then falls back to the always-safe choice). */
static int
op_log_push(uint64_t **v, uint32_t *n, uint32_t *cap, uint64_t sector)
{
	if (*n == *cap) {
		uint32_t nc = *cap ? *cap * 2 : 32;
		uint64_t *nv = tessera_zalloc((size_t)nc * sizeof(uint64_t));
		if (nv == NULL) return -1;
		if (*n > 0) memcpy(nv, *v, (size_t)*n * sizeof(uint64_t));
		if (*v != NULL) tessera_free(*v);
		*v = nv; *cap = nc;
	}
	(*v)[(*n)++] = sector;
	return 0;
}

static int
alloc_node(tessera_btree_t *t, uint64_t *out_sector)
{
	int r = t->io.alloc(t->io.ctx, 1, out_sector);
	if (r != 0) return TESSERA_ENOSPC;
	if (t->op_depth > 0 &&
	    op_log_push(&t->op_allocs, &t->op_allocs_n, &t->op_allocs_cap,
	    *out_sector) != 0)
		t->op_log_lost = 1;   /* unlogged: leaked if the op fails */
	return TESSERA_OK;
}

/*
 * Free a node. Inside a mutation this is DEFERRED to op_end: COW frees the
 * superseded node as soon as its replacement is written, but the operation
 * can still fail further up the tree (the parent's allocation, a root
 * split), and the caller then keeps the OLD root — which still references
 * every node freed so far. A platform allocator that recycles same-epoch
 * frees immediately (the kmod's epoch sweep, exactly when the metadata
 * reserve is exhausted) handed those nodes to another tree while they were
 * still live: "sector N holds a inode node but was reached as snapshot",
 * pinscan aborting on every pass, and a corrupt volume.
 */
static int
free_node(tessera_btree_t *t, uint64_t sector)
{
	if (t->op_depth > 0) {
		if (op_log_push(&t->op_frees, &t->op_frees_n,
		    &t->op_frees_cap, sector) != 0)
			t->op_log_lost = 1;   /* dropped: a leak, never a reuse */
		return TESSERA_OK;
	}
	int r = t->io.free(t->io.ctx, sector, 1);
	return (r == 0) ? TESSERA_OK : TESSERA_EIO;
}

static void
op_begin(tessera_btree_t *t)
{
	if (t->op_depth++ == 0) {
		t->op_frees_n = 0;
		t->op_allocs_n = 0;
		t->op_log_lost = 0;
	}
}

/*
 * Close a mutation. Success: release the superseded nodes. Failure: the
 * tree keeps its old root, so every recorded free is still reachable —
 * discard them — and release only what this operation allocated, none of
 * which the old root can reach. (A node the operation allocated and then
 * superseded appears in both logs; each branch frees from one log only, so
 * nothing is released twice.)
 */
static int
op_end(tessera_btree_t *t, int rc)
{
	if (t->op_depth == 0 || --t->op_depth > 0)
		return rc;
	if (rc == TESSERA_OK) {
		for (uint32_t i = 0; i < t->op_frees_n; i++)
			(void)t->io.free(t->io.ctx, t->op_frees[i], 1);
	} else {
		for (uint32_t i = 0; i < t->op_allocs_n; i++)
			(void)t->io.free(t->io.ctx, t->op_allocs[i], 1);
	}
	t->op_frees_n = 0;
	t->op_allocs_n = 0;
	return rc;
}

/* Binary-search for the largest index i such that entries[i].key <= key.
 * Returns -1 if all keys > key (i.e., key precedes entries[0]). */
static int
search_leaf(const uint8_t *block, uint32_t entry_count,
            uint32_t key_size, uint32_t entry_size,
            const void *key, int *out_exact)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_btree_search(block, BLOCK_SIZE, entry_count, key_size,
	    entry_size, key, out_exact);
#else
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
#endif
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


/* ── node edits ─────────────────────────────────────────────────────
 *
 * The entry-moving half of put, delete and the sorted batch. Under
 * TESSERA_RUST_DECODE these are the bounds-checked Rust edits
 * (tessera-decode btree.rs); otherwise the original C, kept as the
 * reference for the differential test. The callers have already bounded
 * every count by the fanout, so an error here is a caller bug; it comes
 * back as EINVAL rather than a write outside the block. */

/* Overwrite `len` bytes at offset `at` of entry `idx`. */
static int
node_entry_write(uint8_t *block, uint32_t es, uint32_t idx, uint32_t at,
                 const void *src, uint32_t len)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_btree_entry_write(block, BLOCK_SIZE, es, idx, at,
	    src, len);
#else
	memcpy(leaf_entries(block) + (size_t)idx * es + at, src, len);
	return TESSERA_OK;
#endif
}

/* Insert key‖val at `ins` of a node holding `count` (room for one more). */
static int
node_insert(uint8_t *block, uint32_t count, uint32_t es, uint32_t ins,
            const void *key, uint32_t ks, const void *val, uint32_t vs)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_btree_insert(block, BLOCK_SIZE, count, es, ins,
	    key, ks, val, vs);
#else
	uint8_t *base = leaf_entries(block);
	memmove(base + (size_t)(ins + 1) * es, base + (size_t)ins * es,
	    (size_t)(count - ins) * es);
	memcpy(base + (size_t)ins * es, key, ks);
	memcpy(base + (size_t)ins * es + ks, val, vs);
	return TESSERA_OK;
#endif
}

/* Remove entry `idx` of `count`. */
static int
node_remove(uint8_t *block, uint32_t count, uint32_t es, uint32_t idx)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_btree_remove(block, BLOCK_SIZE, count, es, idx);
#else
	uint8_t *base = leaf_entries(block);
	memmove(base + (size_t)idx * es, base + (size_t)(idx + 1) * es,
	    (size_t)(count - idx - 1) * es);
	return TESSERA_OK;
#endif
}

/* The count+1 entries of `block` with key‖val at `ins`: the first half
 * to `left`, the rest to `right` (entries only; headers are the
 * caller's). The split key is right's first key. */
static int
node_split_insert(const uint8_t *block, uint32_t count, uint32_t es,
                  uint32_t ins, const void *key, uint32_t ks,
                  const void *val, uint32_t vs,
                  uint8_t *left, uint8_t *right,
                  uint32_t *out_left_n, uint32_t *out_right_n)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_btree_split_insert(block, BLOCK_SIZE, count, es, ins,
	    key, ks, val, vs, left, right, out_left_n, out_right_n);
#else
	const uint32_t total = count + 1;
	uint8_t *flat = tessera_malloc((size_t)total * es);
	if (flat == NULL) return TESSERA_ENOMEM;
	const uint8_t *base = leaf_entries_const(block);
	memcpy(flat, base, (size_t)ins * es);
	memcpy(flat + (size_t)ins * es, key, ks);
	memcpy(flat + (size_t)ins * es + ks, val, vs);
	memcpy(flat + (size_t)(ins + 1) * es, base + (size_t)ins * es,
	    (size_t)(count - ins) * es);
	const uint32_t left_n = total / 2, right_n = total - left_n;
	memcpy(leaf_entries(left), flat, (size_t)left_n * es);
	memcpy(leaf_entries(right), flat + (size_t)left_n * es,
	    (size_t)right_n * es);
	tessera_free(flat);
	*out_left_n = left_n;
	*out_right_n = right_n;
	return TESSERA_OK;
#endif
}

/* The sorted batch's leaf merge into `flat` (room for count + n). */
static int
node_leaf_merge(const tessera_btree_t *t, const uint8_t *block, uint32_t count,
                const uint8_t *keys, const uint8_t *vals, uint32_t n,
                uint8_t *flat, size_t flat_len,
                tessera_btree_displaced_cb_t cb, void *cbctx,
                uint32_t *out_total)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_btree_leaf_merge(block, BLOCK_SIZE, count,
	    t->key_size, t->value_size, keys, vals, n, flat, flat_len,
	    (tessera_rs_displaced_cb_t)cb, cbctx, out_total);
#else
	(void)flat_len;
	const uint32_t es = leaf_entry_size(t);
	const uint8_t *ex = leaf_entries_const(block);
	uint32_t i = 0, j = 0, total = 0;
	while (i < count || j < n) {
		int c;
		if (i >= count)  c = 1;
		else if (j >= n) c = -1;
		else c = key_compare(ex + (size_t)i * es,
		    keys + (size_t)j * t->key_size, t->key_size);
		uint8_t *dst = flat + (size_t)total * es;
		if (c < 0) {
			memcpy(dst, ex + (size_t)i * es, es);
			i++;
		} else {
			memcpy(dst, keys + (size_t)j * t->key_size, t->key_size);
			memcpy(dst + t->key_size, vals + (size_t)j * t->value_size,
			    t->value_size);
			if (c == 0) {       /* replace */
				/* The merge is the only place that knows this
				 * key collided: hand the displaced value to the
				 * caller before it is overwritten. */
				if (cb != NULL)
					cb(cbctx, ex + (size_t)i * es,
					    ex + (size_t)i * es + t->key_size,
					    vals + (size_t)j * t->value_size);
				i++;
			}
			j++;
		}
		total++;
	}
	*out_total = total;
	return TESSERA_OK;
#endif
}

/* ── open / create / close ───────────────────────────────────────── */

static tessera_btree_t *
make_handle(const tessera_block_io_t *io, uint8_t tree_kind,
            uint32_t key_size, uint32_t value_size)
{
	/* put keeps a split key in a 64-byte buffer (struct put_result,
	 * put_impl's left_min_key); every tree today uses <= 32. The value
	 * bound keeps key + value from wrapping the fanout divisor. */
	if (key_size == 0 || key_size > TESSERA_BTREE_MAX_KEY ||
	    value_size > BLOCK_SIZE)
		return NULL;
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

void
tessera_btree_set_quiet_kind_mismatch(tessera_btree_t *t, int quiet)
{
	if (t != NULL)
		t->quiet_kind_mismatch = quiet ? 1 : 0;
}

/* The one place a root is published; fires the optional platform hook. */
static void
set_root(tessera_btree_t *t, uint64_t r)
{
	t->root = r;
	if (t->io.root_published != NULL)
		t->io.root_published(t->io.ctx, r);
}

tessera_btree_t *
tessera_btree_open(const tessera_block_io_t *io, uint64_t root_sector,
                   uint8_t tree_kind, uint32_t key_size, uint32_t value_size)
{
	if (io == NULL) return NULL;
	tessera_btree_t *t = make_handle(io, tree_kind, key_size, value_size);
	if (t == NULL) return NULL;
	set_root(t, root_sector);
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
	set_root(t, s);
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
	if (t == NULL) return;
	if (t->op_frees != NULL)  tessera_free(t->op_frees);
	if (t->op_allocs != NULL) tessera_free(t->op_allocs);
	tessera_free(t);
}

/* ── get ─────────────────────────────────────────────────────────── */


/* Reader-epoch bracket (see tessera_block_io_t). No-ops when unset. */
static inline int
rd_enter(tessera_btree_t *t, uint64_t *tok)
{
	if (t->io.reader_enter == NULL) return 0;
	*tok = t->io.reader_enter(t->io.ctx);
	return 1;
}
static inline void
rd_exit(tessera_btree_t *t, int active, uint64_t tok)
{
	if (active && t->io.reader_exit != NULL)
		t->io.reader_exit(t->io.ctx, tok);
}

static int
tessera_btree_get_impl(tessera_btree_t *t, const void *key, void *out_value,
    tessera_btree_trace_t *tr)
{
	if (t == NULL || key == NULL || out_value == NULL)
		return TESSERA_EINVAL;
	/* ★ #114: heap, NOT stack. A 4 KiB array here overruns the FreeBSD
	 * kmod's 4-page kstack on the deep GC read path
	 * (gc_data_zone_ex -> _GC_WALK_RECORD -> fetch_blob -> fetch_blob_ex
	 * -> btree_get). It does not panic cleanly: it faults in a loop, so
	 * the thread pins a CPU at 100% in state R with no wchan, emits NOTHING
	 * to the console, cannot be killed, and will not even stop for ddb.
	 * put (below) keeps its node buffers on the heap for the same reason;
	 * the READ path was missed.
	 * Verified: with this change the same GC that hung now walks the
	 * blob index (root sector 325, depth 0 -> 1) and returns rc=0. */
	uint8_t *block = tessera_zalloc(BLOCK_SIZE);
	if (block == NULL) return TESSERA_ENOMEM;
	int _rc = TESSERA_ECORRUPT;         /* depth exceeded, unless set */
	uint64_t cur = t->root;
	if (tr) { tr->depth = 0; tr->leaf_entries = 0; }
	for (uint32_t depth = 0; depth < MAX_DEPTH; depth++) {
		if (tr && depth < 16) { tr->path[depth] = cur; tr->depth = depth + 1; }
		tessera_btree_node_header_t h;
		int r = load_node_h(t, cur, block, &h);
		if (r != TESSERA_OK) { _rc = r; goto out; }
		if (h.node_kind == 0) {                  /* leaf */
			if (tr) tr->leaf_entries = h.entry_count;
			int exact;
			int idx = search_leaf(block, h.entry_count,
			    t->key_size, leaf_entry_size(t), key, &exact);
			if (!exact) { _rc = TESSERA_ENOENT; goto out; }
			const uint8_t *e = leaf_entries_const(block) +
			    (size_t)idx * leaf_entry_size(t);
			memcpy(out_value, e + t->key_size, t->value_size);
			_rc = TESSERA_OK;
			goto out;
		}
		if (h.entry_count == 0) { _rc = TESSERA_ENOENT; goto out; }
		int idx = search_internal(block, h.entry_count,
		    t->key_size, inner_entry_size(t), key);
		const uint8_t *e = leaf_entries_const(block) +
		    (size_t)idx * inner_entry_size(t);
		memcpy(&cur, e + t->key_size, 8);
	}
out:
	tessera_free(block);
	return _rc;
}

uint64_t
tessera_btree_root(const tessera_btree_t *t)
{
	return t == NULL ? 0 : t->root;
}

int
tessera_btree_get(tessera_btree_t *t, const void *key, void *out_value)
{
	if (t == NULL) return TESSERA_EINVAL;
	uint64_t tok = 0;
	int act = rd_enter(t, &tok);
	int rc = tessera_btree_get_impl(t, key, out_value, NULL);
	rd_exit(t, act, tok);
	return rc;
}

int
tessera_btree_get_traced(tessera_btree_t *t, const void *key, void *out_value,
    tessera_btree_trace_t *tr)
{
	if (t == NULL) return TESSERA_EINVAL;
	uint64_t tok = 0;
	int act = rd_enter(t, &tok);
	int rc = tessera_btree_get_impl(t, key, out_value, tr);
	rd_exit(t, act, tok);
	return rc;
}

/* ── put — copy-on-write descent, splits applied bottom-up ──────── */

/* What rewriting one node hands its parent: the node's new sector and,
 * if it split, the split key and the right half's sector. */
struct put_result {
	uint64_t  new_left;          /* always set */
	int       split;             /* 0/1 */
	uint8_t   split_key[TESSERA_BTREE_MAX_KEY];  /* make_handle caps key_size */
	uint64_t  new_right;
};

/* One node on put's root-to-leaf path, as loaded. */
struct put_level {
	uint64_t  sector;
	uint8_t  *block;             /* BLOCK_SIZE, heap */
	uint32_t  count;
	uint32_t  idx;               /* internal: the child descended into */
	uint8_t   kind;              /* 0 leaf, 1 internal */
};

/* Write the edited node to a fresh sector and free the old one. */
static int
put_cow(tessera_btree_t *t, const struct put_level *lv, uint32_t count,
        struct put_result *res)
{
	uint64_t s;
	if (alloc_node(t, &s) != TESSERA_OK) return TESSERA_ENOSPC;
	if (flush_node(t, s, lv->block, lv->kind, count) != TESSERA_OK)
		return TESSERA_EIO;
	(void)free_node(t, lv->sector);
	res->split = 0;
	res->new_left = s;
	return TESSERA_OK;
}

/* Insert key‖val at `ins` of the node, splitting it in two when full. */
static int
put_absorb(tessera_btree_t *t, const struct put_level *lv, uint32_t ins,
           const void *key, const void *val, uint32_t vs,
           struct put_result *res)
{
	const uint32_t es = lv->kind ? inner_entry_size(t) : leaf_entry_size(t);
	const uint32_t fanout = lv->kind ? t->inner_fanout : t->leaf_fanout;
	int rc;
	if (lv->count + 1 <= fanout) {
		rc = node_insert(lv->block, lv->count, es, ins, key, t->key_size,
		    val, vs);
		if (rc != TESSERA_OK) return rc;
		return put_cow(t, lv, lv->count + 1, res);
	}

	const uint32_t total = lv->count + 1;
	uint32_t left_n, right_n;
	uint8_t *left_block  = tessera_zalloc(BLOCK_SIZE);
	uint8_t *right_block = tessera_zalloc(BLOCK_SIZE);
	if (left_block == NULL || right_block == NULL) { rc = TESSERA_ENOMEM; goto out; }
	/* write_header zeroes the entry area, so headers first; the
	 * split's own counts must agree with them. */
	write_header(left_block,  t, lv->kind, total / 2);
	write_header(right_block, t, lv->kind, total - total / 2);
	rc = node_split_insert(lv->block, lv->count, es, ins, key, t->key_size,
	    val, vs, left_block, right_block, &left_n, &right_n);
	if (rc != TESSERA_OK) goto out;
	if (left_n != total / 2 || left_n + right_n != total) {
		rc = TESSERA_EINVAL; goto out;
	}
	memcpy(res->split_key, leaf_entries_const(right_block), t->key_size);

	uint64_t sl, sr;
	if (alloc_node(t, &sl) != TESSERA_OK) { rc = TESSERA_ENOSPC; goto out; }
	if (alloc_node(t, &sr) != TESSERA_OK) {
		(void)free_node(t, sl);
		rc = TESSERA_ENOSPC; goto out;
	}
	if (flush_node(t, sl, left_block,  lv->kind, left_n)  != TESSERA_OK ||
	    flush_node(t, sr, right_block, lv->kind, right_n) != TESSERA_OK) {
		rc = TESSERA_EIO; goto out;
	}
	(void)free_node(t, lv->sector);
	res->split = 1; res->new_left = sl; res->new_right = sr;
	rc = TESSERA_OK;
out:
	if (left_block)  tessera_free(left_block);
	if (right_block) tessera_free(right_block);
	return rc;
}

static int tessera_btree_put_impl(tessera_btree_t *t, const void *key,
    const void *value, uint64_t *out_new_root);

int
tessera_btree_put(tessera_btree_t *t, const void *key, const void *value,
                  uint64_t *out_new_root)
{
	if (t == NULL || key == NULL || value == NULL || out_new_root == NULL)
		return TESSERA_EINVAL;
	op_begin(t);
	return op_end(t, tessera_btree_put_impl(t, key, value, out_new_root));
}

/*
 * A loop, not recursion: the descent records each node on a heap path,
 * then the leaf is edited and each parent absorbs its child's result on
 * the way back up. Stack use is one frame however deep the tree is; the
 * recursive version cost ~480 bytes per level (864 under KASAN), so a
 * crafted 16-level tree took 7.5-13 KiB of kernel stack. Allocation,
 * write and free order is the recursive version's, so images are the same.
 *
 * Entry bytes are not under the header CRC, so a child pointer aimed back
 * up the tree is a cycle: the descent stops at MAX_DEPTH, as get's does.
 */
static int
tessera_btree_put_impl(tessera_btree_t *t, const void *key, const void *value,
                       uint64_t *out_new_root)
{
	struct put_level *path = tessera_zalloc(MAX_DEPTH * sizeof *path);
	struct put_result *res = tessera_zalloc(2 * sizeof *res);
	uint8_t *lb = NULL, *newroot = NULL;
	uint32_t n = 0;
	int rc;
	if (path == NULL || res == NULL) { rc = TESSERA_ENOMEM; goto out; }

	/* Descend. */
	const uint32_t ies = inner_entry_size(t);
	uint64_t cur = t->root;
	for (;;) {
		if (n >= MAX_DEPTH) { rc = TESSERA_ECORRUPT; goto out; }
		struct put_level *lv = &path[n++];
		lv->sector = cur;
		lv->block = tessera_zalloc(BLOCK_SIZE);
		if (lv->block == NULL) { rc = TESSERA_ENOMEM; goto out; }
		rc = load_node(t, cur, lv->block);
		if (rc != TESSERA_OK) goto out;
		tessera_btree_node_header_t h;
		(void)read_header(lv->block, &h);
		lv->count = h.entry_count;
		lv->kind = h.node_kind == 0 ? 0 : 1;
		if (lv->kind == 0) break;

		int idx = search_internal(lv->block, lv->count, t->key_size,
		    ies, key);
		lv->idx = (uint32_t)idx;
		/* Keep entry[0].key the smallest key in child 0. A key below
		 * it routes to child 0 (leftmost-descent rule); without
		 * lowering the routing key here, child 0 accumulates keys below
		 * it, and its next split hands this node a split key smaller
		 * than entry[0].key, inserted out of order at idx+1. An unsorted
		 * internal node makes routing undefined: gets miss live keys
		 * and puts create duplicates in sibling leaves (found by the
		 * batch-merge A/B test). The node is being rewritten anyway. */
		if (idx == 0 && key_compare(key, leaf_entries_const(lv->block),
		    t->key_size) < 0) {
			rc = node_entry_write(lv->block, ies, 0, 0, key,
			    t->key_size);
			if (rc != TESSERA_OK) goto out;
		}
		memcpy(&cur, leaf_entries_const(lv->block) +
		    (size_t)idx * ies + t->key_size, 8);
	}

	/* The leaf: replace in place, or insert (splitting when full). */
	unsigned r = 0;                       /* res[r] is the latest result */
	{
		const struct put_level *lv = &path[n - 1];
		const uint32_t es = leaf_entry_size(t);
		int exact;
		int idx = search_leaf(lv->block, lv->count, t->key_size, es,
		    key, &exact);
		if (exact) {
			rc = node_entry_write(lv->block, es, (uint32_t)idx,
			    t->key_size, value, t->value_size);
			if (rc == TESSERA_OK)
				rc = put_cow(t, lv, lv->count, &res[r]);
		} else {
			rc = put_absorb(t, lv, (uint32_t)(idx + 1), key, value,
			    t->value_size, &res[r]);
		}
		if (rc != TESSERA_OK) goto out;
	}

	/* Back up: each parent points at its child's new sector and takes
	 * the child's split, if any. */
	for (uint32_t i = n - 1; i-- > 0; ) {
		const struct put_level *lv = &path[i];
		const struct put_result *child = &res[r];
		struct put_result *out = &res[r ^ 1];
		rc = node_entry_write(lv->block, ies, lv->idx, t->key_size,
		    &child->new_left, 8);
		if (rc != TESSERA_OK) goto out;
		if (!child->split)
			rc = put_cow(t, lv, lv->count, out);
		else
			rc = put_absorb(t, lv, lv->idx + 1, child->split_key,
			    &child->new_right, 8, out);
		if (rc != TESSERA_OK) goto out;
		r ^= 1;
	}

	if (!res[r].split) {
		set_root(t, res[r].new_left);
		*out_new_root = t->root;
		rc = TESSERA_OK; goto out;
	}

	/* Root split: build a new internal root with two entries. */
	lb = tessera_zalloc(BLOCK_SIZE);
	if (lb == NULL) { rc = TESSERA_ENOMEM; goto out; }
	rc = load_node(t, res[r].new_left, lb);
	if (rc != TESSERA_OK) goto out;

	newroot = tessera_zalloc(BLOCK_SIZE);
	if (newroot == NULL) { rc = TESSERA_ENOMEM; goto out; }
	write_header(newroot, t, 1, 2);
	uint8_t *base = leaf_entries(newroot);
	memcpy(base, leaf_entries_const(lb), t->key_size);
	memcpy(base + t->key_size, &res[r].new_left, 8);
	memcpy(base + ies, res[r].split_key, t->key_size);
	memcpy(base + ies + t->key_size, &res[r].new_right, 8);

	uint64_t rs;
	if (alloc_node(t, &rs) != TESSERA_OK) { rc = TESSERA_ENOSPC; goto out; }
	if (flush_node(t, rs, newroot, 1, 2) != TESSERA_OK) {
		rc = TESSERA_EIO; goto out;
	}
	set_root(t, rs);
	*out_new_root = rs;
	rc = TESSERA_OK;
out:
	if (path != NULL)
		for (uint32_t i = 0; i < n; i++)
			if (path[i].block) tessera_free(path[i].block);
	if (path)    tessera_free(path);
	if (res)     tessera_free(res);
	if (lb)      tessera_free(lb);
	if (newroot) tessera_free(newroot);
	return rc;
}

/* ── sorted-batch put — one COW pass for k updates ───────────────────
 *
 * tessera_btree_put_sorted_batch(): apply n (key,value) pairs, sorted
 * strictly ascending by key, in ONE copy-on-write pass. Semantics are
 * identical to n sequential tessera_btree_put() calls (insert or
 * replace), but each affected node is rewritten ONCE: a flush that
 * drains ~1.4k dirty inodes touches ~50 contiguous leaves instead of
 * paying 1.4k root-to-leaf COW walks (the untar commit tail).
 *
 * A merged node stream that overflows fanout is split into BALANCED
 * siblings (ceil(total/fanout) nodes), and each level returns the
 * (min_key, sector) list of its replacements; the parent splices those
 * in place of the old child entry and merges its own level the same
 * way. New root levels are added at the top while more than one node
 * remains. Heap-only buffers (kmod kstack budget), same alloc/flush/
 * free_node COW discipline as put/delete. */

struct batch_repl {
	uint8_t  *keys;      /* m × key_size — min key of each new node */
	uint64_t *sectors;
	uint32_t  m;
	uint32_t  cap;
};

static void
batch_repl_free(struct batch_repl *r)
{
	if (r->keys)    tessera_free(r->keys);
	if (r->sectors) tessera_free(r->sectors);
	r->keys = NULL; r->sectors = NULL; r->m = 0; r->cap = 0;
}

static int
batch_repl_reserve(struct batch_repl *r, uint32_t want, uint32_t key_size)
{
	if (want <= r->cap) return TESSERA_OK;
	uint32_t nc = r->cap ? r->cap : 8;
	while (nc < want) nc *= 2;
	uint8_t  *nk = tessera_zalloc((size_t)nc * key_size);
	uint64_t *ns = tessera_zalloc((size_t)nc * 8);
	if (nk == NULL || ns == NULL) {
		if (nk) tessera_free(nk);
		if (ns) tessera_free(ns);
		return TESSERA_ENOMEM;
	}
	if (r->m > 0) {
		memcpy(nk, r->keys, (size_t)r->m * key_size);
		memcpy(ns, r->sectors, (size_t)r->m * 8);
	}
	if (r->keys)    tessera_free(r->keys);
	if (r->sectors) tessera_free(r->sectors);
	r->keys = nk; r->sectors = ns; r->cap = nc;
	return TESSERA_OK;
}

/* Emit `total` already-merged entries (entry_size each, in `flat`) as
 * ceil(total/fanout) balanced nodes of `node_kind`; append each node's
 * (min_key, sector) to `repl`. */
static int
batch_emit(tessera_btree_t *t, const uint8_t *flat, uint32_t total,
           uint32_t entry_size, uint8_t node_kind, uint32_t fanout,
           struct batch_repl *repl)
{
	uint8_t *block = tessera_zalloc(BLOCK_SIZE);
	int rc = TESSERA_OK;
	if (block == NULL) return TESSERA_ENOMEM;
	uint32_t nnodes = (total + fanout - 1) / fanout;
	if (nnodes == 0) nnodes = 1;    /* total==0: emit one empty node */
	uint32_t done = 0;
	for (uint32_t i = 0; i < nnodes; i++) {
		uint32_t left  = total - done;
		uint32_t nrem  = nnodes - i;
		uint32_t take  = (left + nrem - 1) / nrem;   /* balanced */
		write_header(block, t, node_kind, take);
		memcpy(leaf_entries(block), flat + (size_t)done * entry_size,
		    (size_t)take * entry_size);
		uint64_t sec;
		if (alloc_node(t, &sec) != TESSERA_OK) {
			rc = TESSERA_ENOSPC; goto out;
		}
		if (flush_node(t, sec, block, node_kind, take) != TESSERA_OK) {
			rc = TESSERA_EIO; goto out;
		}
		rc = batch_repl_reserve(repl, repl->m + 1, t->key_size);
		if (rc != TESSERA_OK) goto out;
		if (take > 0)
			memcpy(repl->keys + (size_t)repl->m * t->key_size,
			    flat + (size_t)done * entry_size, t->key_size);
		repl->sectors[repl->m] = sec;
		repl->m++;
		done += take;
	}
out:
	tessera_free(block);
	return rc;
}

static int
batch_recurse(tessera_btree_t *t, uint64_t cur,
              const uint8_t *keys, const uint8_t *vals, uint32_t n,
              struct batch_repl *out,
              tessera_btree_displaced_cb_t cb, void *cbctx, uint32_t depth)
{
	if (depth >= MAX_DEPTH) return TESSERA_ECORRUPT;    /* cycle */
	uint8_t *block = tessera_zalloc(BLOCK_SIZE);
	uint8_t *flat = NULL;
	int rc;
	if (block == NULL) return TESSERA_ENOMEM;
	rc = load_node(t, cur, block);
	if (rc != TESSERA_OK) goto out;
	tessera_btree_node_header_t h;
	(void)read_header(block, &h);

	if (h.node_kind == 0) {
		/* Leaf: two-pointer merge (replace on equal key). */
		const uint32_t es = leaf_entry_size(t);
		const size_t flat_len = (size_t)(h.entry_count + n) * es;
		flat = tessera_zalloc(flat_len);
		if (flat == NULL) { rc = TESSERA_ENOMEM; goto out; }
		uint32_t total = 0;
		rc = node_leaf_merge(t, block, h.entry_count, keys, vals, n,
		    flat, flat_len, cb, cbctx, &total);
		if (rc != TESSERA_OK) goto out;
		rc = batch_emit(t, flat, total, es, 0, t->leaf_fanout, out);
		if (rc == TESSERA_OK)
			(void)free_node(t, cur);
		goto out;
	}

	/* Internal: partition the batch among children by routing key,
	 * recurse into touched children, splice their replacement lists
	 * into a merged entry stream, re-emit balanced. */
	const uint32_t es = inner_entry_size(t);
	if (h.entry_count == 0) { rc = TESSERA_ECORRUPT; goto out; }
	/* Worst-case new entries: untouched children + every touched
	 * child's replacement list. Build incrementally. */
	uint32_t flat_cap = h.entry_count + n + 8;
	flat = tessera_zalloc((size_t)flat_cap * es);
	if (flat == NULL) { rc = TESSERA_ENOMEM; goto out; }
	uint32_t total = 0;
	uint32_t j = 0;
	for (uint32_t i = 0; i < h.entry_count; i++) {
		const uint8_t *e = leaf_entries_const(block) + (size_t)i * es;
		/* Child i covers keys in [e.key, next.key); child 0 also
		 * covers anything below its key (leftmost-descent rule). */
		uint32_t jend = j;
		if (i + 1 < h.entry_count) {
			const uint8_t *nk =
			    leaf_entries_const(block) + (size_t)(i + 1) * es;
			while (jend < n && key_compare(keys +
			    (size_t)jend * t->key_size, nk, t->key_size) < 0)
				jend++;
		} else {
			jend = n;
		}
		if (jend == j) {
			/* Untouched — keep the entry verbatim. */
			memcpy(flat + (size_t)total * es, e, es);
			total++;
			continue;
		}
		uint64_t child;
		memcpy(&child, e + t->key_size, 8);
		struct batch_repl sub;
		memset(&sub, 0, sizeof sub);
		rc = batch_recurse(t, child,
		    keys + (size_t)j * t->key_size,
		    vals + (size_t)j * t->value_size, jend - j, &sub,
		    cb, cbctx, depth + 1);
		if (rc != TESSERA_OK) { batch_repl_free(&sub); goto out; }
		if (total + sub.m > flat_cap) {
			uint32_t nc = flat_cap * 2 + sub.m;
			uint8_t *nf = tessera_zalloc((size_t)nc * es);
			if (nf == NULL) {
				batch_repl_free(&sub);
				rc = TESSERA_ENOMEM; goto out;
			}
			memcpy(nf, flat, (size_t)total * es);
			tessera_free(flat);
			flat = nf; flat_cap = nc;
		}
		for (uint32_t k = 0; k < sub.m; k++) {
			uint8_t *dst = flat + (size_t)total * es;
			/* Child 0 keeps its ORIGINAL routing key if the
			 * replacement's min key is larger — the leftmost
			 * entry's key must stay <= every key that routes
			 * here (batch keys below entries[0].key routed to
			 * child 0). Simplest correct rule: for the first
			 * replacement of the FIRST entry, take the smaller
			 * of (old key, new min key). */
			if (i == 0 && k == 0 &&
			    key_compare(e, sub.keys, t->key_size) < 0)
				memcpy(dst, e, t->key_size);
			else
				memcpy(dst, sub.keys + (size_t)k * t->key_size,
				    t->key_size);
			memcpy(dst + t->key_size, &sub.sectors[k], 8);
			total++;
		}
		batch_repl_free(&sub);
		j = jend;
	}
	rc = batch_emit(t, flat, total, es, 1, t->inner_fanout, out);
	if (rc == TESSERA_OK)
		(void)free_node(t, cur);
out:
	if (flat) tessera_free(flat);
	tessera_free(block);
	return rc;
}

static int batch_put_impl(tessera_btree_t *t, const void *keys,
    const void *values, uint32_t n, uint64_t *out_new_root,
    tessera_btree_displaced_cb_t cb, void *ctx);

int
tessera_btree_put_sorted_batch(tessera_btree_t *t, const void *keys,
                               const void *values, uint32_t n,
                               uint64_t *out_new_root)
{
	return tessera_btree_put_sorted_batch_ex(t, keys, values, n,
	    out_new_root, NULL, NULL);
}

int
tessera_btree_put_sorted_batch_ex(tessera_btree_t *t, const void *keys,
                                  const void *values, uint32_t n,
                                  uint64_t *out_new_root,
                                  tessera_btree_displaced_cb_t cb, void *ctx)
{
	if (t == NULL || keys == NULL || values == NULL || n == 0 ||
	    out_new_root == NULL)
		return TESSERA_EINVAL;
	const uint8_t *kb = keys;
	/* Strictly ascending keys are a caller contract — verify (cheap,
	 * O(n)) so a mis-sorted batch fails loudly instead of building a
	 * silently mis-ordered tree. */
	for (uint32_t i = 1; i < n; i++) {
		if (key_compare(kb + (size_t)(i - 1) * t->key_size,
		    kb + (size_t)i * t->key_size, t->key_size) >= 0)
			return TESSERA_EINVAL;
	}

	op_begin(t);
	return op_end(t, batch_put_impl(t, keys, values, n, out_new_root,
	    cb, ctx));
}

static int
batch_put_impl(tessera_btree_t *t, const void *keys, const void *values,
               uint32_t n, uint64_t *out_new_root,
               tessera_btree_displaced_cb_t cb, void *ctx)
{
	struct batch_repl repl;
	memset(&repl, 0, sizeof repl);
	int rc = batch_recurse(t, t->root, keys, values, n, &repl, cb, ctx,
	    0);
	if (rc != TESSERA_OK) { batch_repl_free(&repl); return rc; }

	/* Add root levels until a single node remains. */
	while (repl.m > 1) {
		const uint32_t es = inner_entry_size(t);
		uint8_t *flat = tessera_zalloc((size_t)repl.m * es);
		if (flat == NULL) { batch_repl_free(&repl); return TESSERA_ENOMEM; }
		for (uint32_t i = 0; i < repl.m; i++) {
			uint8_t *dst = flat + (size_t)i * es;
			memcpy(dst, repl.keys + (size_t)i * t->key_size,
			    t->key_size);
			memcpy(dst + t->key_size, &repl.sectors[i], 8);
		}
		uint32_t total = repl.m;
		batch_repl_free(&repl);
		memset(&repl, 0, sizeof repl);
		rc = batch_emit(t, flat, total, es, 1, t->inner_fanout, &repl);
		tessera_free(flat);
		if (rc != TESSERA_OK) { batch_repl_free(&repl); return rc; }
	}
	set_root(t, repl.sectors[0]);
	*out_new_root = t->root;
	batch_repl_free(&repl);
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
               uint64_t *out_new, int *out_dropped, uint32_t depth)
{
	if (depth >= MAX_DEPTH) return TESSERA_ECORRUPT;    /* cycle */
	uint8_t *block = tessera_zalloc(BLOCK_SIZE);
	int rc;
	if (block == NULL) { rc = TESSERA_ENOMEM; goto out; }
	rc = load_node(t, cur, block);
	if (rc != TESSERA_OK) goto out;
	tessera_btree_node_header_t h;
	(void)read_header(block, &h);
	*out_dropped = 0;

	if (h.node_kind == 0) {
		const uint32_t es = leaf_entry_size(t);
		int exact;
		int idx = search_leaf(block, h.entry_count, t->key_size, es,
		    key, &exact);
		if (!exact) { rc = TESSERA_ENOENT; goto out; }

		if (h.entry_count == 1) {
			(void)free_node(t, cur);
			*out_dropped = 1;
			rc = TESSERA_OK; goto out;
		}

		rc = node_remove(block, h.entry_count, es, (uint32_t)idx);
		if (rc != TESSERA_OK) goto out;

		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) { rc = TESSERA_ENOSPC; goto out; }
		if (flush_node(t, s, block, 0, h.entry_count - 1) != TESSERA_OK) {
			rc = TESSERA_EIO; goto out;
		}
		(void)free_node(t, cur);
		*out_new = s;
		rc = TESSERA_OK; goto out;
	}

	/* Internal — recurse, but free `block` first to keep heap usage flat. */
	const uint32_t es = inner_entry_size(t);
	int idx = search_internal(block, h.entry_count, t->key_size, es, key);
	uint8_t *child_entry = leaf_entries(block) + (size_t)idx * es;
	uint64_t child_sector;
	memcpy(&child_sector, child_entry + t->key_size, 8);
	uint32_t saved_entry_count = h.entry_count;
	int saved_idx = idx;
	/* Stash a private copy of `block` before recursing — recursion may
	 * need its own heap budget. */
	uint8_t *parent = block;
	block = NULL;

	uint64_t new_child = 0;
	int dropped = 0;
	rc = delete_recurse(t, child_sector, key, &new_child, &dropped,
	    depth + 1);
	if (rc != TESSERA_OK) { tessera_free(parent); goto out; }

	if (dropped) {
		if (saved_entry_count == 1) {
			(void)free_node(t, cur);
			*out_dropped = 1;
			tessera_free(parent);
			rc = TESSERA_OK; goto out;
		}
		rc = node_remove(parent, saved_entry_count, es,
		    (uint32_t)saved_idx);
		if (rc != TESSERA_OK) { tessera_free(parent); goto out; }
		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) {
			tessera_free(parent);
			rc = TESSERA_ENOSPC; goto out;
		}
		if (flush_node(t, s, parent, 1, saved_entry_count - 1) != TESSERA_OK) {
			tessera_free(parent);
			rc = TESSERA_EIO; goto out;
		}
		(void)free_node(t, cur);
		*out_new = s;
		tessera_free(parent);
		rc = TESSERA_OK; goto out;
	}

	rc = node_entry_write(parent, es, (uint32_t)saved_idx, t->key_size,
	    &new_child, 8);
	if (rc != TESSERA_OK) { tessera_free(parent); goto out; }
	uint64_t s;
	if (alloc_node(t, &s) != TESSERA_OK) {
		tessera_free(parent);
		rc = TESSERA_ENOSPC; goto out;
	}
	if (flush_node(t, s, parent, 1, saved_entry_count) != TESSERA_OK) {
		tessera_free(parent);
		rc = TESSERA_EIO; goto out;
	}
	(void)free_node(t, cur);
	*out_new = s;
	tessera_free(parent);
	rc = TESSERA_OK;
out:
	if (block) tessera_free(block);
	return rc;
}

static int tessera_btree_delete_impl(tessera_btree_t *t, const void *key,
    uint64_t *out_new_root);

int
tessera_btree_delete(tessera_btree_t *t, const void *key,
                     uint64_t *out_new_root)
{
	if (t == NULL || key == NULL || out_new_root == NULL)
		return TESSERA_EINVAL;
	op_begin(t);
	return op_end(t, tessera_btree_delete_impl(t, key, out_new_root));
}

static int
tessera_btree_delete_impl(tessera_btree_t *t, const void *key,
                          uint64_t *out_new_root)
{
	uint64_t new_root = 0;
	int dropped = 0;
	int r = delete_recurse(t, t->root, key, &new_root, &dropped, 0);
	if (r != TESSERA_OK) return r;
	if (dropped) {
		/* Tree empty — re-create an empty leaf root. */
		uint8_t *block = tessera_zalloc(BLOCK_SIZE);
		if (block == NULL) return TESSERA_ENOMEM;
		write_header(block, t, 0, 0);
		uint64_t s;
		if (alloc_node(t, &s) != TESSERA_OK) {
			tessera_free(block); return TESSERA_ENOSPC;
		}
		if (flush_node(t, s, block, 0, 0) != TESSERA_OK) {
			tessera_free(block); return TESSERA_EIO;
		}
		tessera_free(block);
		set_root(t, s);
	} else {
		/* Root collapse: if root is internal with a single child,
		 * promote that child as the new root. */
		uint8_t *block = tessera_zalloc(BLOCK_SIZE);
		if (block == NULL) return TESSERA_ENOMEM;
		r = load_node(t, new_root, block);
		if (r != TESSERA_OK) { tessera_free(block); return r; }
		tessera_btree_node_header_t h;
		(void)read_header(block, &h);
		uint32_t hops = 0;
		while (h.node_kind == 1 && h.entry_count == 1) {
			/* a one-child node pointing at itself spins here */
			if (++hops >= MAX_DEPTH) {
				tessera_free(block);
				return TESSERA_ECORRUPT;
			}
			uint64_t only_child;
			memcpy(&only_child,
			    leaf_entries_const(block) + t->key_size, 8);
			(void)free_node(t, new_root);
			new_root = only_child;
			r = load_node(t, new_root, block);
			if (r != TESSERA_OK) { tessera_free(block); return r; }
			(void)read_header(block, &h);
		}
		tessera_free(block);
		set_root(t, new_root);
	}
	*out_new_root = t->root;
	return TESSERA_OK;
}

/* ── walk-nodes (mount-time meta-reserve liveness reconstruction) ── */

static int
walk_recursive(tessera_btree_t *t, uint64_t sector,
               tessera_btree_node_visitor_t cb,
               tessera_btree_node_visitor_t post, void *ctx, uint32_t depth)
{
	if (depth >= MAX_DEPTH) return TESSERA_ECORRUPT;    /* cycle */
	int rc = cb(ctx, sector);
	/* Already-seen subtree: report nothing below it, but this is not an
	 * error and the caller's walk continues with our siblings. */
	if (rc == TESSERA_BTREE_WALK_PRUNE) return TESSERA_OK;
	if (rc != 0) return rc;
	uint8_t *block = tessera_zalloc(BLOCK_SIZE);
	if (block == NULL) return TESSERA_ENOMEM;
	rc = load_node(t, sector, block);
	if (rc != TESSERA_OK) { tessera_free(block); return rc; }
	tessera_btree_node_header_t h;
	(void)read_header(block, &h);
	if (h.node_kind == 0) {
		tessera_free(block);
		if (post != NULL) {
			rc = post(ctx, sector);
			if (rc != 0) return rc;
		}
		return TESSERA_OK;
	}
	/* Internal node — copy out child sectors, free the parent buffer
	 * before recursing to keep heap pressure low across MAX_DEPTH. */
	const uint32_t es = inner_entry_size(t);
	const uint32_t n = h.entry_count;
	uint64_t *children = tessera_malloc((size_t)n * sizeof(uint64_t));
	if (children == NULL) { tessera_free(block); return TESSERA_ENOMEM; }
	for (uint32_t i = 0; i < n; i++) {
		memcpy(&children[i],
		    leaf_entries_const(block) + (size_t)i * es + t->key_size, 8);
	}
	tessera_free(block);
	for (uint32_t i = 0; i < n; i++) {
		rc = walk_recursive(t, children[i], cb, post, ctx, depth + 1);
		if (rc != TESSERA_OK) break;
	}
	tessera_free(children);
	/* Post-visit ONLY on a fully successful subtree — that is the whole
	 * contract `post` exists to provide (see btree.h). */
	if (rc == TESSERA_OK && post != NULL) rc = post(ctx, sector);
	return rc;
}

int
tessera_btree_walk_nodes(tessera_btree_t *t,
                         tessera_btree_node_visitor_t cb, void *ctx)
{
	if (t == NULL || cb == NULL) return TESSERA_EINVAL;
	return tessera_btree_walk_nodes_ex(t, cb, NULL, ctx);   /* bracketed */
}

int
tessera_btree_walk_nodes_ex(tessera_btree_t *t,
                            tessera_btree_node_visitor_t pre,
                            tessera_btree_node_visitor_t post, void *ctx)
{
	if (t == NULL || pre == NULL) return TESSERA_EINVAL;
	uint64_t tok = 0;
	int act = rd_enter(t, &tok);
	int rc = walk_recursive(t, t->root, pre, post, ctx, 0);
	rd_exit(t, act, tok);
	return rc;
}

/* ── cursor: forward in-order iteration ──────────────────────────── */

struct tessera_btree_cursor {
	tessera_btree_t *t;
	uint64_t  rd_token;         /* reader-epoch token held from seek to free */
	int       rd_active;
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
	tessera_btree_t *t = c->t;
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


/* Every cursor dies here — including the failed-descent paths in seek_*,
 * which used to tessera_free(c) directly and leaked the reader-epoch
 * registration (63 phantom descents wedged a drain for 1940 s). */
static void
cursor_abandon(tessera_btree_cursor_t *c)
{
	if (c == NULL) return;
	rd_exit(c->t, c->rd_active, c->rd_token);
	tessera_free(c);
}

tessera_btree_cursor_t *
tessera_btree_seek_first(tessera_btree_t *t)
{
	if (t == NULL) return NULL;
	tessera_btree_cursor_t *c = tessera_zalloc(sizeof *c);
	if (c == NULL) return NULL;
	c->t = t;
	c->rd_active = rd_enter(t, &c->rd_token);
	c->depth = 0;
	if (descend_to_leftmost(c, t->root) != TESSERA_OK) {
		cursor_abandon(c);
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
	c->rd_active = rd_enter(t, &c->rd_token);
	uint64_t cur = t->root;
	c->depth = 0;
	for (;;) {
		if (c->depth >= (int)MAX_DEPTH) { cursor_abandon(c); return NULL; }
		c->path_sectors[c->depth] = cur;
		if (load_node(t, cur, c->path_blocks[c->depth]) != TESSERA_OK) {
			cursor_abandon(c); return NULL;
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
	tessera_btree_t *t = c->t;
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
	tessera_btree_t *t = c->t;

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
	cursor_abandon(c);
}
