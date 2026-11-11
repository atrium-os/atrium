/*
 * fuzz_diff.c — differential test of the Rust on-disk decoders against the C
 * ones (kernel security review 2026-11-07, D3).
 *
 * One binary holds two complete tessera-cores: the C core, and the core built
 * with -DTESSERA_RUST_DECODE whose public symbols are renamed rsmode_*
 * (scripts/tessera-decode-diff.sh). diff_body.c runs every parser over the
 * input once per core and records each result; the two traces must be equal.
 * A difference aborts with the first differing position, so libFuzzer keeps
 * the input as a crash and the replay mode reports it.
 *
 *   libFuzzer:  the first input byte picks the target (diff_trace.h), the
 *               rest is that target's input.
 *   replay:     fuzz_diff.replay <target-name> <file>...  feeds files in the
 *               shape of the matching fuzz_<name> corpus (codec, manifest,
 *               pack, btree, journal, superblock); "auto" takes the target
 *               from the first byte, as libFuzzer does.
 *
 * Before any input, the layout table the Rust side compiled in is checked
 * against offsetof()/sizeof() from format.h.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/format.h"
#include "diff_trace.h"

size_t tessera_rs_layout(size_t i);

void
diff_put(diff_trace_t *t, uint64_t x)
{
	if (t->oom) return;
	if (t->n == t->cap) {
		size_t nc = t->cap ? t->cap * 2 : 1024;
		uint64_t *nv = realloc(t->v, nc * sizeof *nv);
		if (nv == NULL) { t->oom = 1; return; }
		t->v = nv; t->cap = nc;
	}
	t->v[t->n++] = x;
}

static const char *const NAMES[DIFF_NTARGETS] = {
	"codec", "manifest", "pack", "btree", "journal", "superblock",
	"btree_ops",
};

static void
check_layout(void)
{
	static int done;
	if (done) return;
	const size_t want[] = {
		offsetof(tessera_superblock_t, crc32),
		offsetof(tessera_journal_header_t, crc32),
		offsetof(tessera_journal_header_t, head_block),
		offsetof(tessera_journal_header_t, tail_block),
		offsetof(tessera_record_header_t, record_type),
		offsetof(tessera_record_header_t, body_length),
		offsetof(tessera_record_header_t, block_count),
		offsetof(tessera_record_header_t, crc32_body),
		offsetof(tessera_record_header_t, crc32_header),
		offsetof(tessera_pack_header_t, pack_id),
		offsetof(tessera_pack_header_t, blob_count),
		offsetof(tessera_pack_header_t, index_blocks),
		offsetof(tessera_pack_header_t, bloom_bytes),
		offsetof(tessera_pack_header_t, bloom_hash_count),
		offsetof(tessera_pack_header_t, data_offset),
		offsetof(tessera_pack_header_t, data_length),
		offsetof(tessera_pack_header_t, total_pack_bytes),
		offsetof(tessera_pack_header_t, crc32_header),
		offsetof(tessera_pack_footer_t, blob_count_check),
		offsetof(tessera_pack_footer_t, crc32_pack),
		offsetof(tessera_pack_index_entry_t, data_offset),
		offsetof(tessera_pack_index_entry_t, data_size),
		offsetof(tessera_manifest_header_t, manifest_kind),
		offsetof(tessera_manifest_header_t, logical_size),
		offsetof(tessera_manifest_header_t, entry_count),
		offsetof(tessera_btree_node_header_t, node_kind),
		offsetof(tessera_btree_node_header_t, tree_kind),
		offsetof(tessera_btree_node_header_t, entry_count),
		offsetof(tessera_btree_node_header_t, key_size),
		offsetof(tessera_btree_node_header_t, value_size),
		offsetof(tessera_btree_node_header_t, crc32),
		offsetof(tessera_pack_extent_list_t, extent_count),
		offsetof(tessera_pack_extent_list_t, total_length),
		offsetof(tessera_pack_extent_list_t, extents),
		offsetof(tessera_pack_extent_list_t, crc32),
		sizeof(tessera_record_header_t),
		sizeof(tessera_btree_node_header_t),
		sizeof(tessera_manifest_header_t),
		sizeof(tessera_blob_descriptor_t),
		sizeof(tessera_pack_index_entry_t),
		sizeof(tessera_inode_record_t),
		sizeof(tessera_quota_domain_t),
		sizeof(tessera_registry_entry_t),
		sizeof(tessera_free_extent_t),
		offsetof(tessera_superblock_t, total_sectors),
		offsetof(tessera_superblock_t, journal_start),
		offsetof(tessera_superblock_t, journal_length),
		offsetof(tessera_superblock_t, pack_zone_start),
		offsetof(tessera_superblock_t, pack_zone_length),
		offsetof(tessera_superblock_t, meta_reserve_start),
		offsetof(tessera_superblock_t, meta_reserve_length),
		offsetof(tessera_superblock_t, meta_reserve_bump),
	};
	const size_t n = sizeof want / sizeof want[0];
	for (size_t i = 0; i < n; i++) {
		if (tessera_rs_layout(i) != want[i]) {
			fprintf(stderr, "fuzz_diff: LAYOUT MISMATCH at %zu: rust %zu, "
			    "format.h %zu\n", i, tessera_rs_layout(i), want[i]);
			abort();
		}
	}
	if (tessera_rs_layout(n) != SIZE_MAX) {
		fprintf(stderr, "fuzz_diff: rust LAYOUT has more entries than "
		    "this check (%zu)\n", n);
		abort();
	}
	done = 1;
}

static void
compare(int target, const uint8_t *data, size_t len)
{
	diff_trace_t c = { 0 }, r = { 0 };
	diff_run_c(target, data, len, &c);
	diff_run_rs(target, data, len, &r);
	if (c.oom || r.oom) goto out;
	size_t n = c.n < r.n ? c.n : r.n;
	for (size_t i = 0; i < n; i++) {
		if (c.v[i] == r.v[i]) continue;
		if (c.v[i] == DIFF_ANY || r.v[i] == DIFF_ANY) continue;
		fprintf(stderr, "fuzz_diff: %s: C and Rust DIFFER at trace "
		    "position %zu: C %#llx, Rust %#llx (input %zu bytes)\n",
		    NAMES[target], i, (unsigned long long)c.v[i],
		    (unsigned long long)r.v[i], len);
		abort();
	}
	if (c.n != r.n) {
		fprintf(stderr, "fuzz_diff: %s: traces differ in length: C %zu, "
		    "Rust %zu\n", NAMES[target], c.n, r.n);
		abort();
	}
out:
	free(c.v);
	free(r.v);
}

int
LLVMFuzzerTestOneInput(const uint8_t *data, size_t len)
{
	check_layout();
	if (len < 1) return 0;
	compare(data[0] % DIFF_NTARGETS, data + 1, len - 1);
	return 0;
}

#ifdef DIFF_REPLAY_MAIN
#define MAX_INPUT (8u << 20)

int
main(int argc, char **argv)
{
	if (argc < 3) {
		fprintf(stderr, "usage: %s <codec|manifest|pack|btree|journal|"
		    "superblock|btree_ops|auto> <file>...\n", argv[0]);
		return 2;
	}
	int target = -1;
	for (int i = 0; i < DIFF_NTARGETS; i++)
		if (strcmp(argv[1], NAMES[i]) == 0) target = i;
	if (target < 0 && strcmp(argv[1], "auto") != 0) {
		fprintf(stderr, "unknown target %s\n", argv[1]);
		return 2;
	}
	check_layout();
	unsigned long n = 0;
	for (int i = 2; i < argc; i++) {
		FILE *f = fopen(argv[i], "rb");
		if (f == NULL) { perror(argv[i]); return 1; }
		uint8_t *buf = malloc(MAX_INPUT);
		if (buf == NULL) { fclose(f); return 1; }
		size_t len = fread(buf, 1, MAX_INPUT, f);
		fclose(f);
		uint8_t *exact = malloc(len ? len : 1);
		if (exact == NULL) { free(buf); return 1; }
		memcpy(exact, buf, len);
		free(buf);
		if (target < 0)
			(void)LLVMFuzzerTestOneInput(exact, len);
		else
			compare(target, exact, len);
		free(exact);
		n++;
	}
	printf("compared %lu input(s), C and Rust agree\n", n);
	return 0;
}
#endif
