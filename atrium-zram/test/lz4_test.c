/* Round-trip test for zram_lz4.c: cc -O2 -Wall -I.. -o lz4_test lz4_test.c ../zram_lz4.c */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "zram_lz4.h"

int main(void) {
	static char page[4096], out[4096 + 64], back[4096];
	void *ctx = malloc(zram_lz4_ctxsize());
	int bad = 0, i, t, c, d;
	long tot = 0;
	srandom(1);
	for (t = 0; t < 20000; t++) {
		int kind = t % 4;
		for (i = 0; i < 4096; i++)
			page[i] = kind == 0 ? (char)random() :
			    kind == 1 ? "atrium zram lz4 "[i % 16] :
			    kind == 2 ? (char)(random() % 8) : (char)((i / 64) ^ (t & 7));
		c = zram_lz4_compress(ctx, page, out, 4096, 3072);
		if (c == 0) { if (kind == 1 || kind == 3) { if (bad < 3) printf("kind %d c=0\n", kind); bad++; } continue; }	
		tot += c;
		d = zram_lz4_decompress(out, back, c, 4096);
		if (d != 4096 || memcmp(page, back, 4096) != 0) { if (bad < 3) printf("kind %d c %d d %d\n", kind, c, d); bad++; }
		/* corrupt input must not overrun */
		out[c / 2] ^= 0x5a;
		(void)zram_lz4_decompress(out, back, c, 4096);
	}
	printf("%s: %d bad, avg compressed %ld bytes\n", bad ? "FAIL" : "OK", bad, tot / 10000);
	return bad != 0;
}
