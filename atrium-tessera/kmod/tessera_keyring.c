/*
 * tessera_keyring.c — the kernel keyring and /dev/tesserakeys
 * (docs/spec/tessera-encryption.md §4.3; ABI in tessera/keyring.h).
 *
 * arcad (or, in developer builds, tessera-keyload) loads a volume key for
 * one scope; a mount of that volume copies it out, derives its keys and
 * wipes the copy. The device is root:wheel 0600 here; devfs rules hand it to
 * _arca. A jailed process is refused whatever the permissions unless its
 * jail has allow.tessera_keys, which jaild gives arcad's jail alone (policy
 * [tessera_keys]): a devfs ruleset that failed to hide the node still does
 * not hand the keyring to another jail.
 */
#include <sys/param.h>
#include <sys/systm.h>
#include <sys/conf.h>
#include <sys/fcntl.h>
#include <sys/jail.h>
#include <sys/kernel.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/mutex.h>
#include <sys/proc.h>
#include <sys/ucred.h>
#include <machine/atomic.h>

#include "tessera/keyring.h"

static MALLOC_DEFINE(M_TESSERA_KEYS, "tessera_keys", "Tessera keyring");

struct tkey {
	uint8_t  key_id[16];
	uint8_t  uuid[16];
	uint64_t group;
	uint8_t  key[32];
	uint64_t high_water;
	uint64_t committed;           /* highest generation noted */
	uint32_t flags;               /* TKEY_LOAD_* */
	uint32_t owner_uid;
	char     owner_label[32];
	int      used;
};

static struct tkey *ring;         /* TESSERA_KEYRING_MAX slots */
static u_int        ring_allow;   /* PR_ALLOW bit of allow.tessera_keys */
static struct mtx   ring_mtx;
static struct cdev *ring_dev;
static volatile uint64_t ring_epoch = 1;

uint64_t
tessera_keyring_epoch(void)
{
	return (atomic_load_acq_64(&ring_epoch));
}

static struct tkey *
find_locked(const uint8_t key_id[16])
{
	mtx_assert(&ring_mtx, MA_OWNED);
	for (int i = 0; i < TESSERA_KEYRING_MAX; i++)
		if (ring[i].used && memcmp(ring[i].key_id, key_id, 16) == 0)
			return (&ring[i]);
	return (NULL);
}

static void
drop_slot(struct tkey *k)
{
	explicit_bzero(k, sizeof(*k));
}

int
tessera_keyring_get(const uint8_t key_id[16], const uint8_t uuid[16],
    uint64_t group, uint8_t vk[32], uint64_t *high_water, uint32_t *flags)
{
	struct tkey *k;
	int error = ENOENT;

	if (ring == NULL)
		return (ENOENT);
	mtx_lock(&ring_mtx);
	k = find_locked(key_id);
	if (k != NULL && memcmp(k->uuid, uuid, 16) == 0 && k->group == group) {
		memcpy(vk, k->key, 32);
		if (high_water != NULL)
			*high_water = k->high_water;
		if (flags != NULL)
			*flags = k->flags;
		error = 0;
	}
	mtx_unlock(&ring_mtx);
	return (error);
}

void
tessera_keyring_note_gen(const uint8_t key_id[16], uint64_t gen)
{
	struct tkey *k;

	if (ring == NULL)
		return;
	mtx_lock(&ring_mtx);
	k = find_locked(key_id);
	if (k != NULL && gen > k->committed)
		k->committed = gen;
	mtx_unlock(&ring_mtx);
}

static int
ring_ioctl(struct cdev *dev, u_long cmd, caddr_t data, int fflag,
    struct thread *td)
{
	struct tkey *k;
	int error = 0;

	(void)dev;
	if (jailed(td->td_ucred) &&
	    (ring_allow == 0 || !prison_allow(td->td_ucred, ring_allow)))
		return (EPERM);
	switch (cmd) {
	case TKEY_LOAD: {
		struct tkey_load *l = (struct tkey_load *)data;

		if ((fflag & FWRITE) == 0) {
			error = EBADF;
		} else if ((l->flags & ~TKEY_LOAD_OLDER_RO) != 0) {
			error = EINVAL;
		} else {
			mtx_lock(&ring_mtx);
			if (find_locked(l->key_id) != NULL) {
				error = EEXIST;
			} else {
				k = NULL;
				for (int i = 0; i < TESSERA_KEYRING_MAX; i++)
					if (!ring[i].used) {
						k = &ring[i];
						break;
					}
				if (k == NULL) {
					error = ENOSPC;
				} else {
					memcpy(k->key_id, l->key_id, 16);
					memcpy(k->uuid, l->scope_uuid, 16);
					k->group = l->scope_group;
					memcpy(k->key, l->key, 32);
					k->high_water = l->high_water;
					k->flags = l->flags;
					k->committed = 0;
					k->owner_uid = l->owner_uid;
					memcpy(k->owner_label, l->owner_label,
					    sizeof(k->owner_label));
					k->owner_label[sizeof(k->owner_label) - 1]
					    = '\0';
					k->used = 1;
					atomic_add_rel_64(&ring_epoch, 1);
				}
			}
			mtx_unlock(&ring_mtx);
		}
		/* the ioctl buffer is a kernel copy: don't leave the key in it */
		explicit_bzero(l->key, sizeof(l->key));
		break;
	}
	case TKEY_DROP: {
		struct tkey_id *d = (struct tkey_id *)data;
		uint8_t uuid[16];
		uint64_t group = 0;

		if ((fflag & FWRITE) == 0)
			return (EBADF);
		mtx_lock(&ring_mtx);
		k = find_locked(d->key_id);
		if (k != NULL) {
			memcpy(uuid, k->uuid, 16);
			group = k->group;
		}
		mtx_unlock(&ring_mtx);
		if (k == NULL) {
			error = ENOENT;
			break;
		}
		/* A key group's key (encryption §5.4, E4.4): its mounts write
		 * back and forget the group's plaintext while the key is still
		 * here, then wipe their keys once it is gone. */
		if (group != 0)
			tessera_fs_group_closing(uuid, group, 1);
		mtx_lock(&ring_mtx);
		k = find_locked(d->key_id);
		if (k != NULL) {
			drop_slot(k);
			atomic_add_rel_64(&ring_epoch, 1);
		} else
			error = ENOENT;
		mtx_unlock(&ring_mtx);
		if (group != 0)
			tessera_fs_group_closing(uuid, group, 0);
		break;
	}
	case TKEY_GEN: {
		struct tkey_gen *g = (struct tkey_gen *)data;

		mtx_lock(&ring_mtx);
		k = find_locked(g->key_id);
		if (k != NULL)
			g->generation = k->committed;
		else
			error = ENOENT;
		mtx_unlock(&ring_mtx);
		break;
	}
	case TKEY_FORMAT:
		/* dispensatord, unjailed root; never a jail, arcad's included. */
		if ((fflag & FWRITE) == 0)
			return (EBADF);
		if (jailed(td->td_ucred))
			return (EPERM);
		error = tessera_format_dev(td, (struct tkey_format *)data);
		break;
	default:
		error = ENOTTY;
	}
	return (error);
}

static struct cdevsw ring_cdevsw = {
	.d_version = D_VERSION,
	.d_ioctl   = ring_ioctl,
	.d_name    = TESSERA_KEYRING_DEV,
};

int
tessera_keyring_init(void)
{
	struct make_dev_args a;
	int error;

	ring = malloc(sizeof(*ring) * TESSERA_KEYRING_MAX, M_TESSERA_KEYS,
	    M_WAITOK | M_ZERO);
	/* The jail parameter stays registered after unload (the kernel has no
	 * way to remove one); a reload finds the same bit. */
	ring_allow = prison_add_allow(NULL, "tessera_keys", NULL,
	    "Allow the Tessera keyring (/dev/tesserakeys) in the jail");
	mtx_init(&ring_mtx, "tessera_keys", NULL, MTX_DEF);
	make_dev_args_init(&a);
	a.mda_flags = MAKEDEV_WAITOK | MAKEDEV_CHECKNAME;
	a.mda_devsw = &ring_cdevsw;
	a.mda_uid = UID_ROOT;
	a.mda_gid = GID_WHEEL;
	a.mda_mode = 0600;
	error = make_dev_s(&a, &ring_dev, TESSERA_KEYRING_DEV);
	if (error != 0) {
		printf("tessera_fs: /dev/%s: %d\n", TESSERA_KEYRING_DEV, error);
		ring_dev = NULL;
	}
	return (error);
}

void
tessera_keyring_fini(void)
{
	if (ring_dev != NULL) {
		destroy_dev(ring_dev);    /* waits out running ioctls */
		ring_dev = NULL;
	}
	if (ring == NULL)
		return;
	mtx_lock(&ring_mtx);
	explicit_bzero(ring, sizeof(*ring) * TESSERA_KEYRING_MAX);
	mtx_unlock(&ring_mtx);
	mtx_destroy(&ring_mtx);
	free(ring, M_TESSERA_KEYS);
	ring = NULL;
}
