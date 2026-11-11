/* C vs Rust decoder cost on B-tree lookups: blob-index tree (32 B keys,
 * 16 B values), N random keys, then M lookups of present keys. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "tessera/btree.h"
#include "tessera/format.h"
#define SECT 4096
struct img { uint8_t *base; uint64_t sectors, next; };
static int rd(void *c, uint64_t s, uint8_t *o){ struct img *m=c; if(s>=m->sectors) return -1; memcpy(o,m->base+s*SECT,SECT); return 0; }
static int wr(void *c, uint64_t s, const uint8_t *i){ struct img *m=c; if(s>=m->sectors) return -1; memcpy(m->base+s*SECT,i,SECT); return 0; }
static int al(void *c, uint64_t n, uint64_t *o){ struct img *m=c; if(m->next+n>m->sectors) return -1; *o=m->next; m->next+=n; return 0; }
static int fr(void *c, uint64_t s, uint64_t n){ (void)c;(void)s;(void)n; return 0; }
static uint64_t x = 88172645463325252ull;
static uint64_t rnd(void){ x^=x<<13; x^=x>>7; x^=x<<17; return x; }
static double now(void){ struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return t.tv_sec+t.tv_nsec/1e9; }
int main(int argc, char **argv) {
	int N = argc>1?atoi(argv[1]):100000, M = argc>2?atoi(argv[2]):2000000;
	struct img m = { calloc(1u<<20, SECT), 1u<<20, 1 };
	tessera_block_io_t io = { rd, wr, al, fr, &m };
	tessera_btree_t *t = tessera_btree_create(&io, TESSERA_BTREE_KIND_BLOB_INDEX, 32, 16, &(uint64_t){0});
	if (!t) { puts("create failed"); return 1; }
	uint8_t (*keys)[32] = malloc((size_t)N*32), v[16]={1};
	for (int i=0;i<N;i++){ for(int j=0;j<4;j++){uint64_t r=rnd(); memcpy(keys[i]+8*j,&r,8);} uint64_t nr; if(tessera_btree_put(t,keys[i],v,&nr)){puts("put failed");return 1;} }
	double best=1e9; long found=0;
	for (int rep=0; rep<5; rep++) {
		double t0=now(); found=0;
		for (int i=0;i<M;i++){ uint8_t o[16]; found += tessera_btree_get(t, keys[rnd()%N], o)==0; }
		double dt=now()-t0; if(dt<best) best=dt;
	}
	printf("%d keys, %d lookups: best %.1f ns/lookup (found %ld)\n", N, M, best*1e9/M, found);
	return 0;
}
