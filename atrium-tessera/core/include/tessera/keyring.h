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
	uint32_t flags;               /* TKEY_LOAD_OLDER_RO or 0 */
	char     owner_label[32];     /* persona label, NUL-padded */
};
/* §8 "Open read-only?": the person verified opening an older copy. The
 * key opens its volume below high_water, and only read-only: a mount that
 * would write is refused with EROFS, whatever its generation. */
#define TKEY_LOAD_OLDER_RO 0x1u

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

/* Key-group ioctls on the mounted volume (not the keyring device; see
 * tessera_fs.c TESSERA_IOC_GROUP_CREATE): a directory's descriptor. */
struct tessera_ioc_group {
	uint8_t  key_id[16];
	uint64_t group_id;
	uint32_t suite;               /* 0: XAES-256-GCM */
	uint32_t pad;
};
#define TESSERA_IOC_GROUP_CREATE _IOW('T', 7, struct tessera_ioc_group)
#define TESSERA_IOC_QUOTA_GROUP  _IOW('T', 8, uint64_t)
/* Crypto-erase a locked group (§9, E4.5): its domains' inodes go, each
 * domain's root becomes an empty plain directory, the domain and group
 * records go, and a GC pass frees the group's packs from their preludes. */
#define TESSERA_IOC_GROUP_ERASE  _IOW('T', 9, uint64_t)
/* What a directory is, for atrium-volumes' expected groups (§5.4, E4.6):
 * the volume's uuid, the directory's quota domain and whether it is that
 * domain's root, the domain's key group and that group's key id, and the
 * state (TESSERA_GROUP_INFO_*): OPEN only when the group's key is loaded
 * and the domain record carries the group's MAC. Unjailed root only.
 * A locked group's folder can't be opened (E4.4), so the ioctl goes to any
 * directory of the volume (its parent, say) with the folder's inode number
 * from stat(2) in `inode`; 0 asks about the directory itself. */
struct tessera_ioc_group_info {
	uint8_t  volume_uuid[16];
	uint8_t  key_id[16];          /* the group's; zero for no group */
	uint64_t domain_id;           /* 0: in no domain */
	uint64_t group_id;            /* 0: the domain is in no group */
	uint32_t flags;
	uint32_t inode;               /* in: st_ino, or 0 */
};
#define TESSERA_GROUP_INFO_ROOT      0x1u  /* the domain's root */
#define TESSERA_GROUP_INFO_OPEN      0x2u
#define TESSERA_GROUP_INFO_LOCKED    0x4u
#define TESSERA_GROUP_INFO_BAD       0x8u  /* EINTEGRITY: record or MAC */
#define TESSERA_GROUP_INFO_NO_RECORD 0x10u /* names a group with no record */
#define TESSERA_IOC_GROUP_INFO   _IOWR('T', 10, struct tessera_ioc_group_info)

/* Scrub (E4.7, tessera-encryption.md §13.1): make deleted file contents
 * unreadable from the disk. Retires every retained snapshot, runs a GC pass
 * that also rebuilds partly-dead plain packs without their dead blobs, then
 * zeroes every free sector of the data zone and the journal's old records
 * (a plain journal's dirent records name files). atrium-volumes runs it after
 * moving a plain folder into a key group. Unjailed root only; any
 * directory of the volume. Takes as long as writing the free space. */
struct tessera_ioc_scrub {
	uint32_t flags;               /* in: 0 */
	uint32_t pad;
	uint64_t snapshots_retired;   /* out, from here on */
	uint64_t packs_freed;
	uint64_t packs_compacted;
	uint64_t blobs_dropped;
	uint64_t packs_young;         /* partly dead, kept for the grace period */
	uint64_t zeroed_sectors;
	uint64_t journal_blocks;      /* zeroed past the last checkpoint */
};
#define TESSERA_IOC_SCRUB        _IOWR('T', 11, struct tessera_ioc_scrub)

/* EEXIST if the id is loaded (drop it first); ENOSPC past the limit. */
#define TKEY_LOAD  _IOW('K', 1, struct tkey_load)
/* ENOENT if not loaded. Mounts keep a volume's derived keys until unmount;
 * a key group's are flushed and wiped at the drop (E4.4). */
#define TKEY_DROP  _IOW('K', 2, struct tkey_id)
#define TKEY_GEN   _IOWR('K', 3, struct tkey_gen)
#define TKEY_FORMAT _IOW('K', 4, struct tkey_format)

#ifdef _KERNEL
/* For the mount path. Copies the key for (key_id, uuid, group) into vk, its
 * high-water into *high_water and its TKEY_LOAD_* flags into *flags (either
 * may be NULL): 0, or ENOENT (no such key for this scope). The caller wipes
 * vk after deriving its keys. */
int  tessera_keyring_get(const uint8_t key_id[16], const uint8_t uuid[16],
         uint64_t group, uint8_t vk[32], uint64_t *high_water, uint32_t *flags);
/* Bumped by every TKEY_LOAD and TKEY_DROP: a mount holding derived group
 * keys looks again when it changes (encryption §5.4, E4.2). */
uint64_t tessera_keyring_epoch(void);
/* In tessera_fs.c: TKEY_DROP of a group's key calls this with closing = 1
 * before the key goes (its mounts refuse the group's vnode ops, write back
 * and drop its plaintext) and closing = 0 after (they wipe its keys). May
 * sleep; called without the keyring lock. */
void tessera_fs_group_closing(const uint8_t uuid[16], uint64_t group,
         int closing);
/* Record a committed generation under key_id (TKEY_GEN reports the max). */
void tessera_keyring_note_gen(const uint8_t key_id[16], uint64_t gen);
struct thread;
int  tessera_format_dev(struct thread *td, const struct tkey_format *f);
int  tessera_keyring_init(void);
void tessera_keyring_fini(void);
#endif

#endif /* TESSERA_KEYRING_H_ */
