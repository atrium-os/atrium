/*
 * tessera/keyring.h — /dev/tesserakeys, the kernel keyring's ioctl ABI
 * (docs/spec/tessera-encryption.md §4.3). Shared by tessera_fs.ko and its
 * callers (arcad; tessera-keyload in developer builds).
 *
 * A key is loaded for one scope: a volume uuid, and a key group (0 = the
 * volume key). A mount looks the key up by the superblock's master_key_id
 * and its own uuid, so a key loaded for one volume never opens another.
 * Keys are held in kernel memory of their own malloc type, never paged, and
 * zeroed on drop.
 */
#ifndef TESSERA_KEYRING_H_
#define TESSERA_KEYRING_H_

#ifdef _KERNEL
#  include <sys/types.h>
#  include <sys/ioccom.h>
#else
#  include <stdint.h>
#  include <sys/ioctl.h>
#endif

#define TESSERA_KEYRING_DEV   "tesserakeys"
#define TESSERA_KEYRING_MAX   64          /* keys loaded at once */

struct tkey_load {
	uint8_t  key_id[16];
	uint8_t  scope_uuid[16];      /* the volume this key may open */
	uint64_t scope_group;         /* 0: the volume key (VK) */
	uint8_t  key[32];
	uint64_t high_water;          /* §8: lowest generation a mount accepts */
	uint32_t owner_uid;
	uint32_t flags;               /* none yet; must be 0 */
	char     owner_label[32];     /* persona label, NUL-padded */
};

struct tkey_id {
	uint8_t  key_id[16];
};

struct tkey_gen {
	uint8_t  key_id[16];          /* in */
	uint64_t generation;          /* out: highest committed under the key */
};

/* Format a partition sealed (§7.1 step 3) with a loaded key, scope
 * (key_id, uuid, group 0). fd: the partition, open read-write by the caller.
 * total_sectors 0 = the whole partition; journal_sectors 0 = the default;
 * suite 0 = XAES-256-GCM; hash_alg as tessera_format_opts_t. Unjailed
 * callers only. ENOENT: no such key for that uuid; ENOTBLK: not a disk. */
struct tkey_format {
	uint8_t  key_id[16];
	uint8_t  uuid[16];
	uint64_t total_sectors;
	uint64_t journal_sectors;
	int32_t  fd;
	uint32_t suite;
	uint32_t hash_alg;
	uint32_t flags;               /* TKEY_FORMAT_RECOVERY or 0 */
	/* With TKEY_FORMAT_RECOVERY: arcad's recovery slot (§7.4; type 3,
	 * Argon2id, the key wrapped under the code), put in key_slots[0]
	 * under the superblock's HMAC. */
	uint8_t  recovery_slot[256];
};
#define TKEY_FORMAT_RECOVERY 0x1u

/* EEXIST if the id is loaded (drop it first); ENOSPC past the limit. */
#define TKEY_LOAD  _IOW('K', 1, struct tkey_load)
/* ENOENT if not loaded. Mounts keep their derived keys until unmount. */
#define TKEY_DROP  _IOW('K', 2, struct tkey_id)
#define TKEY_GEN   _IOWR('K', 3, struct tkey_gen)
#define TKEY_FORMAT _IOW('K', 4, struct tkey_format)

#ifdef _KERNEL
/* For the mount path. Copies the key for (key_id, uuid, group) into vk and
 * its high-water into *high_water: 0, or ENOENT (no such key for this
 * scope). The caller wipes vk after deriving its keys. */
int  tessera_keyring_get(const uint8_t key_id[16], const uint8_t uuid[16],
         uint64_t group, uint8_t vk[32], uint64_t *high_water);
/* Record a committed generation under key_id (TKEY_GEN reports the max). */
void tessera_keyring_note_gen(const uint8_t key_id[16], uint64_t gen);
struct thread;
int  tessera_format_dev(struct thread *td, const struct tkey_format *f);
int  tessera_keyring_init(void);
void tessera_keyring_fini(void);
#endif

#endif /* TESSERA_KEYRING_H_ */
