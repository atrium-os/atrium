# Tessera at-rest encryption

**Status: accepted 2026-11-17** (Shankar: "continue as planned", all §14 decisions as
recommended). E0 done; cipher changed to XAES-256-GCM after E0 (decision 3).
**Scope:** encrypting Tessera on disk: what is encrypted and with which key, how
metadata is authenticated so that the kernel never parses bytes an attacker wrote, where
keys live (Arca), crypto-erase, dedup under encryption, rollback, and trusted Tessera
sticks. It fills in the `ENCRYPTED` flag that [tessera-fs.md](tessera-fs.md) §3.1
reserves and replaces the "convergent encryption" item in its §17.
**Background:** personas.md §4–5 (persona folders with their own key, persona sticks,
crypto-erase when a persona is removed) and device-assignment.md §4.6 (a stick is trusted
only when it unlocks with a key this machine holds, and only an authenticated stick may
be mounted by the kernel) both wait on this. Shankar 2026-11-17: "then should we start on
the two parts first?"

Related: [tessera-fs.md](tessera-fs.md) (§3 superblock, §5 packs, §10 B+trees, §20 the
dedup existence oracle), [tessera-quotas.md](tessera-quotas.md) (domains),
[arca.md](arca.md) (§6 key hierarchy, classes, machine store),
[personas.md](personas.md), [device-assignment.md](device-assignment.md) §4.6 and §6.9,
[atrium-volumes.md](atrium-volumes.md), [homes.md](homes.md).

## 1. Short answer

- **This is not full disk encryption.** Full disk encryption (GELI, BitLocker,
  FileVault) encrypts a whole device under one key that is unlocked at boot. Here,
  different data gets different keys that open and close separately. §11 covers how the
  two fit together.
- **There are two layers:**
  - A **sealed volume** encrypts and authenticates a whole Tessera filesystem under one
    key: every metadata block, every pack and every blob. A stick is a sealed volume
    whose key is held in one persona's Arca.
  - An **encrypted domain** gives one part of a volume its own key. This is what
    atrium-volumes calls a volume: a directory tree with a quota domain. A person's
    Personal folders and each named persona's folders are encrypted domains on the
    internal Tessera filesystem, keyed in that person's and persona's Arca. Contents,
    names, symlink targets and extended attributes are encrypted under the domain key,
    so root on the running machine cannot read a logged-out person's files, and a thief
    with the disk cannot read anyone's.
  - The **internal Tessera filesystem itself is not sealed**: it is the root the loader
    boots from and where arcad lives, so no Arca key can open it before boot. Sealing it
    is the full disk encryption question of §11.
- **AEAD, never XTS.** On a sealed stick, every block, pack and blob is sealed with an AEAD
  (XAES-256-GCM, or XChaCha20-Poly1305 on chips without AES instructions, §5.1). Each parent records its children's authentication tags,
  so the tree checks itself from the superblock down. The kernel verifies a block before
  it decodes a single byte of it. Replacing any block with an older copy of itself is
  detected.
- **Keys never leave Arca except into the kernel.** A volume key is a new Arca key kind
  that no process can read. arcad writes it into a kernel keyring through a device only
  arcad can open, and the kernel uses it from there.
- **Crypto-erase** destroys the Arca item and drops the kernel's copy. On the internal
  disk, the domain's packs are then freed. Removing a persona works this way, and so does
  wiping a stick by forgetting its key.
- **Dedup survives inside one key** and does not cross keys. Content ids in an encrypted
  domain are keyed hashes (§6), so the convergent encryption that tessera-fs.md §17
  planned is dropped. That also closes the existence oracle across domains
  (tessera-fs.md §20.3).
- **Rollback:** each key's Arca item keeps the highest generation seen. A volume or
  domain older than that is refused, and the person can open it read-only after user
  verification.
- **Trusted sticks** (sealed, and their key is in an Arca persona of the session's
  person) are mounted by the kernel's `tessera_fs`. Everything else keeps the jailed FUSE
  path. A sealed stick without a key is shown as locked and never decoded.

## 2. Words

| word | meaning here |
|---|---|
| **volume** (Tessera) | one Tessera filesystem: a partition, a stick, the internal Tessera at `/` |
| **sealed volume** | a Tessera volume with `INCOMPAT_SEALED` set: everything on it is AEAD-sealed under its volume key. Sticks, and any other Tessera disk that is not the one the machine boots from |
| **domain** | a tessera-quotas.md domain: a directory tree with a quota and a dedup policy. atrium-volumes calls one a "volume" (atrium-volumes.md §6.1), and so does personas.md §4.1 ("one volume per named persona") |
| **encrypted domain** | a domain whose record names a key group (below) |
| **key group** | one key in Arca and the domains encrypted with it. All of a person's Personal domains (home folders, app data under the home, workspaces) form one group, and each named persona forms its own. Dedup works within a group and never across groups (§6) |
| **VK** | volume key: 32 random bytes, one per sealed volume |
| **GK** | group key: 32 random bytes, one per key group |
| **key id** | 16 random bytes naming a VK or GK. It is not secret and is written in the clear on disk so the system knows which key to ask Arca for |

Read "volume" in personas.md §4.1 and §4.2 as "domain" in this spec's sense. A persona's
folders stay on the internal Tessera filesystem as one or more domains. They are not a
partition of their own, so no space needs to be set aside when a persona is made.

## 3. What each layer protects against

| attacker | a sealed stick | the internal disk: encrypted domains |
|---|---|---|
| finds a lost **stick** or steals the **machine, powered off** | sees the stick's size and key id; reads nothing; can't make any kernel parse anything | reads no person's file contents, names, symlinks or xattrs: group keys come from each person's passcode through Arca (Argon2id, and the TPM where there is one) |
| **root on the running machine** while a person is logged out | n/a (a stick is unmounted at logout) | cannot read that person's files or names: their group keys are not in the kernel until they log in |
| root while the person is logged in | everything | everything. Not in scope, as for every OS |
| edits the disk offline | every block is authenticated: edits refused before decoding | contents are authenticated, so cannot be changed. The volume's own metadata is not, so the attacker can edit what the next row covers |
| **restores an old image** | refused by the high-water mark (§8) | a key group's generation is authenticated by its key, so restoring a whole group is refused (§8), unless Arca's store, which is on the same disk, is rolled back with it and there is no TPM to notice. Pointing one file back at an older blob that GC has not yet freed is not detected |
| a **compromised app** of one persona | n/a (it already sees only its own folders through its jail) | n/a |

What stays visible on the internal disk to someone without the person's key: how many
files a domain has, their sizes, times, owners and modes (the inode records, which live
in the volume's shared inode table), and how many packs each group has and how big they
are. fscrypt and ZFS native encryption leak about the same. Hiding these as well means
moving a group's inode records into a tree of its own, or sealing the internal volume
(§11); §12 lists both.

**The kernel and the internal disk.** device-assignment.md §4.6 requires authenticated
metadata before the kernel mounts *removable* media. The internal disk is a different
case: it is the disk the machine boots from, and an attacker who can rewrite it offline
can also rewrite the kernel on it unless the boot chain is verified or encrypted, which
is §11's subject. The kernel's Tessera decoders are hardened Rust (D3) either way.

## 4. Keys

### 4.1 Where they live

```
Arca (arca.md §6.1)
 ├─ machine store (TPM-sealed root, or machine.key)          class: after boot
 │    └─ VolumeKey  VK of a non-boot internal Tessera disk, if any
 └─ person <p>, persona <q>                                   class: after-first-unlock
      ├─ VolumeKey  VK of each of q's sticks
      └─ VolumeKey  GK of q's key group (Personal or a named persona)
```

- **Kind.** New Arca key kind `VolumeKey` (`0x103`, beside `Symmetric = 0x102`). It is a
  *key*, not a bearer secret (arca.md §2): it is used and never given. No op returns it.
  The only use is `LOAD_VOLUME_KEY` (§4.3).
- **Item fields** beyond Arca's usual ones: `key_id[16]`, `scope` (a volume uuid, or a
  volume uuid plus a group id), `high_water` (u64, §8), and `label` (what Scrinium and
  Curia show, e.g. "Work stick, 64 GB, made 2026-11-20").
- **Class: after-first-unlock** for person keys, so a persona's folders stay readable
  while the screen is locked and background work continues (personas.md §4.1). A
  machine-store VK, for a second internal Tessera disk that holds no person's data, is
  usable from boot.
- **Personas never share items** (arca.md §2), so a key in Work's store cannot open
  anything for Personal, even though both belong to one person.

### 4.2 Derived keys

Everything on disk uses subkeys made with HKDF-SHA-256 from VK or GK. `info` is the label
followed by the volume uuid (and the group id for GK):

| subkey | from | used for |
|---|---|---|
| `K_meta` | VK | on a sealed volume: sealing metadata blocks, journal records, pack headers and indexes of packs outside any group, and the superblock MAC |
| `K_blob` | VK | on a sealed volume: sealing blobs outside any group |
| `K_id` | VK | on a sealed volume: content ids for domains outside any group (§6) |
| `K_gmeta` | GK | sealing pack headers and indexes of the group's packs, names in journal records, and the MACs on the group's and its domains' records (§5.4) |
| `K_gblob` | GK | sealing the group's blobs |
| `K_gid[d]` | GK, plus domain d's salt if `salted` | content ids in domain d (§6) |
| `K_name` | GK | keyed directory-name hashes (replaces `tessera_dir_name_hash`'s plain SHA-256 in the group's directories) |
| `K_cdc` | GK | seed for the group's FastCDC gear table (§6.2) |
| `K_check` | VK or GK | `check = HMAC(K_check, "tessera key check")`, stored beside the key id so the wrong key is caught before any decryption |

Rotating a key means rewriting everything under it, which is a repack to a new key id.
That is possible but not part of the first build (§12).

### 4.3 Getting a key to the kernel

Arca's first promise is "use without read" (arca.md §1). Here the user of the key is the
kernel, so the key goes straight from arcad into the kernel and no other process ever
holds it.

- **`/dev/tesserakeys`**, a character device of the `tessera_fs` module, mode 0600 and
  owned by `_arca`, in arcad's devfs ruleset only. arcad opens it at start, before it
  enters Capsicum, and limits the descriptor to `CAP_IOCTL` with these ioctls:
  - `TKEY_LOAD { key_id, scope, key[32], high_water, owner (uid, persona label) }`
  - `TKEY_DROP { key_id }`
  - `TKEY_GEN { key_id } -> generation`: the highest generation the kernel has committed
    under this key since it was loaded (§8)
- **The keyring** holds a key only while the volume or group is in use. The memory is
  malloc'd from its own type, never paged, and zeroed with `explicit_bzero` on drop.
  Kernel crash dumps are not set up on Atrium yet. When they are, they must be
  encrypted (FreeBSD `dumpon -k`) or must leave out keyring pages.
- **New arcad ops** (wire codes assigned when built), callable by named services only.
  The callers are dispensatord for sticks, and atrium-volumes, run by portcullisd at
  login, for key groups:
  - `LOAD_VOLUME_KEY { key_id, scope }`: loads the key, if it belongs to the person
    whose session is active and unlocked (or to the machine store). The caller gets
    `OK`, `NO_KEY` or `LOCKED`; it never sees the key.
  - `MINT_VOLUME_KEY { persona, scope, label }`: arcad generates the key inside itself,
    stores it, loads it and returns `key_id` and `check`.
  - `RECORD_GENERATION { key_id }`: arcad asks the kernel (`TKEY_GEN`) and raises the
    item's high-water mark (§8).
  - `FORGET_VOLUME_KEY { key_id }`: crypto-erase (§9), with user verification
    (Sigillum `VERIFY_USER`).
  - `ADOPT_RECOVERY { slot bytes, scope, persona, keep }`: §7.4.
- **The kernel trusts the scope.** A key loaded for volume uuid U (and group g) opens
  only U (and g). A mount of a different volume naming that key id fails, so a copied
  stick header cannot be used to point a key at someone else's disk.

## 5. On disk

§5.2 and §5.3 apply only to sealed volumes, which have the new incompat bit set. §5.4 and
§5.5 (key groups, group packs and blobs) apply to any volume with key groups, sealed or
not, and set a second incompat bit. Nothing changes for volumes with neither.

The kernel module and the core's `tessera_volume_open` already refuse unknown incompat
bits, and the read-only reader that the loader and atrium-capsa-fuse use opens volumes
through `tessera_volume_open`, so it refuses them too (test_volume's
`test_unknown_incompat_refused`, 2026-11-17).

### 5.1 The AEAD

**XAES-256-GCM** ([c2sp.org/XAES-256-GCM](https://c2sp.org/XAES-256-GCM)) with a fresh
random 24-byte nonce for every sealed unit, from the kernel's `arc4random_buf`. Shankar
chose it over XChaCha20-Poly1305 on 2026-11-17 after E0 (below).

- Random 192-bit nonces mean a nonce never needs to be derived from state. A crash that
  reuses a generation number, a replayed journal, or two machines writing the same stick
  never risk nonce reuse. That rules out plain AES-GCM with 96-bit nonces, which this
  volume's lifetime write count would push towards the birthday bound, and which is
  catastrophic on reuse. XAES derives a per-unit AES-256-GCM key from the first 12 nonce
  bytes (CMAC-based, two AES blocks, with `K1` computed once per key) and uses the last
  12 as the GCM nonce.
- It runs on the AES and carry-less multiply instructions that every amd64 CPU of the
  last decade, Apple silicon, Graviton, Ampere and the Raspberry Pi 5 have, at 2 to 5
  times the speed of XChaCha20-Poly1305 on the same OpenSSL assembly (E0).
- **Suite byte.** 1 = XAES-256-GCM (default), 2 = XChaCha20-Poly1305. A volume is made
  with suite 2 only on a CPU without AES instructions (arm64 without the crypto
  extension, such as the Raspberry Pi 4), where ChaCha on NEON is several times faster
  than AES and software AES is hard to make constant-time. Every kernel reads both
  suites, so a stick made on either kind of machine opens on the other, just slower.
  Without AES instructions the kernel's AES is `ossl(4)`'s constant-time vector AES
  (vpaes), never the table-based `rijndael` code.
- Key slots (§5.2) keep XChaCha20-Poly1305 for their 32-byte wraps, as format.h
  reserved them: arcad does those in software, a few per mount.

A sealed unit is `nonce[24] | ciphertext | tag[16]`. The AAD always starts with the
volume uuid and a one-byte object kind, so no unit can be moved to another volume or
reinterpreted as another kind.

#### E0: what each suite costs

Measured 2026-11-17 with `scripts/tessera-crypt-bench.sh` (atrium-tessera/bench/crypt):
one core, sealing or opening one unit at a time with a fresh 24-byte nonce, every unit
checked, after the c2sp XAES-256-GCM vectors and the draft-irtf-cfrg-xchacha A.3.1
vector pass on every path. *sodium-ref* is the FreeBSD kernel KPI's own code (libsodium
portable ChaCha20 and poly1305-donna from `sys/contrib`, at -O2); the *ossl* rows are
OpenSSL's assembly, the code of the kernel `ossl(4)` driver. Userspace numbers: the
kernel adds OCF dispatch and `fpu_kern` regions, which E1 measures in a VM.

| MB/s, seal (open within 10%) | x86_64 cloud (Xeon 2.3 GHz, VAES, OpenSSL 3.0.13) 4 KiB / 64 KiB | Apple M4 Max (OpenSSL 3.6.2) 4 KiB / 64 KiB |
|---|---|---|
| XChaCha20-Poly1305, kernel KPI (sodium-ref) | 484 / 521 | 711 / 752 |
| XChaCha20-Poly1305, ossl asm | 1626 / 3569 | 1863 / 2198 |
| XAES-256-GCM, ossl asm | 4343 / 11705 | 5781 / 9187 |
| AES-256-GCM, fixed key (ceiling, not usable) | 4524 / 11799 | 5883 / 9238 |
| for scale: SHA-256 (content hashing), 64 KiB | 1837 | 3415 |

What it means:

- **The kernel KPI is out.** At 0.5 to 0.75 GB/s it would cost more than the content
  hashing Tessera already does. Whichever suite, the kernel uses OCF and ossl (§10).
- **XAES costs almost nothing over plain GCM** (1 to 2%), so its random 192-bit nonces
  are free.
- **XChaCha is slower than SHA-256 on the M4**, so it would be a second bottleneck. A
  64 KiB chunk that is hashed and sealed runs at about 1.3 GB/s with XChaCha and 2.5 GB/s
  with XAES on the M4 (1.2 and 1.55 GB/s on the x86 machine). On metadata-sized 4 KiB
  units XAES is 2.7 to 3 times faster on both.

### 5.2 Superblock

The superblock stays in the clear: it holds sector numbers, generations and sizes, but
nothing of the content. It is authenticated rather than encrypted:

- `incompat_flags` gains **`TESSERA_INCOMPAT_SEALED` (bit 1)**. Bit 0 is already
  `HASH_ALG` in format.h, and the spec's §3.1 table is corrected to match.
  `feature_flags` bit 0 `ENCRYPTED` is also set, for tools that only look.
- `incompat_flags` also gains **`TESSERA_INCOMPAT_KEY_GROUPS` (bit 2)**, set on any
  volume (sealed or not) once its first key group exists, so an older kernel cannot
  write plain blobs into an encrypted domain.
- `encryption_flags` (reserved in format.h as "bit 0 = AES-XTS active, bit 1 =
  convergent") is redefined, which is safe because no volume has ever set it: bit 0 =
  sealed, bit 1 = has key groups. The "convergent" bit is retired (§6).
- `reserved_e0` becomes the **suite** byte (1 = XAES-256-GCM, 2 = XChaCha20-Poly1305; §5.1).
- `master_key_id` holds the VK's key id. A new 32-byte field from the trailing slack
  holds `check` (§4.2).
- **`hmac[32]`** (reserved in format.h) holds `HMAC-SHA-256(K_meta, superblock with
  crc32 and hmac zeroed)`. On a sealed volume the HMAC replaces the CRC as the check, and
  the CRC is still written so tools can detect corruption without a key.
- **Root tags.** For every tree root in the superblock (inode, pack registry, free
  extents, quota, blob index, dead extents, snapshots), the slack gains the root block's
  16-byte AEAD tag. These are 112 bytes of the 1692 bytes of slack.
- The advisory counters (`total_blob_count`, `total_pack_count`) and the times
  (`format_time`, `last_mount_time`) are written as zero on a sealed volume. Their real
  values go in a 128-byte sealed record in the slack, so a lost stick does not say when
  it was last used.
- **Key slots.** The eight `tessera_key_slot_t` slots stay. This design uses two types:
  - **Arca** (new type 7): no wrapped key, just the key id and `check`. This is the
    normal case, and says "the key is in some machine's Arca".
  - **recovery** (type 3, already reserved): VK wrapped with XChaCha20-Poly1305 under
    `Argon2id(recovery code, salt)`, for sticks (§7.4).

  Passphrase, TPM2, FIDO2, PKCS#11 and keyfile slots (types 1, 2, 4, 5, 6) are not used.
  Arca already mixes the TPM and FIDO2 into its own root (arca.md §6.1), so one secret
  store does that job once. The slot types stay reserved.

At mount the kernel checks magic, the incompat bits and the HMAC (with the key from the
keyring) before it reads any other field. Every number it then uses (zone starts and
lengths, root sectors) has passed the HMAC, and `tessera_superblock_layout` still bounds
them as it does today.

### 5.3 Metadata blocks

B+tree nodes (every tree), the journal header and journal records are 4 KiB blocks. On a
sealed volume each one is a sealed unit: 4056 bytes of payload, then the 24-byte nonce
and 16-byte tag at the end of the block.

- **AAD** = volume uuid | kind | sector | tree kind (nodes) or sequence (journal).
- **Parents carry tags.** An internal node's entry becomes `(key, child_sector,
  child_tag[16])`, and the superblock carries the roots' tags (§5.2). A node is accepted
  only if its AEAD verifies *and* its tag equals the one its parent recorded. Each write
  gets a new random nonce, so an older copy of the same node at the same sector has a
  different tag and is refused. Together these make a Merkle tree over all metadata,
  using tags the AEAD computes anyway.
- **Capacity.** An inode leaf still holds 26 records (4056 − 32 header = 4024 bytes, /
  152). Internal fan-out drops by 16 bytes an entry. Journal record bodies shrink from
  4068 to 4028 bytes per block.
- **Journal chain.** Each record's AAD also includes the previous record's tag, so
  records cannot be reordered, dropped from the middle or spliced in from an older
  journal. Cutting the tail looks like a crash before the last commit, which replay
  already handles. Because the superblock is rewritten at every commit (tessera-fs.md
  §18 step 13), the cut cannot undo anything the superblock already covers.
- **Names in journal records** (`DIR_INSERT`, `DIR_REMOVE`) of an inode in an encrypted
  domain are sealed a second time under `K_gmeta`, so the volume key alone never shows a
  name.

### 5.4 Key groups, domains and inodes

- **Key-group records** sit in the quota tree beside the domain records, under their own
  key prefix: `group_id`, `key_id`, `check`, suite, the group's generation (§8) and a
  16-byte MAC under `K_gmeta` over all of these.
- **A domain record** names its group in 8 of its 32 reserved bytes (0 = no group) and
  carries a 16-byte MAC under `K_gmeta` over its id, root inode, dedup policy, salt and
  group. So on an unsealed volume too, nobody without the key can move a domain out of
  its group, change its salt, or roll the group's generation back.
- **Expected groups.** atrium-volumes keeps, per person, which domains must be encrypted
  and under which key id (the volume's `encrypt = "persona"`, §13 E4). At login it
  refuses to mount a domain whose record names no group or another key. This stops an
  offline edit that strips the group so that new files are written in the clear.
- **Inodes.** Inode records stay in the volume's inode table (sealed under `K_meta` on a
  sealed volume). The record already has `quota_domain` (format.h). The kernel refuses
  a directory entry whose inode has a different domain from its parent, unless it is
  the root of the domain it names. So an offline edit cannot pull a file out of its
  group either. A rename into or out of a group, or between two groups, answers `EXDEV`,
  so the mover copies (which re-encrypts) and deletes, as across filesystems.
- **Locked.** The kernel refuses access (`EACCES`, logged as "locked") to the contents,
  names and xattrs of an inode whose group key is not loaded. `stat` on a locked inode
  still works for root, which is the leak §3 lists.

### 5.5 Packs and blobs

- **A pack belongs to one key group, or to none.** Its header gains `key_group` (u64,
  0 = none) and the blob descriptor grows to hold a nonce and a tag. A pack never mixes
  groups. That is what lets GC and crypto-erase handle a group without its key (§9).
- **The pack's front matter** (header, blob index, bloom) is one sealed unit: under
  `K_gmeta` for a group's pack, under `K_meta` for other packs on a sealed volume. Its
  plaintext prelude is a magic, the key group, the unit's length and the nonce. On a
  sealed volume the pack-registry entry (sealed under `K_meta`) records the front
  matter's tag, and so the registry's value grows to 80 bytes there.
- **Each blob** is a sealed unit under `K_gblob` or `K_blob`, with AAD = volume uuid |
  kind | key group | content id. So a blob cannot be swapped for another in the same
  pack, nor moved to another group.
- **Compression** (tessera-fs.md §17, reserved) happens before sealing. Compressing a
  blob that mixes attacker-chosen bytes with a secret can leak through the size, so
  compression stays off for `salted` domains.

## 6. Dedup under encryption

### 6.1 Keyed content ids

In a key group, and everywhere on a sealed volume, a content id (chunk hash, manifest
hash, and the keys of the pack index, blob index and bloom) is a **keyed hash** of the
plaintext:

- keyed BLAKE3 when the volume's `hash_alg` is BLAKE3, HMAC-SHA-256 when it is SHA-256;
- keyed with `K_gid[d]` for domain d of a key group, with `K_id` outside groups on a
  sealed volume. Outside groups on the unsealed internal volume, ids stay plain SHA-256
  or BLAKE3 as today: that is where the OS and app trees live.

So:

- **Within one key group,** identical content has identical ids and dedups as today. The
  domain's dedup policy (`global`, `deferred`, `salted`, tessera-fs.md §20.2) still
  applies inside the group. For example, two app overlays of the same person under
  `deferred` show no free-space change, and a `salted` domain's ids differ from its
  neighbours'. That is §20.3's requirement that the domain salt be composed into the
  key.
- **Across groups and across volumes,** the same bytes have unrelated ids. A probe in one
  persona says nothing about another persona or another person: the existence oracle
  across domains (§20.1) is closed by construction, as `salted` closes it today.
- **Offline,** a thief with a disk cannot ask "does this disk hold file X?" by hashing
  X, because the ids need the key. Plain SHA-256 ids would allow that.

**Why not convergent encryption** (tessera-fs.md §17): encrypting each blob under a key
derived from its own content makes ciphertexts equal across keys, which is useful only
for dedup between machines or people who don't share a key. Atrium doesn't want that
for private data (it *is* the oracle), and public content (apps, OS images) is stored
outside any group, with plain ids, dedup across the whole machine and cross-host
addressing as today. So convergent encryption buys nothing here and is dropped.

### 6.2 Chunk sizes

FastCDC boundaries depend only on content, so the sequence of chunk lengths can
fingerprint a known file even when every chunk is encrypted. Published attacks on
encrypted backup tools that use content-defined chunking, such as restic and borg, do
exactly this. An encrypted group's FastCDC gear table is therefore generated from
`K_cdc`. This changes boundaries only inside the group, which is the only place they
dedup anyway. Domains outside any group keep the public table, because cross-host
manifest determinism (tessera-fs.md §6.6) matters for app trees.

### 6.3 Send and receive

The diff stream (tessera-fs.md §16) carries content ids. From a sealed volume it has two
modes:

- **plain**: blobs are decrypted and re-addressed with SHA-256 for the receiver. This is
  for moving data to another machine; refused for `salted` domains as today.
- **sealed**: ciphertexts and keyed ids are copied as they are, and the receiver needs
  the same key. This fits backups to a disk or server you don't trust (personas.md
  §4.2). It is designed here so the format allows it, and built with backups.

## 7. Sticks

### 7.1 Making a sealed stick

Scrinium's "Make this a Work stick" (personas.md §5.1), or "Encrypt this stick" for
Personal:

1. Scrinium asks dispensatord to format the partition (a new `FORMAT` media op, root or
   the active session's user, the same rule as `EJECT`) and names the persona. It shows
   the person what will be erased first.
2. dispensatord calls arcad `MINT_VOLUME_KEY` under that persona. arcad makes the VK,
   stores it, loads it into the kernel and returns the key id.
3. The kernel formats the partition sealed (a `TESSERA_IOC_FORMAT` ioctl on the
   partition, with the key id). The kernel module already links the Tessera core that
   mkfs uses. Formatting in the kernel means no process ever holds the key, not even
   mkfs.
4. If the person asked for a recovery code (§7.4), arcad produces one, wraps the VK into
   slot type 3, and Scrinium shows the code once.
5. The stick is mounted as a trusted stick (§7.2).

`mkfs.tessera --sealed --key-file` exists only in developer builds, for tests.

### 7.2 Plugging one in

1. dispensatord's FUSE probe (atrium-capsa-fuse, jailed, device-assignment.md §6.9) reads
   the superblock and reports `SEALED`, the key id, `check` and the volume uuid. It
   decodes nothing else. Apart from a recovery slot (§7.4), these fields are the only
   part of a sealed stick that any process parses, and they have fixed sizes.
2. dispensatord calls arcad `LOAD_VOLUME_KEY { key_id, scope = uuid }`. arcad looks in
   the active, unlocked person's stores (every persona of that person) and checks
   `check`.
3. **Key found:** the stick is trusted. dispensatord asks jaild to mount it with the
   kernel's `tessera_fs` (option `key=<key id>`) at `/media/<part>`. This is the trusted
   column of device-assignment.md §4.6. The mount carries the persona: Scrinium and the
   picker show it with that persona's chip, and only that persona's instances get it
   (personas.md §5.1). Lock, logout, eject and the one-writer rule work as for FAT
   sticks (device-assignment.md §6.5, §4.8). At eject and unmount, dispensatord calls
   `RECORD_GENERATION` and then the key is dropped.
4. **No key:** the stick shows as **Locked** in Forum's stick menu and in Scrinium
   ("Encrypted stick from another machine or persona"), with **Enter recovery code**
   when it has a recovery slot, and **Erase and format**. Nothing on it is decoded,
   by the kernel or by any process.

### 7.3 Not mine, but a person here has the key

Keys are looked up in the active person's stores only. If someone else on this machine
owns the stick, it shows as Locked to the session that has it plugged in. That is the
same as for a stranger's stick.

### 7.4 Recovery codes

A key that exists only in one machine's Arca dies with that machine. A recovery code
lets the owner open the stick elsewhere.

- **The code:** 128 random bits plus a checksum, shown as 6 groups of 5 base-32
  characters, like BitLocker's recovery key. Argon2id (Arca's cost setting) derives the wrapping key.
- **Offered at format.** Default **off for named personas**, because the point of a Work
  stick is that leaving the job ends access (personas.md §8, "a lost Work stick"), and
  **on for Personal**, where losing the laptop should not lose the stick. Curia says
  plainly that while a recovery code exists, forgetting the key here does not erase the
  stick (§9).
- **Using it:** the code is typed into an Arca credential field in a system sheet
  (arca.md §8.3; the app never sees it). dispensatord passes the slot's 256 bytes to
  arcad's `ADOPT_RECOVERY`. arcad parses the slot, a fixed record, derives the key,
  unwraps the VK and checks `check`. Then it either keeps the VK in the persona the
  person picks ("Keep on this machine") or loads it for this session only.
- **Removing the code** later (Scrinium, stick settings) overwrites the slot in both
  superblock pairs, through the mounted volume.

## 8. Rollback

Per-block tags (§5.3) stop anyone from swapping in an older copy of *part* of a volume.
They cannot stop someone from restoring *the whole* volume, or a whole domain, from an
earlier image, because that image is consistent. The high-water mark does:

- **Generation.** Every commit raises the superblock generation. Every commit that
  touches a key group also raises that group's generation, a u64 in the key-group record,
  covered by the record's MAC (§5.4).
- **High-water in Arca.** The kernel tracks the highest generation committed under each
  loaded key (`TKEY_GEN`). arcad copies it into the item's `high_water` at eject,
  unmount and logout, and every 5 minutes while mounted (`RECORD_GENERATION`, called by
  dispensatord and atrium-volumes).
- **At load,** arcad passes `high_water` with the key. The kernel refuses to mount a
  volume, or to unlock a group, whose authenticated generation is lower.
- **Opening an older copy deliberately.** Restoring a backup is a legitimate reason.
  Scrinium offers "This stick is older than the last time it was used here. Open
  read-only?", behind user verification. Writing to it, or adopting it as current, is a
  second verified step that lowers the mark.
- **What it doesn't cover:**
  - Writes since the last recording: up to 5 minutes, or the last session before a
    crash.
  - Pointing one file of a group back at an older blob of the same group that GC has
    not freed yet, by editing the unsealed inode table offline (§3).
  - An attacker who can also roll back Arca's own store. With a TPM, arca.md §6.1's NV
    counter covers the store and so covers these marks. Without a TPM there is no
    anchor, and setup already says the no-TPM machine is weaker.

## 9. Crypto-erase

| what | how | what is left |
|---|---|---|
| **a stick** ("Forget this stick's key") | Sigillum `VERIFY_USER`, then `FORGET_VOLUME_KEY`: arcad deletes the item and the kernel drops the key | ciphertext on the stick. Unreadable unless it has a recovery slot, in which case Curia says so and offers to erase the stick while it's plugged in |
| **a persona** (personas.md §4.1, "removing a persona") | instances stop; the group's domains are unmounted; arcad deletes the persona with every item in it (GK and its sticks' VKs); kernel drops; atrium-volumes removes the domains' inodes (found by `quota_domain`) and retires every pack tagged with the group | nothing readable. Packs are freed without decrypting them, because packs are tagged by group (§5.5) |
| **a person** (account removal) | the same for each of their groups | nothing readable |

**The flash caveat.** Deleting an item in the Arca store frees a record, but an SSD may
keep old copies of the store's blocks, and the item there is wrapped by keys that came
from the person's passcode. Someone who later gets both a raw flash image and the
passcode could unwrap a "destroyed" key. With a TPM, arcad rotates the persona's
per-recipient wrapping key on removal and keeps the TPM-sealed root key out of flash,
which closes this. Without a TPM, erase holds against everyone who lacks the person's
passcode, and Curia says so when the persona is removed.

## 10. Kernel and process boundaries

- **Authenticate, then decode.** On a sealed volume, every block reaches the Rust
  decoders (`tessera-decode`, D3) only after its AEAD and parent tag verify. The decoders
  stay hardened anyway, because the kernel's own bugs can produce bad blocks too. So on
  a trusted stick, the kernel parses only bytes that this machine's key wrote.
- **The clear parts** that are read before authentication are a fixed list: superblock
  magic, the incompat flags, `encryption_flags`, suite, key id, `check`, volume uuid,
  key slots, and the pack prelude (§5.5). The superblock fields are read by the jailed
  FUSE probe, never by the kernel before the key is loaded. Each has a fuzz target with a
  positive control, as Arca's formats do (arca.md §6.3).
- **Refusing sealed volumes without support.** atrium-capsa-fuse's Tessera backend and
  the loader's reader already refuse any incompat bit they don't know (§5); a test keeps
  it so.
- **Crypto code** (revised after E0, §5.1). The kmod's Rust part is compiled file by
  file with only `core` and the KPI crate, so it cannot take RustCrypto crates, and
  writing our own AEAD is not an option. The kernel therefore uses FreeBSD's own crypto:
  bulk AEAD through OCF (`crypto(9)`) on the `ossl(4)` driver, whose OpenSSL assembly
  is what E0 measured, with a per-request key; subkey derivation (HChaCha20 from the
  kernel's libsodium, or XAES's two AES blocks) beside it. The kernel's convenience KPI
  `xchacha20_poly1305_encrypt()` is not used: it runs libsodium's portable ChaCha20 and
  is 3 to 7 times slower (§5.1). Userspace tools (developer `mkfs`/`fsck` with a key
  file, differential tests) use RustCrypto, and the two are tested against each other
  and the published vectors. Argon2id runs only in arcad.
- **Page cache.** Plaintext of open files is in memory while a group is unlocked. When a
  group locks at logout, the kernel flushes and invalidates that group's vnodes and
  pages before it drops GK. Anything still open makes the lock wait, and logout stops the
  person's processes first.

## 11. How this fits with full disk encryption

Tessera encryption covers person data and sticks. The internal Tessera filesystem is
also the root the loader boots from (`atrium-tessera/loader/tessera.c`), and it holds the
kernel, arcad, the OS and app trees, `machine.key` when there is no TPM, and any swap.
None of that can wait for an Arca key, because Arca runs from it. So:

- **Person data is protected either way** (§3): contents and names under keys from each
  person's passcode.
- **The rest of the internal disk is not encrypted.** It is mostly public (the signed
  OS and apps), but an offline attacker can read volume metadata (§3), read
  `machine.key` on a machine without a TPM (and so the machine store: Bluetooth bonds,
  Wi-Fi passphrases), and change the OS itself.

Closing that is a boot question: either GELI under the Tessera root (the loader asks for
a passphrase or a TPM-sealed key), or this spec's sealed format applied to the root, with
the loader given the key by the TPM (FreeBSD's loader has no TPM support today), and
verified boot. That is the "full disk encryption" layer. It would add to these layers
rather than replace them: FDE gives one key for the whole machine at boot, while this
design gives keys per person, persona and stick, crypto-erase, authenticated contents and
rollback checks. *Recommend a short follow-up spec on the system disk once the TPM work
(Arca A4) starts.*

## 12. Not in this spec

- Hiding sizes, times and owners of files in a locked group, and authenticating the
  inode table on the internal disk (a tree of inode records per group, or a sealed root;
  §3, §11).
- Key rotation without a full rewrite (rotation = repack under a new key id).
- Shared sticks: one stick openable by two personas or two people (would add slot types
  per Arca store).
- Sealed send streams to backup targets (§6.3; designed, built with backups).
- Encrypting the boot disk and the machine store on it (§11).
- Opening a sealed stick on a non-Atrium system: no.

## 13. Build order

| step | what | done when |
|---|---|---|
| E0 | Suite benchmark on the kernel's candidate code paths, vectors checked | **done 2026-11-17** (a09cb45, 007eb73): numbers in §5.1 for x86_64 and Apple M4; XAES-256-GCM chosen; in-kernel timing moves to E1 |
| E1 | Sealed volumes (layer A): incompat bit (readers already refuse it, tested 2026-11-17), superblock HMAC and root tags, sealed metadata blocks with parent tags, journal chain, sealed packs and blobs, keyed ids, kernel keyring with a developer key loader, kernel format; `fsck.tessera --key-file` (developer builds) | VM: write, crash, replay and fsck on a sealed volume. Flipping any byte of any block is refused before decoding (fuzz across every block kind). An older copy of any one block is refused. The capsa reader refuses a sealed volume. Throughput vs unsealed recorded |
| E2 | Arca: `VolumeKey`, `/dev/tesserakeys` held by arcad, `LOAD`/`MINT`/`RECORD_GENERATION`/`FORGET` | arca-vm-test: key loads only for the active, unlocked owner, never released by any op; a mount with the wrong key id or a foreign uuid is refused |
| E3 | Trusted sticks: format from Scrinium, probe, kernel mount, Locked state, recovery code, high-water, eject recording | vm-automount-check: a sealed stick opens for its persona only; a stick from another machine is Locked; a recovery code adopts it; an older image is refused and opens read-only after verification; forgetting the key leaves it Locked |
| E4 | Encrypted domains on the internal volume: key-group and domain records with MACs, group-tagged packs, keyed ids, names and CDC per group, the parent-domain rule, lock at logout with page invalidation, atrium-volumes `encrypt = "persona"` with expected groups | VM: two people; root cannot read B's files or names while B is logged out; an offline edit that strips a domain's group or moves a file out of it is refused at login; dedup within a group, none across groups; GC leaves a locked group's packs alone |
| E5 | Personas: persona domains and sticks keyed in the persona's Arca, crypto-erase on persona removal (personas.md P2 and P4) | removing Work leaves none of its packs, items or keys, and its sticks show Locked |
| E6 | Move existing home folders and app data into the person's Personal group (copy into the new encrypted domain, then GC the old) | an upgraded VM keeps every file, now sealed |

## 14. Decisions

Accepted 2026-11-17, all as recommended.

1. **Two layers: sealed volumes for sticks (key per stick) and encrypted domains on the
   internal volume (key per person and persona)**, so persona folders need no partition
   of their own. *Recommend yes.* The alternative is one sealed Tessera filesystem per
   persona, on its own partition or file-backed. That gives authenticated metadata too,
   but space has to be set aside up front and dedup is lost even within a persona.
2. **AEAD on every block, pack and blob of a sealed volume, with parent-held tags, and
   on every blob and group record in a key group; no XTS.** Format.h's
   "AES-XTS" and "convergent" reservations are redefined (nothing on disk uses them).
   *Recommend yes.*
3. **Cipher suite: XAES-256-GCM**, with XChaCha20-Poly1305 as suite 2 for CPUs without
   AES instructions. *Changed by Shankar 2026-11-17 after E0* (the spec first recommended
   XChaCha20-Poly1305; E0 showed it slower than SHA-256 on the M4 and 2 to 5 times
   slower than XAES on the same assembly).
4. **Keys only ever in Arca and the kernel:** new kind `VolumeKey`, loaded by arcad
   through `/dev/tesserakeys`, formatting done by the kernel. *Recommend yes.*
5. **The internal Tessera volume stays unsealed for now**, because the loader boots from
   it and Arca runs from it. Person data is protected by encrypted domains; the rest of
   the disk waits for the system-disk follow-up (decision 12). *Recommend yes.*
6. **Personal is encrypted too:** each person's home folders and app data form one key
   group under their Personal persona, so root cannot read a logged-out person's files.
   This extends personas.md, which keyed only named personas. *Recommend yes.*
7. **Groups are after-first-unlock and lock at logout.** *Recommend yes.*
8. **Keyed content ids per group and domain, keyed CDC per group; convergent encryption
   dropped.** *Recommend yes.*
9. **Recovery codes for sticks: offered at format, default off for named personas and
   on for Personal.** *Recommend yes.*
10. **Rollback: high-water marks in Arca for sticks and key groups, and
    older copies open read-only only after user verification.** *Recommend yes.*
11. **Trusted sticks (sealed, and the key is in the session person's Arca) are mounted
    by the kernel; all others stay on the jailed path, and Locked sticks are never
    decoded.** *Recommend yes.*
12. **System disk encryption (GELI, or a sealed root unlocked by the TPM in the loader)
    is a separate follow-up**, after Arca's TPM work.
    *Recommend yes.*
