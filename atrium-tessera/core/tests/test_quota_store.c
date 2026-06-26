/*
 * Quota-domain persistence through a real B+tree (tessera-quotas.md §4.2).
 * Backed by an in-memory "disk" (same shape as test_btree) so the full
 * key-encode → btree → codec → decode path runs without volume.c I/O.
 *
 *   - put N domains, get each back byte-for-byte
 *   - update (reserve, re-put) → used_bytes persists across reload
 *   - delete → ENOENT
 *   - absent id → ENOENT
 */

#include "tessera/quota.h"
#include "tessera/btree.h"
#include "tessera/error.h"
#include "tessera/format.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures = 0;
#define CHECK(c) do { if (!(c)) { \
	fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); failures++; } } while (0)

#define BLOCK_SIZE  4096u
#define MAX_SECTORS 4096u

struct mem_disk {
	uint8_t  blocks[MAX_SECTORS][BLOCK_SIZE];
	uint8_t  used[MAX_SECTORS];
	uint64_t next_sector;
};
static int mem_read(void *c, uint64_t s, uint8_t *o) {
	struct mem_disk *d = c;
	if (s >= MAX_SECTORS || !d->used[s]) return -1;
	memcpy(o, d->blocks[s], BLOCK_SIZE); return 0;
}
static int mem_write(void *c, uint64_t s, const uint8_t *b) {
	struct mem_disk *d = c;
	if (s >= MAX_SECTORS) return -1;
	memcpy(d->blocks[s], b, BLOCK_SIZE); return 0;
}
static int mem_alloc(void *c, uint64_t n, uint64_t *o) {
	struct mem_disk *d = c;
	if (n != 1) return -1;
	for (uint64_t i = d->next_sector; i < MAX_SECTORS; i++)
		if (!d->used[i]) { d->used[i] = 1; d->next_sector = i + 1; *o = i; return 0; }
	return -1;
}
static int mem_free(void *c, uint64_t s, uint64_t n) {
	struct mem_disk *d = c;
	if (n != 1 || s >= MAX_SECTORS || !d->used[s]) return -1;
	d->used[s] = 0; if (s < d->next_sector) d->next_sector = s; return 0;
}

int
main(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;
	tessera_block_io_t io = {
		.read_block = mem_read, .write_block = mem_write,
		.alloc = mem_alloc, .free = mem_free, .ctx = d,
	};

	uint64_t root;
	tessera_btree_t *t = tessera_btree_create(&io, TESSERA_BTREE_KIND_QUOTA,
	    /*key*/ 8, /*value*/ TESSERA_QUOTA_DOMAIN_SIZE, &root);
	CHECK(t != NULL);

	/* Put three domains with distinct limits + usage. */
	for (uint64_t id = 1; id <= 3; id++) {
		tessera_quota_domain_t dm;
		tessera_quota_domain_init(&dm, id, 1000 + id, id * 1000);
		CHECK(tessera_quota_reserve(&dm, id * 100) == TESSERA_OK);
		CHECK(tessera_quota_store_put(t, &dm, &root) == TESSERA_OK);
	}

	/* Read each back; fields persisted. */
	for (uint64_t id = 1; id <= 3; id++) {
		tessera_quota_domain_t got;
		CHECK(tessera_quota_store_get(t, id, &got) == TESSERA_OK);
		CHECK(got.domain_id == id);
		CHECK(got.root_inode_no == 1000 + id);
		CHECK(got.limit_bytes == id * 1000);
		CHECK(got.used_bytes == id * 100);
	}

	/* Update domain 2: reserve more, re-put, reload → used_bytes grew. */
	{
		tessera_quota_domain_t dm;
		CHECK(tessera_quota_store_get(t, 2, &dm) == TESSERA_OK);
		CHECK(tessera_quota_reserve(&dm, 150) == TESSERA_OK);
		CHECK(tessera_quota_store_put(t, &dm, &root) == TESSERA_OK);
		tessera_quota_domain_t got;
		CHECK(tessera_quota_store_get(t, 2, &got) == TESSERA_OK);
		CHECK(got.used_bytes == 200 + 150);
	}

	/* Absent id → ENOENT. */
	{
		tessera_quota_domain_t got;
		CHECK(tessera_quota_store_get(t, 99, &got) == TESSERA_ENOENT);
	}

	/* Delete domain 1 → ENOENT after; 2 and 3 still present. */
	CHECK(tessera_quota_store_delete(t, 1, &root) == TESSERA_OK);
	{
		tessera_quota_domain_t got;
		CHECK(tessera_quota_store_get(t, 1, &got) == TESSERA_ENOENT);
		CHECK(tessera_quota_store_get(t, 3, &got) == TESSERA_OK);
	}

	tessera_btree_close(t);
	free(d);
	if (failures) { fprintf(stderr, "%d failure(s)\n", failures); return 1; }
	printf("ok\n");
	return 0;
}
