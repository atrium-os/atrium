/* Host unit test: sorted-batch put vs sequential puts on the PACK-
 * REGISTRY-shaped tree (16B key, 64B value, tree_kind 1). Task #37:
 * the original differential test covered only the inode geometry
 * (4B/144B); the registry overlay flush batches through the same
 * code with very different fanout/entry arithmetic. RAM block io. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include "tessera/btree.h"
#include "tessera/error.h"
#include "tessera/format.h"

#define NSEC 200000
#define KSZ 16
#define VSZ 64
static uint8_t (*disk)[4096];
static uint64_t bump = 1;

static int rd(void *c, uint64_t s, uint8_t *b){ (void)c; if(s>=NSEC) return -1; memcpy(b, disk[s], 4096); return 0; }
static int wr(void *c, uint64_t s, const uint8_t *b){ (void)c; if(s>=NSEC) return -1; memcpy(disk[s], b, 4096); return 0; }
static int al(void *c, uint64_t n, uint64_t *out){ (void)c; if(bump+n>NSEC) return -1; *out=bump; bump+=n; return 0; }
static int fr(void *c, uint64_t s, uint64_t n){ (void)c;(void)s;(void)n; return 0; }

/* Deterministic 16-byte key from a seed — spread bytes so memcmp order
 * differs from numeric order (like real pack_ids = truncated hashes). */
static void mkkey(uint32_t seed, uint8_t k[KSZ]){
    uint32_t x = seed * 2654435761u + 12345u;
    for (int i = 0; i < KSZ; i++) { x ^= x << 13; x ^= x >> 17; x ^= x << 5; k[i] = (uint8_t)x; }
}
static int keycmp(const void *a, const void *b){ return memcmp(a, b, KSZ); }

int main(void)
{
    disk = calloc(NSEC, 4096);
    tessera_block_io_t io = { .ctx=NULL, .read_block=rd, .write_block=wr, .alloc=al, .free=fr };

    srand(424242);
    for (int round = 0; round < 12; round++) {
        memset(disk, 0, (size_t)NSEC * 4096); bump = 1;
        uint64_t rootA=0, rootB=0;
        tessera_btree_t *A = tessera_btree_create(&io, 1, KSZ, VSZ, &rootA);
        tessera_btree_t *B = tessera_btree_create(&io, 1, KSZ, VSZ, &rootB);
        assert(A && B);

        int base = rand() % 3000;
        for (int i = 0; i < base; i++) {
            uint32_t s = (uint32_t)(rand() % 10000);
            uint8_t k[KSZ]; mkkey(s, k);
            uint8_t v[VSZ]; memset(v, (int)(s & 0xff) | 1, sizeof v);
            assert(tessera_btree_put(A, k, v, &rootA) == TESSERA_OK);
            assert(tessera_btree_put(B, k, v, &rootB) == TESSERA_OK);
        }

        /* Sorted unique batch: random seeds (mix new + replace), then
         * memcmp-sort the KEYS (matching registry_ov_flush behavior). */
        int bn = 1 + rand() % 2000;
        static uint32_t seeds[4096];
        static uint8_t keys[4096*KSZ], vals[4096*VSZ];
        static uint8_t rec[4096][KSZ + VSZ];
        int n = 0;
        for (int i = 0; i < bn; i++) {
            uint32_t s = (uint32_t)(rand() % ((round % 5 == 0) ? 400 : 10000));
            mkkey(s, rec[n]);
            memset(rec[n] + KSZ, (int)(s % 251) + 1, VSZ);
            n++;
        }
        qsort(rec, (size_t)n, KSZ + VSZ, keycmp);
        /* de-dup adjacent equal keys (keep last) */
        int m = 0;
        for (int i = 0; i < n; i++) {
            if (m > 0 && memcmp(rec[m-1], rec[i], KSZ) == 0) m--;
            memcpy(rec[m], rec[i], KSZ + VSZ); m++;
        }
        n = m;
        for (int i = 0; i < n; i++) {
            memcpy(keys + (size_t)i*KSZ, rec[i], KSZ);
            memcpy(vals + (size_t)i*VSZ, rec[i] + KSZ, VSZ);
        }
        (void)seeds;

        int rc = tessera_btree_put_sorted_batch(A, keys, vals, (uint32_t)n, &rootA);
        if (rc != TESSERA_OK) { printf("round %d: batch rc=%d\n", round, rc); return 1; }
        for (int i = 0; i < n; i++)
            assert(tessera_btree_put(B, keys + (size_t)i*KSZ, vals + (size_t)i*VSZ, &rootB) == TESSERA_OK);

        /* Every batch key must be present in A with the batch value. */
        for (int i = 0; i < n; i++) {
            uint8_t chk[VSZ];
            int g = tessera_btree_get(A, keys + (size_t)i*KSZ, chk);
            if (g != TESSERA_OK || memcmp(chk, vals + (size_t)i*VSZ, VSZ) != 0) {
                printf("round %d: A LOST batch item %d g=%d\n", round, i, g);
                return 1;
            }
        }

        /* Compare across the whole seed space. */
        for (uint32_t s = 0; s < 11000; s++) {
            uint8_t k[KSZ]; mkkey(s, k);
            uint8_t va[VSZ], vb[VSZ];
            int ra = tessera_btree_get(A, k, va);
            int rb = tessera_btree_get(B, k, vb);
            if (ra != rb) { printf("round %d seed %u: ra=%d rb=%d\n", round, s, ra, rb); return 1; }
            if (ra == TESSERA_OK && memcmp(va, vb, VSZ) != 0) {
                printf("round %d seed %u: VALUE MISMATCH\n", round, s); return 1;
            }
        }

        /* Cursor-walk both — order + count must match. */
        tessera_btree_cursor_t *ca = tessera_btree_seek_first(A);
        tessera_btree_cursor_t *cb = tessera_btree_seek_first(B);
        int cnt = 0;
        while (ca && cb) {
            uint8_t ka[KSZ], kb2[KSZ], va[VSZ], vb[VSZ];
            int ga = tessera_btree_cursor_get(ca, ka, va);
            int gb = tessera_btree_cursor_get(cb, kb2, vb);
            if (ga != gb) { printf("round %d: cursor ga=%d gb=%d\n", round, ga, gb); return 1; }
            if (ga != TESSERA_OK) break;
            if (memcmp(ka, kb2, KSZ) || memcmp(va, vb, VSZ)) {
                printf("round %d: cursor mismatch at %d\n", round, cnt); return 1;
            }
            cnt++;
            int na = tessera_btree_cursor_next(ca);
            int nb = tessera_btree_cursor_next(cb);
            if ((na==TESSERA_OK) != (nb==TESSERA_OK)) { printf("round %d: next mismatch\n", round); return 1; }
            if (na != TESSERA_OK) break;
        }
        if (ca) tessera_btree_cursor_free(ca);
        if (cb) tessera_btree_cursor_free(cb);
        tessera_btree_close(A);
        tessera_btree_close(B);
        printf("round %d ok (base=%d batch=%d walked=%d)\n", round, base, n, cnt);
    }
    printf("ALL OK\n");
    return 0;
}
