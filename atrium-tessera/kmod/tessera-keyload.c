/*
 * tessera-keyload — developer-build loader for /dev/tesserakeys
 * (docs/spec/tessera-encryption.md §4.3). Production keys come from arcad;
 * this exists so a sealed volume can be mounted in a VM before Arca's ops
 * are built. The key file holds the 32 raw key bytes.
 *
 *   tessera-keyload load KEY_FILE KEY_ID_HEX UUID_HEX [HIGH_WATER]
 *   tessera-keyload drop KEY_ID_HEX
 *   tessera-keyload gen  KEY_ID_HEX
 *
 * Build: cc -I../core/include -o tessera-keyload tessera-keyload.c
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <unistd.h>
#include <sys/ioctl.h>

#include "tessera/keyring.h"

static int
unhex16(const char *h, uint8_t out[16])
{
	size_t n = 0;

	for (; *h != '\0' && n < 32; h++) {
		if (*h == '-')
			continue;
		int v;
		if (*h >= '0' && *h <= '9') v = *h - '0';
		else if (*h >= 'a' && *h <= 'f') v = *h - 'a' + 10;
		else if (*h >= 'A' && *h <= 'F') v = *h - 'A' + 10;
		else return (-1);
		if (n % 2 == 0) out[n / 2] = (uint8_t)(v << 4);
		else out[n / 2] |= (uint8_t)v;
		n++;
	}
	return (n == 32 && *h == '\0' ? 0 : -1);
}

static void
usage(void)
{
	fprintf(stderr, "usage: tessera-keyload load KEY_FILE KEY_ID UUID "
	    "[HIGH_WATER]\n       tessera-keyload drop KEY_ID\n"
	    "       tessera-keyload gen KEY_ID\n");
	exit(2);
}

int
main(int argc, char **argv)
{
	int fd;

	if (argc < 3)
		usage();
	fd = open("/dev/" TESSERA_KEYRING_DEV, O_RDWR);
	if (fd < 0) {
		perror("/dev/" TESSERA_KEYRING_DEV);
		return (1);
	}
	if (strcmp(argv[1], "load") == 0 && (argc == 5 || argc == 6)) {
		struct tkey_load l;
		int kf;
		ssize_t n;

		memset(&l, 0, sizeof l);
		if (unhex16(argv[3], l.key_id) != 0 ||
		    unhex16(argv[4], l.scope_uuid) != 0) {
			fprintf(stderr, "key id and uuid: 32 hex digits\n");
			return (2);
		}
		if (argc == 6)
			l.high_water = strtoull(argv[5], NULL, 0);
		l.owner_uid = (uint32_t)getuid();
		strlcpy(l.owner_label, "dev", sizeof l.owner_label);
		kf = open(argv[2], O_RDONLY);
		if (kf < 0) {
			perror(argv[2]);
			return (1);
		}
		n = read(kf, l.key, sizeof l.key);
		if (n != (ssize_t)sizeof l.key || read(kf, &l.flags, 1) != 0) {
			fprintf(stderr, "%s: must be exactly 32 bytes\n", argv[2]);
			explicit_bzero(&l, sizeof l);
			return (1);
		}
		l.flags = 0;
		close(kf);
		int r = ioctl(fd, TKEY_LOAD, &l);
		explicit_bzero(&l, sizeof l);
		if (r != 0) {
			perror("TKEY_LOAD");
			return (1);
		}
		printf("loaded\n");
	} else if (strcmp(argv[1], "drop") == 0 && argc == 3) {
		struct tkey_id d;

		if (unhex16(argv[2], d.key_id) != 0)
			usage();
		if (ioctl(fd, TKEY_DROP, &d) != 0) {
			perror("TKEY_DROP");
			return (1);
		}
		printf("dropped\n");
	} else if (strcmp(argv[1], "gen") == 0 && argc == 3) {
		struct tkey_gen g;

		memset(&g, 0, sizeof g);
		if (unhex16(argv[2], g.key_id) != 0)
			usage();
		if (ioctl(fd, TKEY_GEN, &g) != 0) {
			perror("TKEY_GEN");
			return (1);
		}
		printf("%ju\n", (uintmax_t)g.generation);
	} else {
		usage();
	}
	close(fd);
	return (0);
}
