/*
 * tessera_format.c — the kernel formats sealed volumes
 * (docs/spec/tessera-encryption.md §7.1 step 3).
 *
 * TKEY_FORMAT on /dev/tesserakeys names a partition by an open descriptor
 * and a volume key already in the keyring (arcad MINT loaded it, scoped to
 * the uuid given here). The key goes from the keyring into derived keys in
 * kernel memory and is wiped; no process, not even the formatter's caller,
 * ever holds it. The Tessera core's tessera_volume_format_sealed does the
 * work, the same code tessera-mkfs-sealed runs in developer builds.
 *
 * I/O goes through the caller's descriptor (fo_read / fo_write at explicit
 * offsets), so GEOM's access counts are the caller's open: a mounted
 * partition can't be opened for writing in the first place.
 */
#include <sys/param.h>
#include <sys/systm.h>
#include <sys/capsicum.h>
#include <sys/disk.h>
#include <sys/fcntl.h>
#include <sys/file.h>
#include <sys/filedesc.h>
#include <sys/kernel.h>
#include <sys/malloc.h>
#include <sys/proc.h>
#include <sys/uio.h>
#include <sys/vnode.h>

#include "tessera/crypt.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/keyring.h"
#include "tessera/seal.h"
#include "tessera/volume.h"

MALLOC_DECLARE(M_TESSERA);

struct fmt_dev {
	struct file   *fp;
	struct thread *td;
	uint64_t       sectors;
};

static int
fmt_rw(struct fmt_dev *d, enum uio_rw rw, uint64_t sector, uint8_t *buf)
{
	struct iovec iov;
	struct uio uio;
	int error;

	if (sector >= d->sectors)
		return (-1);
	iov.iov_base = buf;
	iov.iov_len = TESSERA_SECTOR_SIZE;
	uio.uio_iov = &iov;
	uio.uio_iovcnt = 1;
	uio.uio_offset = (off_t)(sector * TESSERA_SECTOR_SIZE);
	uio.uio_resid = TESSERA_SECTOR_SIZE;
	uio.uio_segflg = UIO_SYSSPACE;
	uio.uio_rw = rw;
	uio.uio_td = d->td;
	if (rw == UIO_READ)
		error = fo_read(d->fp, &uio, d->td->td_ucred, FOF_OFFSET, d->td);
	else
		error = fo_write(d->fp, &uio, d->td->td_ucred, FOF_OFFSET, d->td);
	return (error != 0 || uio.uio_resid != 0 ? -1 : 0);
}

static int
fmt_read(void *ctx, uint64_t sector, uint8_t *out)
{
	return (fmt_rw(ctx, UIO_READ, sector, out));
}

static int
fmt_write(void *ctx, uint64_t sector, const uint8_t *in)
{
	/* fo_write doesn't modify the buffer; the uio just isn't const. */
	return (fmt_rw(ctx, UIO_WRITE, sector, __DECONST(uint8_t *, in)));
}

int
tessera_format_dev(struct thread *td, const struct tkey_format *f)
{
	struct tessera_seal_keys *k = NULL;
	tessera_format_opts_t o;
	tessera_block_io_t io;
	struct fmt_dev d;
	struct file *fp = NULL;
	cap_rights_t rights;
	off_t media = 0;
	uint8_t vk[32];
	uint64_t hw;
	uint32_t suite;
	int error, r;

	if ((f->flags & ~TKEY_FORMAT_RECOVERY) != 0)
		return (EINVAL);
	suite = f->suite != 0 ? f->suite : TESSERA_SUITE_XAES_256_GCM;
	if (tessera_crypt_backend() == NULL)
		return (EOPNOTSUPP);
	error = fget_write(td, f->fd, cap_rights_init_one(&rights, CAP_PWRITE),
	    &fp);
	if (error != 0)
		return (error);
	if (fp->f_type != DTYPE_VNODE || fp->f_vnode == NULL ||
	    fp->f_vnode->v_type != VCHR || (fp->f_flag & FREAD) == 0) {
		error = ENOTBLK;
		goto out;
	}
	error = fo_ioctl(fp, DIOCGMEDIASIZE, (caddr_t)&media, td->td_ucred, td);
	if (error != 0)
		goto out;
	d.fp = fp;
	d.td = td;
	d.sectors = (uint64_t)media / TESSERA_SECTOR_SIZE;
	if (f->total_sectors > d.sectors) {
		error = EINVAL;
		goto out;
	}
	if (f->total_sectors != 0)
		d.sectors = f->total_sectors;

	/* The key, for exactly this volume. */
	if (tessera_keyring_get(f->key_id, f->uuid, 0, vk, &hw, NULL) != 0) {
		error = ENOENT;
		goto out;
	}
	k = malloc(sizeof(*k), M_TESSERA, M_WAITOK | M_ZERO);
	r = tessera_seal_keys_init(k, suite, vk, f->uuid);
	explicit_bzero(vk, sizeof(vk));
	if (r != TESSERA_OK) {
		error = EINVAL;
		goto out;
	}
	memcpy(k->key_id, f->key_id, 16);

	memset(&o, 0, sizeof(o));
	o.total_sectors = d.sectors;
	o.journal_sectors = f->journal_sectors;
	memcpy(o.volume_uuid, f->uuid, 16);
	o.hash_alg = f->hash_alg;
	memset(&io, 0, sizeof(io));
	io.read_block = fmt_read;
	io.write_block = fmt_write;
	io.ctx = &d;
	r = (f->flags & TKEY_FORMAT_RECOVERY) != 0
	    ? tessera_volume_format_sealed_recovery(&io, &o, k, f->recovery_slot)
	    : tessera_volume_format_sealed(&io, &o, k);
	if (r != TESSERA_OK) {
		printf("tessera_fs: format of a sealed volume failed (%d)\n", r);
		error = (r == TESSERA_EINVAL) ? EINVAL : EIO;
		goto out;
	}
	/* Out of the drive's cache before we say it's done. */
	(void)fo_ioctl(fp, DIOCGFLUSH, NULL, td->td_ucred, td);
out:
	if (k != NULL) {
		tessera_seal_keys_clear(k);
		zfree(k, M_TESSERA);
	}
	if (fp != NULL)
		fdrop(fp, td);
	return (error);
}
