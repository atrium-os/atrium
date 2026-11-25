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

The read-only `kern.tessera.sealed_mounts` sysctl gives the host administrator
the count of mounts with an authenticated volume seal. Crypta uses it for
reviewed GlobalProtect HIP posture. The count includes sealed filesystems
only and exports no volume paths, identifiers or keys.
The read-only `kern.tessera.verified_encrypted_domains` sysctl gives the host
administrator a separate count of domain records on live mounts whose MAC
verifies against a loaded, authenticated group key. Locked domains remain
encrypted and are outside this count while the key is unavailable. Neither
query exports domain or group identifiers.

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
- **As built (E2).** The ops are `VK_MINT` 0xB0, `VK_LOAD` 0xB1, `VK_RECORD` 0xB2 and
  `VK_FORGET` 0xB3 (`arca/arcad/src/proto.rs`, `volume_op`); every body starts with
  `str user | str persona`, so the caller names the owner and arcad checks it. The item is
  kind `VolumeKey` (0x103, a key shape, so the service `RELEASE` op refuses it), stored
  for the recipient `tessera-keyring`, a name no service may take. `VK_LOAD` answers
  `DENIED` for an unknown key id or a scope that isn't the item's exact scope, rather
  than `NO_KEY`, so a caller can't probe which ids exist under someone else. `VK_MINT`
  returns only `key_id`: the formatter in the kernel computes `check` when it writes the
  header. `ADOPT_RECOVERY` waits for E5. arcad does not enter Capsicum yet; the
  descriptor is opened before it serves and nothing else in its jail can open the node.
- **Only arcad's jail.** The node is shown by devfs ruleset 32 (`atrium_arca`), which
  jaild pins to the arcad service, and the kernel refuses every jailed caller whose jail
  lacks `allow.tessera_keys`. tessera.ko registers that jail parameter, and jaild gives it
  only to jails in `policy.tessera_keys.allow_jails` (arcad alone), so a ruleset mistake
  in another jail still doesn't reach the keyring.
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

#### E1: in the kernel

Measured 2026-11-17 in the arm64 FreeBSD VM on the Mac (QEMU with HVF, OCF driver
armv8crypto, a 1 GiB file written then read back on an md image, single runs; plain
writes ranged 45 to 62 MiB/s across the day's runs, so read these as indicative):

| 1 GiB, MiB/s | write | read |
|---|---|---|
| plain | 45 | 68–69 |
| sealed | 43–44 | 48–52 |

Sealing costs 2 to 4% on writes and 25 to 30% on reads, where each 4 KiB unit is
opened in its own synchronous OCF request.

**OCF sessions take one request at a time.** armv8crypto and aesni apply a request's
`crp_cipher_key` by rewriting the session's key schedule, unlocked. Every XAES unit has
its own derived key, so requests sharing a session ran under each other's keys: nodes
written with tags no reader could reproduce, which surfaced only under concurrent load.
The kernel backend (kmod/tessera_crypt_ocf.c) holds one set of sessions per CPU (up to
16), each under its own lock for the whole request.

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
- `reserved_e0` becomes the **suite** byte `enc_suite` (1 = XAES-256-GCM, 2 = XChaCha20-Poly1305; §5.1).
- `master_key_id` holds the VK's key id. A new 32-byte field from the trailing slack
  holds `check` (§4.2).
- **`hmac[32]`** (reserved in format.h) holds `HMAC-SHA-256(K_meta, superblock with
  crc32 and hmac zeroed)`. On a sealed volume the HMAC replaces the CRC as the check, and
  the CRC is still written so tools can detect corruption without a key.
- **Root tags.** For every tree root in the superblock (inode, pack registry, free
  extents, quota, blob index, dead extents, snapshots), the slack gains the root block's
  16-byte AEAD tag, indexed by tree kind (`root_tags[7][16]`, 112 bytes), and the
  tag of the last journal record the superblock covers (`journal_anchor_tag`, §5.3).
  With `key_check` and `sealed_stats` that is 288 of the 1692 bytes of slack.
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

In core this is `tessera_volume_open_sealed` (core/src/volume.c). It reads both
superblock copies and uses the newest one whose key check and HMAC pass, so a damaged or
forged copy, even one claiming a newer generation, falls back to the other. When neither
passes it returns `ENOKEY` if the key is not the volume's, `EAUTH` if the key is right
but nothing authenticates, and `EINCOMPAT` for an unsealed volume. `tessera_volume_open`
refuses a sealed volume with `EINCOMPAT`, as the loader and the capsa reader must.
`tessera_volume_format_sealed` seals every tree, starts the journal at a random anchor,
seals the seed pack's front and blobs under keyed ids, and writes the root tags, the
anchor and the HMAC last (tests/test_volume_sealed.c reads a seeded file back through
all of it on both suites).

In the kernel (atrium-tessera/kmod): `tessera_crypt_ocf.c` installs the backend at module
load over OCF with per-request keys and refuses to install if the XAES and XChaCha known
answers fail; `tessera_keyring.c` is `/dev/tesserakeys` (§4.3; developer builds load keys
with `tessera-keyload`). The mount looks the key up by `master_key_id` and the uuid, then
selects the superblock as above; trees open only under the superblock's root tags, and blobs
are read by opening the pack front from its own sectors against the registry's tag.
Sealed mounts are read-write: packs are built sealed (registry values are 80 bytes, the
entry then the front's tag; one contiguous extent, no sealed PEL), every commit copies each
tree's root tag into the superblock and seals a copy of it, the journal's ROOT_UPDATE carries
the root tags, and each commit (and a clean unmount) moves the journal to a fresh anchor
written in the superblock before the checkpoint. A journal that does not open under the
superblock's anchor (a crash between that superblock write and the checkpoint) is
reformatted by a writable mount: the superblock already holds all of it.

Garbage collection runs on sealed volumes. Pass 1 walks manifests through the sealed fetch;
pass 2 learns each pack's keyed ids by opening its front against the registry tag (no blob
is read), and a front that does not open keeps its pack. A concurrent pass takes each frozen
root's tag with the root under the commit gate, and the background pin scan walks those
frozen trees under the same tags. Repacking does not apply: it consolidates multi-extent
packs, and sealed packs are always one extent. Snapshots and the dead-extent log stay off
on sealed volumes for now (snapshot records would need their roots' tags).
scripts/tessera-sealed-vm.sh covers write, remount, garbage collection and crash replay.

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
- **Capacity.** A node has 4056 − 64 (header) = 3992 bytes for entries instead of 4032,
  and an internal entry is 16 bytes longer (sector and tag). An inode leaf holds 26
  records instead of 27 (4 + 144 bytes each) and an inode internal node 142 children
  instead of 336; a pack-registry leaf 49 entries instead of 50 and an internal node 99
  instead of 168. Journal record bodies shrink from 4068 to 4028 bytes per block.
  Built in core/src/btree.c (`tessera_btree_open_sealed`, `tessera_btree_create_sealed`;
  tests/test_btree_sealed.c).
- **Journal chain.** Every journal block's AAD also includes the record's sequence
  number and the previous block's tag, so records cannot be reordered, dropped from the
  middle or spliced in from an older journal. Each checkpoint starts an epoch with a
  fresh random anchor: the superblock written at that commit carries it as
  `journal_anchor_tag`, the journal header's AAD includes it, and the first block after
  the checkpoint chains to it, so a journal or header from an earlier epoch does not
  open. Replay stops at the first block that does not open, exactly as at a torn
  write. Cutting the tail looks like a crash before the last commit, which replay
  already handles. Because the superblock is rewritten at every commit (tessera-fs.md
  §18 step 13), the cut cannot undo anything the superblock already covers. Built in
  core/src/journal.c (`tessera_journal_open_sealed`, `tessera_journal_checkpoint_anchor`;
  tests/test_journal_sealed.c).
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
  matter's tag, and so the registry's value grows to 80 bytes there. On disk: the
  64-byte prelude (`TPACKSL1`, version, front length, key group, zeros), then the
  sealed front (nonce, header, index and bloom, tag), with the whole prelude in its AAD;
  the data area and footer follow as in a plain pack. Opening the front in place gives
  back the plain pack byte for byte, so the pack reader and the kernel's partial reads
  work on it unchanged (core/src/seal.c `tessera_pack_seal_front`,
  `tessera_pack_open_front`; tests/test_seal.c).
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
   mkfs. *As built:* `TKEY_FORMAT` on `/dev/tesserakeys` (`tessera/keyring.h`), with
   the partition as a descriptor the caller opened read-write, the key id and the uuid
   the key was minted for; unjailed callers only (`kmod/tessera_format.c`).
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

### 7.2a As built (E3, 2026-11-17)

- **Format:** media op `FORMAT { part, persona, label }` (root or the session's user,
  `dispensator/media`). dispensatord stops whatever served the stick, picks a uuid, has
  arcad `VK_MINT` the key under that persona, then `TKEY_FORMAT`, mounts it and gives the
  top folder to the person (0700). A blank stick can be formatted; it never has to be
  listed first.
- **Plug-in:** dispensatord reads the two superblock copies' magic, SEALED bit, uuid and
  key id (fixed offsets, `dispensator/src/sealed.rs`) itself rather than through the
  FUSE probe: four fixed fields, and the kernel authenticates everything before it
  mounts. arcad `VK_FIND { user, key_id, uuid }` looks in every persona of the session's
  person and answers the persona. Found: the kernel mounts it `nosuid,noexec` at
  `/media/<part>`, listed with `persona`. Not found, or the kernel refuses it (damaged,
  or older than the key's high-water mark): listed `locked`, not mounted.
- **Leaving:** lock, logout, a user switch, eject and unplug unmount it, record the
  generation (`VK_RECORD`) and drop the key.
- **arcad sees dispensatord by uid**, and dispensatord is root, which arcad serves no
  one as; its arcad calls run in a child as `_dispensator-vk` (65043), named
  `dispensatord` in `ARCA_SERVICES`.
- **Recovery codes (§7.4):** `FORMAT { recovery: true }` (`--format … --recovery`).
  arcad `VK_RECOVERY` 0xB5 makes the code (128 bits and a 22-bit checksum, Crockford
  base 32, `XXXXX-` × 6) and a type-3 slot: Argon2id at arcad's cost (kept in the
  slot) over the code's 16 bytes, then XChaCha20-Poly1305 over the key, bound to the
  uuid and key id. `TKEY_FORMAT_RECOVERY` has the kernel write the slot in
  `key_slots[0]` under the HMAC. The code comes back in the format reply, once; it
  passes through dispensatord, never on a command line. A Locked stick with a slot is
  listed `recovery`; media op `ADOPT { part, persona, keep, code }` (0x0B) has arcad
  `VK_ADOPT` 0xB6 unwrap the key and keep it in the persona (or load it for this
  session only), then the kernel mounts it. The AEAD tag stands in for comparing
  `check`: a wrong code fails the unwrap, and the kernel still checks the key against
  the volume. Still to come: the Arca credential field the code is typed into
  (arca.md §8.3; for now the client sends it; since built), removing a code later (since built, below),
  and Curia's wording.
- **Older copies (§8):** the kernel refuses a copy below the key's high-water mark with
  `ESTALE` (an authentication failure stays `EINTEGRITY`), and dispensatord lists it
  `locked older` with its persona. Media op `OPEN_OLDER` (0x0C) has arcad `VK_OLDER`
  0xB7, behind Sigillum's `VERIFY_USER`, reload the key `TKEY_LOAD_OLDER_RO`: the
  kernel then opens the copy below the mark, read-only only (a writable mount or a
  ro→rw remount with that key is `EROFS`). Opened, it is listed `older` and read-only;
  media op `MAKE_CURRENT` (0x0D, "Use this copy as current") stops its server,
  unmounts it and has arcad `VK_CURRENT` 0xB8, behind a second `VERIFY_USER`,
  lower the item's mark to the generation the kernel saw mounted (the kernel
  notes every mounted generation in the keyring, read back with `TKEY_GEN`;
  never a number from dispensatord) and reload the key as usual; the kernel then
  mounts it writable. Refused or unverified, it goes back read-only.
- **Forgetting a stick's key (§9):** media op `FORGET` (0x0E, `--forget PART`) for an
  open sealed stick of the session's person: dispensatord stops its server, unmounts
  it, records its generation, and has arcad `VK_FORGET` 0xB3, behind Sigillum's
  `VERIFY_USER`, delete the item and drop the key; the stick is then listed `locked`.
  Refused or unverified, it is mounted again as it was. An open stick is listed
  `recovery` when it has a slot, so Scrinium's "Forget this stick's key…" sheet says
  whether its recovery code would still open it (and points to Erase and format).
- **Removing a recovery code (§7.4):** media op `REMOVE_CODE` (0x0F, `--remove-code
  PART`) for an open, writable sealed stick of the person's that has one. dispensatord
  opens the mount's root and issues `TESSERA_IOC_CLEAR_RECOVERY` (`_IO('T', 6)`, unjailed
  root only): the kernel zeroes every type-3 slot in the in-core superblock and marks it
  dirty, and the fsync that follows commits both superblock copies under the HMAC. No
  verification: it only takes a way in away. Scrinium's "Remove recovery code…" says
  that only this machine's key opens it afterwards.
- **Grants of its folders** (built; `tessera-sticks-vm.sh` §5b, 12 checks, all pass in the VM on 2026-11-18 at 908973d): a trusted stick has no
  FUSE server of its own, so dispensatord starts one per mounted stick,
  `atrium-capsa-local`, in a jail of jaild's `[stick_servers]` class: named
  `system-stick-<part>`, running as the stick's person (a person's uid, never a
  system one), with only its binary and libraries mounted and no network. jaild
  opens `/media/<part>` itself, refuses it unless it is the root of a Tessera
  mount, and hands it over as a directory descriptor (fd 3); the server enters
  Capsicum and serves every path relative to it without following links (links
  and device nodes aren't shown). Its own FUSE mount sits at
  `/media/.sticks/<part>` (root's, 0700); grants arrive on its control socket
  like a FAT stick's (device-assignment.md §4.8), so the kernel's permission
  checks as the person still apply beneath every grant. A persona's stick is
  granted only to that persona's instances, as the launch registry binds the
  app's uid (personas.md §5.1); an older copy's grants are read-only. Eject
  ends the grants and the server before the gentle unmount (and brings the
  server back if the stick is busy); lock, unplug and session end do the same
  through `stop_sealed`. jaild hands the directory to the child only after
  `jail_attach`, over a socket at fd 3: the kernel refuses to attach a process
  holding a directory descriptor. Needs `fusefs.ko` on the Atrium kernel
  (kernel-hardening.md keeps it loadable).
- **Screens (host tests, 2026-11-18):** Scrinium lists a sealed stick with its
  state. Another persona's stick shows that persona's name and isn't opened
  ("Open it from a Work window"). This window's own sticks say "Encrypted". A
  Locked stick shows "Encrypted stick from another machine or persona" in place
  of its files, with **Enter recovery code…** when it has a slot, and **Erase and
  format…**. An older copy offers **Open read-only…**; once open, a strip offers
  **Use this copy as current…**. "Make this a Work stick" (or "Encrypt this stick"
  in a Personal window, `SCRINIUM_PERSONA`) is on the stick's actions. Its sheet
  says what will be erased, with a recovery code ticked by default for Personal
  only, and the code it makes is shown once. Every request goes to dispensatord
  off the window's thread, because arcad may wait for verification.
  Forum's stick menu shows whose a stick is, or Locked, and points to Scrinium.
- **The recovery code is typed into Arca's field** (a Limen `credential` slot,
  purpose `RecoveryCode`). arca-ui draws it as typed (the code is long, and the slot is
  Arca's own surface) and holds it with `U_HOLD … HOLD_FOR_RECOVERY`. Scrinium
  passes only the handle in the media op `ADOPT { handle }`. dispensatord sends
  `VK_ADOPT` with an empty code and the handle, and arcad redeems it
  (`take_recovery`): once, for the user who typed it, and only from that kind
  of field. No fill, courier or `TAKE_WIFI` takes it. root's `--adopt` still
  passes a typed code.
- Gate: `scripts/tessera-sticks-vm.sh` (an md disk stands in for the stick): all 43
  checks in the VM on 2026-11-18 (e61b9c0: format, lock, Locked for another person,
  eject, replug, an older copy refused and not opened unverified, a recovery code
  adopting the stick into another person's Arca), with `arca-vm-test.sh` §9 66/66 and
  `tessera-sealed-vm.sh` §10 (an older copy opens read-only only with a key loaded
  `OLDER_RO`). A forgotten key leaving its stick Locked is an arcad host test.

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
  unwraps the VK and checks `check`. The slot also carries the stick's name (its
  `label`, at most 63 bytes), sealed under the same key in the slot's reserved bytes and
  padded so its length doesn't show, so the stick keeps its name wherever the code opens
  it; a slot without one opens all the same. Then it either keeps the VK in the persona the
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
| E1 | Sealed volumes (layer A): incompat bit (readers already refuse it, tested 2026-11-17), superblock HMAC and root tags, sealed metadata blocks with parent tags, journal chain, sealed packs and blobs, keyed ids, kernel keyring with a developer key loader, kernel format; `fsck.tessera --key-file` (developer builds) | VM: write, crash, replay and fsck on a sealed volume. Flipping any byte of any block is refused before decoding (fuzz across every block kind). An older copy of any one block is refused. The capsa reader refuses a sealed volume. Throughput vs unsealed recorded **done 2026-11-17**: scripts/tessera-sealed-vm.sh run/fsck/bench pass on both modules (2266fbd); byte flips over every block of a formatted volume in test_volume_sealed, per tree node and journal block in test_btree_sealed and test_journal_sealed, older copies in the latter two; capsa refuses via tessera_volume_open; throughput in §5.1. Garbage collection on sealed volumes added after (§5.2); snapshots still off |
| E2 | Arca: `VolumeKey`, `/dev/tesserakeys` held by arcad, `LOAD`/`MINT`/`RECORD_GENERATION`/`FORGET` | arca-vm-test: key loads only for the active, unlocked owner, never released by any op; a mount with the wrong key id or a foreign uuid is refused. **Done** 2026-11-17 (93f2b45..9e842d7; scripts/arca-vm-test.sh §9, 24/25 in the VM, the one miss a test expectation fixed after) |
| E3 | Trusted sticks: format from Scrinium, probe, kernel mount, Locked state, recovery code, high-water, eject recording | vm-automount-check: a sealed stick opens for its persona only; a stick from another machine is Locked; a recovery code adopts it; an older image is refused and opens read-only after verification; forgetting the key leaves it Locked |
| E4 | Encrypted domains on the internal volume: key-group and domain records with MACs, group-tagged packs, keyed ids, names and CDC per group, the parent-domain rule, lock at logout with page invalidation, atrium-volumes `encrypt = "persona"` with expected groups | VM: two people; root cannot read B's files or names while B is logged out; an offline edit that strips a domain's group or moves a file out of it is refused at login; dedup within a group, none across groups; GC leaves a locked group's packs alone |
| E5 | Personas: persona domains and sticks keyed in the persona's Arca, crypto-erase on persona removal (personas.md P2 and P4) | removing Work leaves none of its packs, items or keys, and its sticks show Locked. **Done** 2026-11-19 (993cd71d; `tessera-personas-vm.sh` 57/0: the Work stick key leaves the kernel and isn't found, the removal's scrub zeroes the deleted app data; arca 207/0 full and 124/0 VK only, sticks 74/0, sticks-ui 15/0) |
| E6 | Move existing home folders and app data into the person's Personal group (copy into the new encrypted domain, then GC the old); in steps in §13.2 | an upgraded VM keeps every file, now sealed. **Done** 2026-11-19 (e6e13712, main's jailed stack deployed: `tessera-personas-vm.sh` 78/2, the two section 6 misses test bugs fixed after (1891f944: a group read by position, and du can't show holes on Tessera); new `vm-logout-check.sh` 0 failed through the real ostiarius and portcullisd; vm-personas-check 0 failed, vm-apps-access 0 failed, arca 124/0, sticks 74/0, sticks-ui 15/0) |

### 13.1 E4 in steps

E4 is mostly kernel work, so it is built in steps that each pass on their own:

| step | what | done when |
|---|---|---|
| E4.1 | Core: group keys from GK (`tessera_group_keys_init`), key-group records in the quota tree (prefix 0xFF, `tessera_key_group_t`, MAC under `K_gmeta`), a domain's `key_group` and `group_mac`, `K_gid[d]`, keyed directory-name hashes, sealed journal names. The kernel's domain loader skips group records | host tests (`test_keygroup`, plain and ASan) **done 2026-11-18** |
| E4.2 | Keyring and mount: `TKEY_LOAD` with a group scope makes the group's keys; a mount reads its group records and verifies each against a loaded key; `TESSERA_IOC_GROUP_CREATE` (key id, group) writes a record and sets `INCOMPAT_KEY_GROUPS`; `TESSERA_IOC_QUOTA_SET` takes a group and seals the domain into it | VM: a group made, remounted, verified; a domain record edited offline is refused. **Done** 2026-11-18 (3617ba7, 41f7a02, 0c625c3; tessera-sealed-vm.sh check 11) |
| E4.3 | Writes and reads in a group: one open pack per group, blobs sealed under `K_gblob` with ids under `K_gid[d]`, the front under `K_gmeta`, directory names hashed under `K_name`, journal names sealed. On an unsealed volume the registry keeps its 40-byte entries: a group pack's front is authenticated by its own tag, and packs never change, so there is no older copy to swap in | VM: files and names written in a group are not in the clear on the device; read back after remount; dedup inside the group, none across. **Done** 2026-11-18 (e21928e, 378a656; check 12) |
| E4.4 | Locked groups and the rules: without the key, contents, names and xattrs answer `EACCES` (root can still `stat`); the parent-domain rule; rename into, out of or between groups is `EXDEV`; dropping a key flushes and invalidates the group's vnodes and pages | VM: root can't read B's files while B's key is dropped; an offline edit that moves a file out of the group is refused **Done** 2026-11-18 (34936eb, 6a01870c, ec474cee; check 13). |
| E4.5 | GC: group packs are kept while the group is locked and collected by group when it's loaded; crypto-erase retires a group's packs without its key | VM: GC on a volume with a locked group frees nothing of it **Done** 2026-11-19 (89d2f896, 33254b47, 83a70340; check 14). |
| E4.6 | atrium-volumes `encrypt = "persona"` with expected groups, arcad group keys (`VK_MINT`/`VK_LOAD` with a group scope) at login and dropped at logout | VM: two people, the §13 E4 gate **Done** 2026-11-19 (87d5f8ec, 74d59854, 486f4f03; `tessera-personas-vm.sh` 38/0, sealed 14/14, arca 124/0, sticks 74/0, sticks-ui 15/0). |
| E4.7 | Scrub after a move-in (decision of 2026-11-19, "scrub them, as a next step"): `TESSERA_IOC_SCRUB` retires retained snapshots, rebuilds partly-dead plain packs without their dead blobs, zeroes the data zone's free space and the journal's old records; atrium-volumes queues one before it deletes a moved-in folder's plain copy and runs it in the background | VM: after a move-in, none of the old contents or names is anywhere on the device, and a plain file that shared a pack with them still reads (`tessera-personas-vm.sh` 1b). **Done** 2026-11-19 (48e6480d; personas 47/0: 3 snapshots retired, 5 packs freed, 3 rebuilt, 63 journal blocks zeroed; sealed 14/14, arca 124/0, sticks 74/0, sticks-ui 15/0) |

**E4.3 as built (2026-11-18, kernel).** Every write carries a write context
(`struct tessera_fs_wctx`): the keys of the domain of the inode it is for, taken
once at the write's entry point (file content, directory updates, create, mkdir,
symlink, xattrs). In a group that is the group's `K_gmeta`/`K_gblob` and
`K_gid[d]` as the id key; outside, the volume's own (sealed) or none (plain). A
write whose group is locked or doesn't verify is refused, never made under the
volume's keys. Group packs are single-extent, flagged `KEY_GROUP` in the
registry (bit 3), kept out of the location and byte caches, and never batched
with other manifests: the pending-manifest drain publishes a group's manifests
one per pack under that group's keys. Reads open a group's pack under the group
its prelude names (EACCES without the key); on an unsealed volume the blob index
is tried first, then the plain path, then a scan of the groups' packs. GC keeps
every group pack until E4.5, and a grouped domain can't be detached or change
its dedup policy. With groups on a mount the dirty-content drain is serial
(the parallel prepare uses the volume's keys). Names in journal dirent records
of a group's directories are sealed under `K_gmeta`, bound to parent and inode
(`reserved[0] = 1`); replay keeps them sealed, and the journal drain opens them
once the key is loaded (journaling them again until then). Directory manifests
are sealed blobs, so their plain name hashes are never on disk in the clear;
keyed name hashes (`K_name`) and the keyed gear table (`K_cdc`; the kernel
chunks at fixed sizes) are left to userspace writers. Host tools (fsck, the
reader) don't open group packs yet. VM gate: `tessera-sealed-vm.sh` check 12.

**E4.4 as built (2026-11-18, kernel).** Every vnode op that uses an
inode's contents, names or xattrs (access for read, write or search, so
lookups served from the name cache too; open, read, write, readdir, lookup,
readlink, getpages and the four xattr ops) asks whether the inode's group
is open; a locked one answers `EACCES` for root too and is logged "locked"
(rate-limited). `getattr` doesn't ask, so root can stat a group's root from
its plain parent; below it, names need the key. A vnode caches its group,
looked up again when a quota ioctl changes domains. The parent-domain rule
is enforced at lookup when either domain is in a group: a name whose inode
is in another domain than its directory is refused (`EINTEGRITY`, logged)
unless the inode is its domain's root. Rename and link compare the moving
inode's domain with the target directory's: into, out of or between groups
is `EXDEV` (`mv` then copies and deletes); a group's root may move between
directories of the same group or of none. A directory of a grouped default
domain can't start a new domain. Dropping a group's key (`TKEY_DROP`)
first calls into every mount of that volume: the group is marked closing
(its vnode ops answer `EACCES`, while the flush still writes with its
keys), dirty mapped pages are written back, the mount is flushed, and the
group's VM pages, cached manifest and chunk and name-cache entries are
thrown away; after the drop the derived keys are wiped at once. An open
file stays open and its reads answer `EACCES`. Because the flush commits
before the key goes, the E4.3 limit on journaled names only bites when a
flush fails. Test hook: `kern.tessera.test_cross_domain_link` lets
`link(2)` skip the domain check, standing in for an offline edit. VM gate:
`tessera-sealed-vm.sh` check 13.

**E4.5 as built (2026-11-19, kernel).** GC pass 1 walks an inode of a key
group only while the group is open; a group it can't walk (locked, bad, or
a read its key no longer opens, say a key dropped mid-pass) goes into a
skip set, and pass 2 keeps every pack whose prelude names a skipped group.
Other groups' packs are judged like a sealed volume's: their ids come from
the front opened under the group's keys, and a pack that doesn't open is
kept. So a locked group loses nothing, and GC still frees everything else.
A group with no inodes left is not in the skip set, but its packs only go
once its key is loaded. Crypto-erase is `TESSERA_IOC_GROUP_ERASE` (group id;
`tessera-keyload group-erase DIR GROUP`), for atrium-volumes after arcad
deleted the key: the group must be locked (EBUSY otherwise) and none of its
vnodes in use. It deletes
every inode of the group's domains, turns each domain's root into an empty
directory in no domain (the caller removes it by name), retires the domain
records and the group record, then runs a GC pass, which frees every pack
whose prelude names a group with no record without opening it (no grace
period). Retained snapshots (one per commit, `snapshot_retention`) still
name the erased inodes; pass 1 skips an inode whose domain is gone when its
manifest doesn't open, rather than abort. Deleted data in a group, as
anywhere, is only collectable once the snapshots that hold it age out. A
group id should not be reused before that pass has run: a later
group with the same id would keep the old packs. VM gate:
`tessera-sealed-vm.sh` check 14.

**E4.6 as built (2026-11-19).** Each persona of a person (Personal is
`personal`) has one key group; each of its five folders is a quota domain
in it. portcullisd asks atrium-volumes (`PersonaFolders`) before it makes or
hands out a persona's folders (`home::ensure_person`, `ensure_persona`, so
at the first launch after login). atrium-volumes asks the kernel what each
folder is (`TESSERA_IOC_GROUP_INFO _IOR('T',10)`: volume uuid, domain, root
or not, group, the group's key id, and open/locked/bad, where open means the
domain record carries the group's MAC; `tessera-keyload group-info DIR`).
The first time, it picks a random group id (never reused, even if its own
file is lost), has arcad mint the key (`VK_MINT` with that group) and makes
the group (`GROUP_CREATE`); a missing folder is made empty, made a domain
and put in the group (`QUOTA_GROUP`). A plain folder from before is moved
in (decision of 2026-11-19): renamed to `.<name>.unencrypted`, made again in
the group, its tree copied in (contents, mode, owner, times, the download
origin attribute, links as links) and the old copy deleted; an interrupted
move finishes at the next ask. Expected groups are kept in
`/var/db/atrium/volumes.groups.toml` (person, persona, volume, group, key
id, the folders in it). At the next login a locked group's key is loaded
with `VK_LOAD`; a folder that is expected and is plain, in another group,
not the domain's root, names another key, or doesn't verify is refused
(`FoldersRefused`), and portcullisd doesn't hand it out. A locked Arca is an
error too. Without Tessera or Arca keys (arcad unreachable, no keyring, or
atrium-volumes not in `ARCA_SERVICES`) folders that were never encrypted
stay plain. arcad's calls run in a child as `_volumes-vk` (65053), named
`atrium-volumes` in `ARCA_SERVICES`. Logout: ostiarius stops the person's
apps, then locks Arca, which drops their group keys (the kernel flushes as
each goes). portcullisd unmounts the person's overlays before that
(SessionEnded); one that something outside their uids still holds after
five seconds is unmounted with `umount -f` while the key is still loaded,
so its flush lands, and its md is detached (`-o force`); one still attached
after that is logged and named in SessionEnded's answer, and the key goes
anyway. So a process of another uid that has a person's overlay open (a
root shell, or a system service such as an indexer or a backup) gets EBADF
at their logout instead of holding it up: logout ending on time with the
key gone outranks that process's open files (`scripts/vm-logout-check.sh` 7,
20 rounds with a writer; 0 failed on 2026-11-25 at b21ceebb5 on an arm64 Tessera
guest with persona key groups: every round forced, no md left, the image fsck-clean
and the writer's lines read back, no panic). Removing a persona: `EraseFolders`; arcad's `VK_RETIRE` (0xB9,
atrium-volumes only, key-group keys only, no second verification since
portcullisd had Sigillum verify the removal) deletes the item and drops
the key, then `GROUP_ERASE`; if a folder is still in use the erase waits
for the person's next ask. Limit: a mount holds at most 32 groups
(`TESSERA_FS_MAX_GROUPS`), so 32 person-personas per volume for now. A
locked group's folder can't be opened (E4.4), so `GROUP_INFO` is asked of
the folder's parent with the folder's inode number (`inode`, from stat,
which root may do on a locked inode); the ioctl is `_IOWR('T',10)`. Files moved in from a plain
folder were on disk in the clear before; E4.7 scrubs what they leave behind.
VM gate: `scripts/tessera-personas-vm.sh`.

**E4.7 as built (2026-11-19).** A deleted file's bytes stay on the device
until something overwrites them, so a moved-in folder's plain copy outlives
its delete in four places: free space; plain packs it shared with files that
still live (GC frees a pack only when every blob in it is dead, and small
files are batched into shared packs); trees pinned by retained snapshots;
and the journal, whose plain dirent records name files and which every
commit checkpoints back to its first block, leaving later blocks as they
were. `TESSERA_IOC_SCRUB` (`_IOWR('T',11)`, unjailed root, any directory of
the volume; `tessera-keyload scrub DIR`) clears all four:

1. Flush, retire every retained snapshot, commit.
2. A GC pass that also notes each plain pack (no key group, unsealed
   volume) that is partly dead, with its dead blobs. Its apply phase,
   still gated, rebuilds each one without them (`tessera_pack_compact` in
   core): reads it, writes the rebuild elsewhere, swaps the registry entry,
   frees the old extents. A blob a writer touched during the scan is kept,
   and a pack inside the GC grace period is left for the next scrub. The
   rebuild gets a new random pack id and the kept blobs are re-indexed
   under it: pack ids are derived from what a pack first held, and a
   publish that finds its id registered writes nothing, so keeping the id
   would let a later write of the dropped content dedup onto a pack that no
   longer holds it.
3. Zero the data zone's free space in disk order: take a run of up to
   1 MiB out of the allocator (`tessera_extent_take_next`), write zeros,
   give it back, 16 runs per hold of the gate and with nothing uncommitted
   underneath (a sector freed since the last commit may still be named by
   the durable superblock).
4. Zero the journal's record blocks right after a commit
   (`tessera_journal_scrub`; a sealed journal is left alone).

It costs a write of the whole free space, so atrium-volumes runs it on a
thread of its own and login doesn't wait. The ask is queued in
`/var/db/atrium/volumes.scrub` (one directory per line) before the plain
copy is deleted, so a crash in between still gets its scrub; the queue is
read at start and after each move-in, a failed scrub stays queued, and one
scrub serves every entry on its volume. Not covered: the metadata zone's
free sectors (btree nodes: inode records, the registry and indexes, no
contents or names), and the flash cells under the logical sectors (an SSD
may keep the old data until it erases the block; disk encryption is the
answer there, §11). Plain packs elsewhere on the volume are rebuilt too,
which is the point: everything deleted becomes unreadable, not only the
moved-in folder.

**E5 as built (2026-11-19).** Persona domains already come from E4.6 (one
key group per persona, its key a `VolumeKey` in that persona's Arca), and a
trusted stick formatted for Work keeps its key in Work's Arca (E3). Removing
a persona (Curia, with Sigillum verification) now destroys all of it:

- portcullisd refuses the removal while a sealed stick of that persona is
  mounted ("eject it first"), checked before and after verification, so no
  mount keeps a copy of a key that is about to go;
- it deletes the persona's app places (overlays, workspaces), then asks
  atrium-volumes `erase_folders` with the person's home;
- atrium-volumes queues a scrub of that volume first (durable, as in E4.7),
  retires the folders' key and crypto-erases the group (E4.6), then asks
  arcad `PERSONA_DESTROY` (0xBA, atrium-volumes only, never Personal or the
  machine store): every `VolumeKey` of the persona leaves the kernel keyring,
  and every item group of the persona (stick keys, passwords, passkeys) is
  deleted from the person's store. Then it starts the scrub;
- with nothing of the persona left in Arca, its sticks show Locked
  (`VK_FIND` finds no key), and the recovery code is the only way back in.

App data (overlays, caches) is not encrypted yet; it is deleted in the
clear and the scrub makes it unreadable on the device (decision of
2026-11-19, "delete, then scrub (now)"). Encrypting it live, in the
persona's group, comes with E6.

### 13.2 E6 in steps

The home folders already move in at login (E4.6, scrubbed after by E4.7).
What is left is the app data under `~/.atrium`, and a named persona's app
overlays, which sat in among Personal's (`apps/<id>@<persona>`):

| step | what | done when |
|---|---|---|
| E6.1 | atrium-volumes: a move-in copies a file's runs of zeros as holes (an overlay image is gigabytes, nearly all holes), refuses to cross a mount point, and leaves a plain folder that something is mounted in or from (an overlay, a jail's view) as it is, answering it in `waiting`, until a later ask finds it free | host tests |
| E6.2 | Overlays: a persona's instances at `~/.atrium/persona-apps/<persona>/<id>`, one folder per persona so it can be a domain of that persona's group; old ones moved there (a rename, before anything is encrypted) | host check |
| E6.3 | portcullisd: before a launch, a session component's state, a workspace or a dev core sweep, Personal's `apps` and `state` (root's, 0755) and `workspaces` and `cache` (root's, files group, 0750) are asked for in the Personal group, and a persona's `persona-apps/<persona>` (root's, 0700) in its own; ostiarius tells portcullisd at logout, before Arca locks, and portcullisd ends the person's app processes and workspaces and unmounts their overlays | VM: `tessera-personas-vm.sh` 6 |
| E6.4 | VM gate: a person's state, workspace and overlay image from before E6 move in, the image stays sparse and mounts from the group, nothing is left in the clear, and none of it reads while they are out; the personas and apps gates pass with the new overlay places | `tessera-personas-vm.sh` 6, `vm-personas-check.sh` |

**E6 as built (2026-11-19).** An app's overlay is its own Tessera volume in
an image file (portcullis.md §4.1), so encrypting app data means the image
file is in the person's group: the overlay's blocks are sealed on the
internal volume, and the overlay itself stays a plain volume inside. The
image mounts only while the group is open, so the overlays now come down at
logout: ostiarius sends `SessionEnded` after tearing the session down and
before locking Arca; portcullisd sends TERM, then KILL, to every app uid
of the person (all personas), stops their workspaces and unmounts each
overlay. Something left in use is flushed by the kernel as the key goes
(E4.4) and its reads fail; it is logged. This is decision 7 (groups lock
at logout) reaching apps: nothing of a person runs while they are logged
out, workspaces included. Fast user switching keeps both sessions, and
their keys, as before.

A part of `~/.atrium` is asked for each time something is about to use
it, which also loads the key after a login. A part with an overlay still
mounted (a system upgraded without a reboot) waits, plain, and moves in at
the first ask after it is unmounted, at the latest after the next logout.
Every other outcome refuses the launch (mail.md §6.4a A1, accepted
2026-11-25), and the visible folders (Documents, Downloads, …) at login
follow the same rule: no atrium-volumes, an error or unexpected answer,
or `encrypted: false`. Sealed needs both answers: atrium-volumes'
`FoldersReady { encrypted: true }` (it alone knows each folder's expected
group, so a folder moved offline into another open group is
`FoldersRefused`), and the kernel's (`TESSERA_IOC_GROUP_INFO`, opened
`O_DIRECTORY | O_NOFOLLOW`): every folder in a key group that is open and
not bad. Plain is allowed in two cases only:
- `plain_app_data = true` in `/etc/atrium/volumes.policy.toml`, on a
  machine whose volume has no key groups. The kernel says so: GROUP_INFO
  sets `TESSERA_GROUP_INFO_VOLUME_GROUPS` once any group was made on the
  volume, and a folder in a group counts too. Any error but "not Tessera"
  (ENOTTY, EOPNOTSUPP) or a folder not there yet counts as having groups.
  Otherwise the opt-out is ignored and logged.
- Personal's one login while atrium-volumes reports folders `waiting`.
  `/var/db/atrium/plain-app-data/<person>` records it at first use, with
  this boot's `kern.boottime`; it holds for that boot (portcullisd may
  restart), and is spent at logout or by any later boot, so a power-off
  that never sent SessionEnded spends it too. An unreadable record is
  spent. Left open: a logout portcullisd never hears, followed by another
  login in the same boot, gets it again.
A named persona never runs plain under the migration allowance.
portcullisd tells the app which it was (`ATRIUM_APP_DATA`, portcullis.md
"App data fails closed"). The folder is checked by descriptor and
mounted by path; the app data folders are root-owned all the way down
and the visible ones the person's, so no app can swap one between the
two.

A removed persona's `persona-apps/<persona>` goes with its
group (E5).

Not covered: the machine-wide overlays of apps launched with no person
(`/var/lib/atrium/overlays`) are not personal data and stay plain.

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
