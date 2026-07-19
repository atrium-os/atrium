/*
 * tessera.c — FreeBSD loader (stand/libsa) fs_ops for Tessera.
 *
 * A thin adapter: struct fs_ops tessera_fsops delegates to the portable
 * read-only reader in libtessera_core (tessera/reader.h). The reader does
 * all the work (path walk, inode/manifest/chunk resolution, directory
 * enumeration) over a block_io callback; here that callback just wraps the
 * loader device strategy. Read-only + hash-free, so it links no hash code
 * and reads any-algorithm volumes.
 *
 * Built into libsa with -DTESSERA_STAND. Registered in the EFI loader's
 * file_system[] so `vfs.root.mountfrom=tessera:...` and loading the kernel
 * from a Tessera volume work with no ZFS/UFS in the boot path.
 */
#include <sys/param.h>
#include <sys/stat.h>
#include <sys/dirent.h>
#include <string.h>

#include "stand.h"

#include "tessera/error.h"
#include "tessera/reader.h"

#define F_READ            0x0001              /* libsa: opened for reading */
#define TESSERA_SEC       4096u
#define SEC_PER_DEVB      (TESSERA_SEC / DEV_BSIZE)   /* 4096/512 = 8 */

/* Per-open state, hung off open_file.f_fsdata. */
struct tessera_file {
	struct open_file *f;      /* back-ref: reach f_dev / f_devdata */
	tessera_reader_t *rd;
	uint32_t ino;
	uint32_t mode;
	uint64_t size;
	uint32_t rd_idx;          /* readdir cursor */
};

/* block_io read: one 4 KiB Tessera sector via the loader device strategy. */
static int
tf_read_block(void *ctx, uint64_t sector, uint8_t *out)
{
	struct tessera_file *tf = ctx;
	size_t rsize = 0;
	int rc = tf->f->f_dev->dv_strategy(tf->f->f_devdata, F_READ,
	    (daddr_t)(sector * SEC_PER_DEVB), TESSERA_SEC, (char *)out, &rsize);
#ifdef TESSERA_LOADER_DEBUG
	if (rc != 0 || rsize != TESSERA_SEC)
		printf("tessera: read_block sec=%ju rc=%d rsize=%zu\n",
		    (uintmax_t)sector, rc, rsize);
#endif
	return (rc == 0 && rsize == TESSERA_SEC) ? 0 : -1;
}

/* bulk read: `count` contiguous 4 KiB sectors in one device strategy call.
 * Collapses per-sector round-trips on large reads (the kernel). The reader
 * falls back to tf_read_block per sector if a bulk read is rejected. */
static int
tf_read_blocks(void *ctx, uint64_t sector, uint32_t count, uint8_t *out)
{
	struct tessera_file *tf = ctx;
	size_t rsize = 0;
	size_t bytes = (size_t)count * TESSERA_SEC;
	int rc = tf->f->f_dev->dv_strategy(tf->f->f_devdata, F_READ,
	    (daddr_t)(sector * SEC_PER_DEVB), bytes, (char *)out, &rsize);
	return (rc == 0 && rsize == bytes) ? 0 : -1;
}

/* libtessera_core references tessera_content_hash from format/builder
 * paths that the read-only reader never calls; provide a stub so the
 * hash-free subset links without SHA/BLAKE3. */
void tessera_content_hash(uint32_t alg, const uint8_t *data, size_t len,
                          uint8_t *out);
void
tessera_content_hash(uint32_t alg, const uint8_t *data, size_t len, uint8_t *out)
{
	(void)alg; (void)data; (void)len;
	if (out) memset(out, 0, 32);
}

/* More format/write-path symbols the read-only reader never calls but that
 * volume.c's tessera_volume_format references — stub them so the read
 * subset links without the hash / journal implementations. */
void tessera_sha256(const uint8_t *data, size_t len, uint8_t out[32]);
void
tessera_sha256(const uint8_t *data, size_t len, uint8_t out[32])
{
	(void)data; (void)len;
	if (out) memset(out, 0, 32);
}

int tessera_journal_format(const tessera_block_io_t *io, uint64_t start,
                           uint64_t len);
int
tessera_journal_format(const tessera_block_io_t *io, uint64_t start, uint64_t len)
{
	(void)io; (void)start; (void)len;
	return -1;   /* never called on the read path */
}

/* libsa has no qsort; the pack builder (never called by the reader) needs
 * one to link. Simple insertion sort — correctness only, never hot. */
void tessera_stand_qsort(void *base, size_t n, size_t sz,
                         int (*cmp)(const void *, const void *));
void
tessera_stand_qsort(void *base, size_t n, size_t sz,
                    int (*cmp)(const void *, const void *))
{
	char *a = base;
	char tmp[256];
	if (sz > sizeof tmp)
		return;   /* reader never sorts records this large */
	for (size_t i = 1; i < n; i++) {
		memcpy(tmp, a + i * sz, sz);
		size_t j = i;
		while (j > 0 && cmp(a + (j - 1) * sz, tmp) > 0) {
			memcpy(a + j * sz, a + (j - 1) * sz, sz);
			j--;
		}
		memcpy(a + j * sz, tmp, sz);
	}
}

static int
tessera_open(const char *path, struct open_file *f)
{
	struct tessera_file *tf = calloc(1, sizeof *tf);
	if (tf == NULL)
		return (ENOMEM);
	tf->f = f;

	tessera_block_io_t io;
	memset(&io, 0, sizeof io);
	io.read_block = tf_read_block;
	io.ctx = tf;
	tf->rd = tessera_reader_open_ex(&io, tf_read_blocks);
#ifdef TESSERA_LOADER_DEBUG
	printf("tessera: open '%s' reader=%p\n", path, (void *)tf->rd);
#endif
	if (tf->rd == NULL) {          /* not a Tessera volume / bad SB */
		free(tf);
		return (EINVAL);
	}

	uint32_t ino, mode; uint64_t size;
	int lrc = tessera_reader_lookup(tf->rd, path, &ino, &mode, &size);
#ifdef TESSERA_LOADER_DEBUG
	printf("tessera: lookup '%s' rc=%d ino=%u mode=0%o size=%ju\n",
	    path, lrc, ino, mode, (uintmax_t)size);
#endif
	if (lrc != TESSERA_OK) {
		tessera_reader_close(tf->rd);
		free(tf);
		return (ENOENT);
	}
	tf->ino = ino; tf->mode = mode; tf->size = size;
	f->f_fsdata = tf;
	return (0);
}

static int
tessera_close(struct open_file *f)
{
	struct tessera_file *tf = f->f_fsdata;
	if (tf != NULL) {
		if (tf->rd) tessera_reader_close(tf->rd);
		free(tf);
		f->f_fsdata = NULL;
	}
	return (0);
}

static int
tessera_read(struct open_file *f, void *buf, size_t size, size_t *resid)
{
	struct tessera_file *tf = f->f_fsdata;
	if ((tf->mode & S_IFMT) == S_IFDIR)
		return (EISDIR);
	size_t got = 0;
	if ((uint64_t)f->f_offset < tf->size) {
		size_t want = size;
		if ((uint64_t)f->f_offset + want > tf->size)
			want = (size_t)(tf->size - f->f_offset);
		if (tessera_reader_pread(tf->rd, tf->ino, (uint64_t)f->f_offset,
		    buf, want, &got) != TESSERA_OK)
			return (EIO);
		f->f_offset += got;
	}
	if (resid != NULL)
		*resid = size - got;      /* libsa: bytes NOT filled */
	return (0);
}

static off_t
tessera_seek(struct open_file *f, off_t offset, int where)
{
	struct tessera_file *tf = f->f_fsdata;
	switch (where) {
	case SEEK_SET: f->f_offset = offset; break;
	case SEEK_CUR: f->f_offset += offset; break;
	case SEEK_END: f->f_offset = (off_t)tf->size + offset; break;
	default: errno = EINVAL; return (-1);
	}
	return (f->f_offset);
}

static int
tessera_stat(struct open_file *f, struct stat *sb)
{
	struct tessera_file *tf = f->f_fsdata;
	memset(sb, 0, sizeof *sb);
	sb->st_mode = tf->mode;       /* includes S_IFMT type bits */
	sb->st_size = (off_t)tf->size;
	return (0);
}

static int
tessera_readdir(struct open_file *f, struct dirent *d)
{
	struct tessera_file *tf = f->f_fsdata;
	char name[256]; uint64_t child = 0; uint32_t cmode = 0;
	if (tessera_reader_readdir(tf->rd, tf->ino, tf->rd_idx, name, sizeof name,
	    &child, &cmode) != TESSERA_OK)
		return (ENOENT);
	tf->rd_idx++;
	d->d_fileno = (uint32_t)child;
	d->d_namlen = (uint16_t)strlen(name);
	strlcpy(d->d_name, name, sizeof d->d_name);
	d->d_type = ((cmode & S_IFMT) == S_IFDIR) ? DT_DIR : DT_REG;
	return (0);
}

struct fs_ops tessera_fsops = {
	.fs_name    = "tessera",
	.fo_open    = tessera_open,
	.fo_close   = tessera_close,
	.fo_read    = tessera_read,
	.fo_write   = null_write,
	.fo_seek    = tessera_seek,
	.fo_stat    = tessera_stat,
	.fo_readdir = tessera_readdir,
};
