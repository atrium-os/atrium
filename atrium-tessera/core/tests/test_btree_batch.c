/* Host unit test: sorted-batch put vs sequential puts must produce
 * identical GET results, across random workloads, on the inode-shaped
 * tree (4B key, 144B value). RAM-backed block io. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include "tessera/btree.h"
#include "tessera/error.h"
#include "tessera/format.h"

#define NSEC 200000
static uint8_t (*disk)[4096];
static uint64_t bump = 1;

static int rd(void *c, uint64_t s, uint8_t *b){ (void)c; if(s>=NSEC) return -1; memcpy(b, disk[s], 4096); return 0; }
static int wr(void *c, uint64_t s, const uint8_t *b){ (void)c; if(s>=NSEC) return -1; memcpy(disk[s], b, 4096); return 0; }
static int al(void *c, uint64_t n, uint64_t *out){ (void)c; if(bump+n>NSEC) return -1; *out=bump; bump+=n; return 0; }
static int fr(void *c, uint64_t s, uint64_t n){ (void)c;(void)s;(void)n; return 0; }

static void mkkey(uint32_t ino, uint8_t k[4]){ k[0]=ino>>24; k[1]=ino>>16; k[2]=ino>>8; k[3]=ino; }

int main(void)
{
    disk = calloc(NSEC, 4096);
    tessera_block_io_t io = { .ctx=NULL, .read_block=rd, .write_block=wr, .alloc=al, .free=fr };

    srand(12345);
    for (int round = 0; round < 12; round++) {
        memset(disk, 0, (size_t)NSEC * 4096); bump = 1;  /* fresh disk */
        uint64_t rootA=0, rootB=0;
        tessera_btree_t *A = tessera_btree_create(&io, 0, 4, 144, &rootA);
        tessera_btree_t *B = tessera_btree_create(&io, 0, 4, 144, &rootB);
        assert(A && B);

        /* Pre-populate both with the same random base set. */
        int base = rand() % 3000;
        for (int i = 0; i < base; i++) {
            uint32_t ino = (uint32_t)(rand() % 10000);
            uint8_t k[4]; mkkey(ino, k);
            uint8_t v[144]; memset(v, (int)(ino & 0xff), sizeof v);
            assert(tessera_btree_put(A, k, v, &rootA) == TESSERA_OK);
            assert(tessera_btree_put(B, k, v, &rootB) == TESSERA_OK);
        }

        /* Build a sorted unique batch (mix of new + replacing keys). */
        int bn = 1 + rand() % 2000;
        static uint32_t inos[4096];
        int n = 0;
        uint32_t cur = rand() % 50;
        for (int i = 0; i < bn; i++) {
            inos[n++] = cur;
            cur += 1 + rand() % ((round % 5 == 0) ? 2 : 17); /* dense some rounds */
        }
        static uint8_t keys[4096*4], vals[4096*144];
        for (int i = 0; i < n; i++) {
            mkkey(inos[i], keys + i*4);
            memset(vals + (size_t)i*144, (int)(inos[i] % 251) + 1, 144);
        }

        /* A: batch;  B: sequential puts. */
        int rc = tessera_btree_put_sorted_batch(A, keys, vals, (uint32_t)n, &rootA);
        if (rc != TESSERA_OK) { printf("round %d: batch rc=%d\n", round, rc); return 1; }
        for (int i = 0; i < n; i++)
            assert(tessera_btree_put(B, keys + i*4, vals + (size_t)i*144, &rootB) == TESSERA_OK);
        for (int i = 0; i < n; i++) {
            uint8_t chk[144];
            int g = tessera_btree_get(B, keys + i*4, chk);
            if (g != TESSERA_OK || chk[0] != (uint8_t)((inos[i] % 251) + 1)) {
                printf("round %d: B lost batch item %d (ino %u) g=%d val=%u want=%u\n",
                    round, i, inos[i], g, chk[0], (unsigned)((inos[i]%251)+1));
                return 1;
            }
        }

        /* Compare every key 0..11000 on both trees. */
        for (uint32_t ino = 0; ino < 11000; ino++) {
            uint8_t k[4]; mkkey(ino, k);
            uint8_t va[144], vb[144];
            int ra = tessera_btree_get(A, k, va);
            int rb = tessera_btree_get(B, k, vb);
            if (ra != rb) { printf("round %d ino %u: ra=%d rb=%d\n", round, ino, ra, rb); return 1; }
            if (ra == TESSERA_OK && memcmp(va, vb, 144) != 0) {
                { int inb=0; for(int q=0;q<n;q++) if(inos[q]==ino) inb=1;
                printf("round %d ino %u: VALUE MISMATCH a=%u b=%u in_batch=%d\n", round, ino, va[0], vb[0], inb); return 1; }
            }
        }

        /* Cursor-walk both trees — order + count must match. */
        tessera_btree_cursor_t *ca = tessera_btree_seek_first(A);
        tessera_btree_cursor_t *cb = tessera_btree_seek_first(B);
        int cnt = 0;
        while (ca && cb) {
            uint8_t ka[4], kb2[4], va[144], vb[144];
            int ga = tessera_btree_cursor_get(ca, ka, va);
            int gb = tessera_btree_cursor_get(cb, kb2, vb);
            if (ga != gb) { printf("round %d: cursor ga=%d gb=%d\n", round, ga, gb); return 1; }
            if (ga != TESSERA_OK) break;
            if (memcmp(ka, kb2, 4) || memcmp(va, vb, 144)) {
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
