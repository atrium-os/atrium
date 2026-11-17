/*
 * tessera-mkfs-sealed — developer-build mkfs for a sealed volume
 * (docs/spec/tessera-encryption.md §5, §13 E1). Production sticks are made
 * through arcad (§7.1), which never shows the key; this takes it from a file
 * so the kernel's sealed mount can be tested before that exists.
 *
 *   tessera-mkfs-sealed --key-file K --key-id HEX [--uuid HEX]
 *       [--suite xaes|xchacha] [--hash-alg sha256|blake3] [-j SECTORS]
 *       [--create -s SIZE_MIB] [--seed-file NAME [--seed-content FILE]
 *       [--seed-chunk-size N]] PATH
 *
 * Prints the uuid (random unless given) for tessera-keyload.
 *
 *   tessera-mkfs-sealed --plain [--create -s SIZE_MIB] [...] PATH
 *
 * formats the same volume unsealed, for sealed-vs-plain comparisons.
 *
 * Build (FreeBSD, beside the core library):
 *   cc -I../core/include -o tessera-mkfs-sealed tessera-mkfs-sealed.c \
 *       ../core/src/crypt_openssl.c libtessera_core.a -lmd -lcrypto
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>

#include "tessera/crypt.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/seal.h"
#include "tessera/volume.h"

void tessera_crypt_openssl_install(void);

static int fd = -1;

static int
f_read(void *c, uint64_t s, uint8_t *o)
{
	(void)c;
	return (pread(fd, o, 4096, (off_t)(s * 4096)) == 4096 ? 0 : -1);
}
static int
f_write(void *c, uint64_t s, const uint8_t *b)
{
	(void)c;
	return (pwrite(fd, b, 4096, (off_t)(s * 4096)) == 4096 ? 0 : -1);
}

static int
unhex16(const char *h, uint8_t out[16])
{
	size_t n = 0;

	for (; *h != '\0' && n < 32; h++) {
		int v;
		if (*h == '-') continue;
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

static uint8_t *
slurp(const char *path, size_t *len)
{
	int f = open(path, O_RDONLY);
	struct stat st;
	uint8_t *b;

	if (f < 0 || fstat(f, &st) != 0) {
		perror(path);
		exit(1);
	}
	b = malloc((size_t)st.st_size + 1);
	if (b == NULL || read(f, b, (size_t)st.st_size) != st.st_size) {
		perror(path);
		exit(1);
	}
	close(f);
	*len = (size_t)st.st_size;
	return (b);
}

static void
usage(void)
{
	fprintf(stderr, "usage: tessera-mkfs-sealed --key-file K --key-id HEX "
	    "[--uuid HEX] [--suite xaes|xchacha] [--hash-alg sha256|blake3]\n"
	    "    [-j SECTORS] [--create -s SIZE_MIB] [--seed-file NAME "
	    "[--seed-content FILE] [--seed-chunk-size N]] PATH\n");
	exit(2);
}

int
main(int argc, char **argv)
{
	const char *keyfile = NULL, *path = NULL, *seed_name = NULL;
	const char *seed_file = NULL;
	uint8_t key_id[16], uuid[16], vk[32];
	int have_id = 0, have_uuid = 0, create = 0, plain = 0;
	uint32_t suite = TESSERA_SUITE_XAES_256_GCM, hash_alg = 0;
	uint64_t jsect = 256, mib = 0, seed_inode = 100;
	uint32_t chunk = 0;
	tessera_format_opts_t o;
	tessera_seal_keys_t k;

	for (int i = 1; i < argc; i++) {
		const char *a = argv[i];
		const char *v = i + 1 < argc ? argv[i + 1] : NULL;
#define NEED() do { if (v == NULL) usage(); i++; } while (0)
		if (strcmp(a, "--key-file") == 0) { NEED(); keyfile = v; }
		else if (strcmp(a, "--key-id") == 0) {
			NEED();
			if (unhex16(v, key_id) != 0) usage();
			have_id = 1;
		} else if (strcmp(a, "--uuid") == 0) {
			NEED();
			if (unhex16(v, uuid) != 0) usage();
			have_uuid = 1;
		} else if (strcmp(a, "--suite") == 0) {
			NEED();
			if (strcmp(v, "xaes") == 0)
				suite = TESSERA_SUITE_XAES_256_GCM;
			else if (strcmp(v, "xchacha") == 0)
				suite = TESSERA_SUITE_XCHACHA20_POLY1305;
			else usage();
		} else if (strcmp(a, "--hash-alg") == 0) {
			NEED();
			if (strcmp(v, "sha256") == 0) hash_alg = 0;
			else if (strcmp(v, "blake3") == 0)
				hash_alg = TESSERA_HASH_ALG_BLAKE3_256;
			else usage();
		} else if (strcmp(a, "-j") == 0) { NEED(); jsect = strtoull(v, NULL, 0); }
		else if (strcmp(a, "--create") == 0) create = 1;
		else if (strcmp(a, "--plain") == 0) plain = 1;
		else if (strcmp(a, "-s") == 0) { NEED(); mib = strtoull(v, NULL, 0); }
		else if (strcmp(a, "--seed-file") == 0) { NEED(); seed_name = v; }
		else if (strcmp(a, "--seed-inode") == 0) { NEED(); seed_inode = strtoull(v, NULL, 0); }
		else if (strcmp(a, "--seed-content") == 0) { NEED(); seed_file = v; }
		else if (strcmp(a, "--seed-chunk-size") == 0) { NEED(); chunk = (uint32_t)strtoul(v, NULL, 0); }
		else if (a[0] == '-' || path != NULL) usage();
		else path = a;
#undef NEED
	}
	if ((!plain && (keyfile == NULL || !have_id)) || path == NULL ||
	    (create && mib == 0))
		usage();

	tessera_crypt_openssl_install();
	if (!plain) {
		size_t kl;
		uint8_t *kb = slurp(keyfile, &kl);
		if (kl != 32) {
			fprintf(stderr, "%s: must be exactly 32 bytes\n", keyfile);
			return (1);
		}
		memcpy(vk, kb, 32);
		tessera_wipe(kb, kl);
		free(kb);
	}
	if (!have_uuid)
		tessera_crypt_backend()->random(uuid, 16);

	fd = open(path, O_RDWR | (create ? O_CREAT | O_TRUNC : 0), 0644);
	if (fd < 0) {
		perror(path);
		return (1);
	}
	if (create && ftruncate(fd, (off_t)(mib << 20)) != 0) {
		perror("ftruncate");
		return (1);
	}
	off_t size = lseek(fd, 0, SEEK_END);
	if (size <= 0) {
		fprintf(stderr, "%s: no size (a device needs its size known)\n",
		    path);
		return (1);
	}

	memset(&o, 0, sizeof o);
	o.total_sectors = (uint64_t)size / 4096;
	o.journal_sectors = jsect;
	memcpy(o.volume_uuid, uuid, 16);
	o.hash_alg = hash_alg;
	size_t clen = 0;
	uint8_t *content = NULL;
	if (seed_name != NULL) {
		o.seed_dirent_name = seed_name;
		o.seed_dirent_name_len = (uint16_t)strlen(seed_name);
		o.seed_dirent_inode = seed_inode;
		if (seed_file != NULL) {
			content = slurp(seed_file, &clen);
			o.seed_content_data = content;
			o.seed_content_len = clen;
			o.seed_chunk_size = chunk;
		}
	}
	tessera_block_io_t io;
	memset(&io, 0, sizeof io);
	io.read_block = f_read;
	io.write_block = f_write;
	if (plain) {
		int pr = tessera_volume_format(&io, &o);
		free(content);
		if (pr != TESSERA_OK || fsync(fd) != 0) {
			fprintf(stderr, "format failed: %d\n", pr);
			return (1);
		}
		close(fd);
		printf("formatted %s (plain)\n", path);
		return (0);
	}
	if (tessera_seal_keys_init(&k, suite, vk, uuid) != TESSERA_OK) {
		fprintf(stderr, "key derivation failed\n");
		return (1);
	}
	tessera_wipe(vk, sizeof vk);
	memcpy(k.key_id, key_id, 16);

	int r = tessera_volume_format_sealed(&io, &o, &k);
	tessera_seal_keys_clear(&k);
	free(content);
	if (r != TESSERA_OK) {
		fprintf(stderr, "format failed: %d\n", r);
		return (1);
	}
	if (fsync(fd) != 0) {
		perror("fsync");
		return (1);
	}
	close(fd);
	printf("formatted %s (sealed, %s)\nuuid ", path,
	    suite == TESSERA_SUITE_XAES_256_GCM ? "XAES-256-GCM" :
	    "XChaCha20-Poly1305");
	for (int i = 0; i < 16; i++)
		printf("%02x", uuid[i]);
	printf("\n");
	return (0);
}
