/*
 * tessera-damage-root — point a superblock root at a sector it does not own.
 *
 * TEST TOOL, not shipped. fsck's stale-root tiers (#115) each claim a
 * different outcome — rebuild, clear, refuse — and none of those claims meant
 * anything until a volume could actually be put into that state on purpose.
 * The dead-extent case was only ever reproduced because repack happened to
 * cause it; the free-extent Rebuild tier has no such accident to lean on.
 *
 * Why this cannot be `dd`: the superblock is CRC32-covered (bytes 0..crc32),
 * so a raw byte poke makes the SB undecodable and every tool refuses the
 * volume before reaching the check under test. Decode, set, re-encode.
 *
 * Field names come from tessera/reserve_trees.h, so this tool cannot fall
 * behind the list it is meant to damage.
 *
 * usage: tessera-damage-root <device> <field> <sector>
 *        tessera-damage-root <device> --list
 */
#include <tessera/reserve_trees.h>
#include <tessera/volume.h>
#include <tessera/codec.h>
#include <tessera/format.h>
#include <tessera/error.h>

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void
usage(void)
{
	fprintf(stderr, "usage: tessera-damage-root <device> <field> <sector>\n"
	                "       tessera-damage-root <device> --list\n\nfields:\n");
#define ROW(f, k, ks, vs, tier, why) fprintf(stderr, "  %-20s (tier %s)\n", #f, #tier);
	TESSERA_RESERVE_TREES(ROW)
#undef ROW
	exit(2);
}

int
main(int argc, char **argv)
{
	if (argc < 3) usage();
	const char *dev = argv[1], *field = argv[2];

	int fd = open(dev, O_RDWR | O_SYNC);
	if (fd < 0) { perror("open"); return 1; }

	uint8_t buf[TESSERA_SECTOR_SIZE];
	if (pread(fd, buf, sizeof buf, 0) != (ssize_t)sizeof buf) {
		perror("pread sb"); return 1;
	}
	tessera_superblock_t sb;
	if (tessera_decode_superblock(buf, &sb) != TESSERA_OK) {
		fprintf(stderr, "superblock does not decode (bad magic or CRC)\n");
		return 1;
	}

	if (strcmp(field, "--list") == 0) {
#define SHOW(f, k, ks, vs, tier, why) \
		printf("  %-20s = %llu\n", #f, (unsigned long long)sb.f);
		TESSERA_RESERVE_TREES(SHOW)
#undef SHOW
		return 0;
	}
	if (argc < 4) usage();
	uint64_t sector = strtoull(argv[3], NULL, 0);

	int found = 0;
	uint64_t old = 0;
#define SET(f, k, ks, vs, tier, why) \
	if (strcmp(field, #f) == 0) { old = sb.f; sb.f = sector; found = 1; }
	TESSERA_RESERVE_TREES(SET)
#undef SET
	if (!found) { fprintf(stderr, "unknown field: %s\n", field); usage(); }

	/* Bump the generation so the kernel and tools see a newer SB rather than
	 * silently preferring the untouched copy. Both copies are rewritten. */
	sb.generation += 1;
	if (tessera_encode_superblock(&sb, buf) != TESSERA_OK) {
		fprintf(stderr, "encode failed\n"); return 1;
	}
	for (int i = 0; i < 2; i++) {
		if (pwrite(fd, buf, sizeof buf, (off_t)i * TESSERA_SECTOR_SIZE)
		    != (ssize_t)sizeof buf) { perror("pwrite sb"); return 1; }
	}
	if (fsync(fd) != 0) { perror("fsync"); return 1; }
	close(fd);
	printf("%s: %s %llu -> %llu (generation %llu)\n", dev, field,
	    (unsigned long long)old, (unsigned long long)sector,
	    (unsigned long long)sb.generation);
	return 0;
}
