# TPM, measured boot and secure boot

**Status: proposed 2026-11-18.** Nothing here is built. Shankar asked: "what is the plan
for a TPM? do we have access to TPMs on various hardware and secure boot?", then "ok" to
writing it up as a spec.
**Owner:** the Arca thread (phase A4) for the TPM client and sealing; the boot thread for
the loader, measured boot and secure boot.
**Related:** [arca.md](arca.md) §6.1, §6.4, §8.4, §8.5, §11 (A4);
[tessera-encryption.md](tessera-encryption.md) §2, §8, §9, §11;
[vestibulum.md](vestibulum.md) §6.1 (recovery key); [sigillum.md](sigillum.md) §12 (FIDO2
`hmac-secret`); [lock-screen.md](lock-screen.md); [opifex.md](opifex.md) §5 (key roles),
decision 12 (base updates); [fast-boot.md](fast-boot.md); [r9700-bare-metal.md](r9700-bare-metal.md);
[kernel-hardening.md](kernel-hardening.md); [device-assignment.md](device-assignment.md).

## 1. Short answer

- The specs already lean on a TPM "where there is one": Arca seals the user root key and
  the machine store key to it, keeps device-bound keys (DBSC, optionally passkeys) in it,
  uses its dictionary-attack lockout against passcode guessing, and uses an NV counter to
  catch a rolled-back store. Tessera's rollback marks and crypto-erase inherit that.
  Without a TPM everything still works, from the passcode through Argon2id alone, and
  setup says the machine is weaker.
- None of it is built. This spec fills in how: one small Rust TPM 2.0 client inside arcad,
  talking to FreeBSD's `tpm(4)`.
- Secure boot and measured boot are in no spec yet. This one adds them in steps: the
  loader measures what it loads, the machine key is sealed to a **signed** boot policy so
  OS updates don't lock anyone out, then our own secure boot keys and a loader that checks
  the kernel. A Microsoft-signed shim is left for later.
- **Hardware:** almost every x86 PC from the Windows 11 era has a TPM 2.0. Most arm64
  boards don't. Apple Silicon has no TPM, and another OS can't use its Secure Enclave. VMs
  get a software TPM (swtpm). So there are three tiers (§3), and Atrium supports all three.

## 2. What exists today

### 2.1 In Atrium

| piece | state |
|---|---|
| Arca user root key | `arca-store/src/root.rs`: Argon2id from the passcode (256 MiB, 3 passes), computed by ostiarius at unlock; the comment names A4 for TPM sealing |
| Arca machine store key | 32 random bytes in `/var/db/arca/machine.key`; arcad warns at every start (arca.md §6.4) |
| FIDO2 factor | `hmac-secret` from a security key can be mixed into the root key (arca.md §6.1, sigillum.md §12) |
| Recovery | Vestibulum's recovery key wraps a backup of the master key (vestibulum.md §6.1); Tessera stick recovery codes (arcad `VK_RECOVERY`) |
| Kernel | amd64 `ATRIUM` includes GENERIC, which has `device tpm` and `RANDOM_ENABLE_TPM`. arm64 builds `tpm.ko` (TIS and CRB over ACPI, TIS over SPI) but doesn't load it |
| Loader | the fork (based 2026-11-10) has no TPM measurement. `LOADER_VERIEXEC` and `LOADER_EFI_SECUREBOOT` exist but are off |
| Boot on the R9700 box | Atrium's unsigned `loader.efi` beside Ubuntu's shim, started with `efibootmgr --bootnext`, so secure boot is off on that box. Whether it has a TPM is unchecked |
| Dev VM firmware | `~/atrium-vm/code.fd` (QEMU's ArmVirtQemu build) has TCG2 measurement code but no image verification, so measured boot yes, secure boot no. `run-vm.sh` attaches no TPM |

### 2.2 In FreeBSD

- **`tpm(4)`** drives TPM 2.0 over CRB (firmware TPMs) and TIS (discrete chips), found by
  ACPI, and TIS over SPI on FDT boards. It gives one raw command device, `/dev/tpm0`, one
  opener at a time (`EBUSY` otherwise), with no resource manager in the kernel. It also
  feeds the RNG.
- **Measured boot:** the firmware measures `loader.efi` into PCR 4 and the secure boot
  state into PCR 7. FreeBSD main added loader measurement in 2026
  (`stand/efi/libefi/tpm.c`: loader configuration into PCR 8, loaded files into PCR 9,
  secrets and entropy left out). Our fork predates it.
- **Secure boot:** `loader.efi` isn't signed by Microsoft and FreeBSD has no shim.
  `uefisign(8)` (base, BSD) signs EFI images with our own key. `libsecureboot` and
  `mac_veriexec` let the loader and kernel check signed manifests (BearSSL), off by default.
- **Userland TPM stacks** (tpm2-tss, tpm2-tools, BSD-licensed) are in ports. We don't need
  them (§4.1).

## 3. Hardware tiers

| tier | where | what Arca gets |
|---|---|---|
| **T: TPM + Atrium secure boot** | x86 PCs with our keys enrolled (§6.3); Ampere-class arm64 servers | everything below, and the machine key opens only under our signed boot chain |
| **M: TPM, measured only** | x86 PCs with secure boot off or Microsoft-only; the dev VMs with swtpm | sealed user and machine keys, lockout, NV counter, device-bound keys; the machine key opens only after a measured Atrium boot |
| **N: no TPM** | most arm64 boards (Pi, Rockchip), Apple Silicon bare metal, VMs without swtpm, a TPM that is off or known-bad (§4.6) | today's behaviour: Argon2id alone, `machine.key` on disk, FIDO2 recommended; setup and Curia say so |

Availability, in more detail:

- **x86 PCs.** Windows 11 requires TPM 2.0, so nearly every PC since about 2018 has one:
  AMD fTPM (in the PSP), Intel PTT (in the CSME), or a discrete chip on SPI or LPC. It is
  sometimes off by default on DIY boards (a firmware toggle). Secure boot is usually on,
  with Microsoft's keys. Firmware TPMs use CRB and discrete ones TIS; FreeBSD drives both.
- **arm64.** Raspberry Pi and most SBCs have no TPM; SPI add-on modules (Infineon SLB9670
  and similar) work with FreeBSD's SPI driver on FDT. Arm servers (Ampere Altra) have TPM
  headers and full UEFI secure boot. Snapdragon laptops have a firmware TPM but FreeBSD
  doesn't run on them.
- **Apple Silicon.** No TPM and no UEFI. The Secure Enclave serves only macOS; no
  third-party OS uses it. Bare-metal Atrium on a Mac is tier N. On the Mac host,
  `vestibulum-macos` stays out of scope (arca.md §6.1).
- **VMs.** QEMU's `tpm-tis-device` (arm64 `virt`) or `tpm-crb` (x86) with swtpm
  (BSD-licensed; Homebrew and apt have it) gives a full TPM 2.0. Its state is a file on the
  host, so it is a development stand-in, not protection. The Mac VM's firmware already
  measures; secure boot in a VM needs an edk2 build with `SECURE_BOOT_ENABLE`.

## 4. The TPM in Arca (phase A4)

### 4.1 One owner, one small client

- **arcad is the only process that opens `/dev/tpm0`.** Every TPM use is Arca's (sealing,
  device-bound keys, the counter), the device takes one opener anyway, and the rest of the
  system asks arcad. dispensatord grants the node to arcad's jail with a `driver` rule, as
  for other devices; the devfs ruleset pins it to arcad (like ruleset 32 for
  `/dev/tesserakeys`).
- **`atrium-tpm`, a Rust crate written for this,** not tpm2-tss. It needs about 20
  commands: GetCapability, GetRandom, CreatePrimary, Create, Load, Unseal, Sign,
  StartAuthSession, PolicyPCR, PolicyAuthValue, PolicyAuthorize, PolicyGetDigest,
  LoadExternal, VerifySignature, NV_DefineSpace, NV_Increment, NV_Read, PCR_Read,
  FlushContext, ContextSave/Load. It is built on the RustCrypto crates Arca already uses
  (p256, sha2, hmac, aes), with a fuzz target on the response parser. That keeps arcad's
  "minimal dependency set" promise.
- **ECC P-256 only,** no RSA keys (also avoids the Infineon ROCA class of bug).
- **No ownership taken.** Atrium never changes the owner, endorsement or lockout
  authorizations, and never clears the TPM. Windows (dual boot) and firmware keep working.
  Keys live under a primary key re-created each boot from a fixed template, so nothing is
  stored in the TPM's persistent handles. The only NV space is the counter (§4.4).
- **Bus sniffing.** A discrete TPM's SPI or LPC bus can be read with a probe. Every unseal
  and every authValue goes through a salted session with parameter encryption, salted to
  the primary key's public part recorded at enrolment. A different key at the next boot
  (another chip or an interposer) is refused and shown, not re-enrolled silently.

### 4.2 The user root key

Today: `root = Argon2id(passcode)`. With a TPM:

```
a     = Argon2id(passcode, salt)                  (unchanged cost, computed by ostiarius)
auth  = HKDF(a, "atrium tpm auth")                (the sealed object's authValue)
s     = TPM Unseal(user object, authValue = auth) (32 random bytes made at enrolment)
root  = HKDF(a || s [|| fido2 hmac-secret], "atrium user root")
```

- **A stolen disk alone** gives nothing to guess against: `s` is only in the TPM.
- **A stolen machine** lets someone guess through the TPM, and the TPM's lockout limits
  that (Windows sets a threshold of about 32 failures, after which tries come back
  slowly; the exact values are read by the probe, §4.6). Atrium doesn't change those
  settings (§4.1).
- **Argon2id keeps its full cost.** Firmware TPMs have been extracted before (AMD fTPM by
  voltage glitching, 2023), and then the passcode is the remaining barrier.
- The user object's policy is the authValue only, not PCRs. A firmware or OS update must
  never make a person's own passcode stop working. Boot state protects the machine key
  (§4.3); a person's key is protected by the passcode plus the TPM.
- **The ostiarius–arcad split stays.** ostiarius computes `a` as now and hands it to arcad
  on the existing channel; arcad does the unseal and the mix. The TPM is never in the login
  jail.
- **Losing the TPM** (cleared, fTPM reset by a firmware update or CPU change, board
  replaced) makes `s` unrecoverable. The way back is Vestibulum's recovery key
  (vestibulum.md §6.1), which wraps the master key independently of the root. Setup
  already makes one; this spec makes it required on tier T and M machines.
- Passcode change: unseal with the old auth, re-create the object with the new one.

### 4.3 The machine store key

- Sealed with a policy of **PolicyAuthorize** by an Atrium **boot policy key**, whose
  signature over a PCR policy (§5.2) authorizes it. Each OS release ships a signed policy
  for its expected PCR values. Updates then need no reseal step, and a machine booted into
  another OS or an altered loader can't open it.
- First boot on a TPM machine moves `machine.key` into a sealed object and overwrites the
  file. A TPM that can't open the key fails closed, as now (arca.md §6.4): the machine
  store stays locked, and Curia says why (Bluetooth bonds and Wi-Fi passwords must be
  entered again, or a person with the recovery key re-seals it).
- Tier N keeps `machine.key`.

### 4.4 Rollback counter

- One NV counter index (owner hierarchy, empty owner auth, as Windows leaves it), defined
  at enrolment. The user and machine stores record its value in their header. A store
  whose header is older than the counter is refused as a rollback (arca.md §6.1).
- Wear: arcad increments it only at events that matter (logout, lock, a generation change
  in a Tessera key group, a forgotten recipient), not on every write. This also gives
  Tessera's high-water marks their anchor (tessera-encryption.md §8).

### 4.5 Device-bound keys

- DBSC keys, and passkeys a person marks device-bound, are P-256 keys created in the TPM
  under the primary key. Arca stores only the wrapped blob; `Sign` happens in the TPM
  (tens of ms on firmware TPMs, fine for DBSC refresh and WebAuthn).
- Crypto-erase (tessera-encryption.md §9): arcad rotates a persona's wrapping secret by
  re-creating a sealed object, so a deleted persona's keys can't be unwrapped later even
  with a disk image and the passcode.

### 4.6 Probing and known-bad TPMs

- `atrium-tpm probe` (and Curia's Devices page) reports: present or not, interface (CRB,
  TIS, SPI), manufacturer and firmware version, PCR banks, secure boot state (from EFI
  variables), whether the loader measured, and the resulting tier.
- A short list of firmware versions with known key bugs (Intel PTT TPM-FAIL timing leak,
  2019; Infineon ROCA, RSA only) is checked. A listed firmware is used for sealing but not
  for device-bound signing keys, and Curia suggests a firmware update.

## 5. Measured boot

### 5.1 What gets measured

| PCR | by | contents |
|---|---|---|
| 0, 2 | firmware | firmware and option ROMs: **not** in our policy, since a BIOS update would break it |
| 4 | firmware | `loader.efi` |
| 7 | firmware | secure boot state and the keys that allowed `loader.efi` |
| 8 | loader | loader configuration (FreeBSD main's code) |
| 9 | loader | the **Atrium boot manifest**: one signed list of the kernel and module hashes for this release (§6.2), measured once; each file is checked against it as it is loaded |

Measuring one manifest instead of each file keeps PCR 9 the same for every machine on a
release. Per-machine module choices (a GPU driver here, Wi-Fi there) come from the same
signed manifest, so they don't change it.

### 5.2 The policy

- The boot policy key signs `(release, PCR 4, PCR 7 when secure boot is on, PCR 9)`.
  PCR 8 is left out because local loader settings legitimately differ. Instead the
  Atrium loader ignores tunables outside a short allowlist (no `boot_single`, no
  `kern.securelevel` lowering, no extra modules).
- A new **`boot` key role** in Opifex's `root.toml`, scoped to boot policies and (§6)
  boot images, held offline on a hardware token like the root keys. It changes as rarely
  as the loader and is used once per release.
- Base system updates are outside the catalog in v1 (opifex.md decision 12), so the signed
  policies ship with the base image for now and move with base updates when those are
  designed.

### 5.3 Loader work

- Bring FreeBSD main's `stand/efi/libefi/tpm.c` into the fork, then add the manifest
  measurement to PCR 9. Atrium's Tessera loader (`atrium-tessera/loader/tessera.c`) reads
  files through the same `file_load` path, so it is measured the same way.
- Cost at boot: a few TCG2 calls, well under fast-boot.md's budget. To be measured in B0's
  harness.

## 6. Secure boot

### 6.1 Order

1. **Measured first (§5).** That alone stops a changed loader or kernel from opening the
   machine key, on any machine with a TPM.
2. **Atrium's own keys,** for machines whose owner opts in. Gives tier T.
3. **A Microsoft-signed shim, later,** only if Atrium should boot with secure boot on and
   without enrolling keys.

### 6.2 The verified chain

- `loader.efi` is signed with the **boot** key using `uefisign(8)`. uefisign takes a key
  file today, so it gets a PKCS#11 path (the same `libykcs11` route as
  `portcullis-token`, opifex.md §14.9). sbsigntools is GPL and isn't used.
- The loader is built with `LOADER_EFI_SECUREBOOT`/`LOADER_VERIEXEC` and checks the boot
  manifest's signature (boot key) and every kernel and module against it. With secure boot
  on, it refuses anything unlisted.
- Executables after boot are already covered differently: system services and apps come
  as signed bundles checked by Opifex and portcullisd. `mac_veriexec` for base binaries is
  left out of this spec.

### 6.3 Enrolling our keys

- Done once from Curia (or setup), with the owner's typed confirmation. The firmware must
  be in Setup Mode (the owner clears the platform key in the firmware screen; Curia shows
  how). Atrium then writes db, KEK and a platform key made on this machine.
- **db keeps Microsoft's UEFI CA** (2011 and 2023 third-party certificates) beside
  Atrium's. Discrete GPUs' option ROMs (the R9700 included) and Ubuntu's shim are signed by
  it. Removing it gives a black screen on many PCs. Microsoft's Windows CA stays too, so
  dual boot keeps working.
- The machine's platform key goes into its Arca machine store. Changing secure boot keys
  later needs it.

### 6.4 The shim route (not now)

shim is BSD-licensed, but Microsoft signs it only after a public shim-review. That needs an
EV certificate, SBAT metadata, and a next stage that refuses unsigned code all the way into
the kernel (kernel modules signed, no raw `/dev/mem` or `/dev/kmem`). Revisit after §6.2
holds on real hardware.

## 7. Development and test

- **Mac VM:** `run-vm.sh` gets `TPM=1`: start `swtpm socket --tpm2` with a per-VM state
  folder, add `-chardev socket,... -tpmdev emulator,... -device tpm-tis-device`. Check that
  our patched QEMU build has the TPM emulator backend. Booting still needs Shankar's typed
  OK through the coordinator.
- **Cloud amd64 VM:** the same with `tpm-crb` and OVMF; a second OVMF build with secure
  boot enabled for §6 tests.
- **Host tests:** `atrium-tpm` against swtpm on Linux in CI and the cloud container,
  including a man-in-the-middle swap of the primary key, lockout behaviour, and a power
  loss between NV increment and store write.
- **Real hardware:** run `atrium-tpm probe` on the R9700 box (Ubuntu's `/sys/class/tpm`
  first, then Atrium).

## 8. Build order

| step | what | done when |
|---|---|---|
| **P0** | `tpm.ko` loaded on arm64; swtpm in `run-vm.sh` and the cloud VM; `atrium-tpm probe`; R9700 box probed | probe prints the tier in both VMs and on the box |
| **P1** | `atrium-tpm` crate: sessions with parameter encryption, seal/unseal, policies, NV counter, Sign; fuzz target | host tests pass against swtpm, including the swapped-key refusal |
| **P2** | arcad owns `/dev/tpm0` (dispensatord rule, pinned devfs ruleset); machine store key sealed (§4.3, policy signed by a test boot key), `machine.key` migrated | `arca-vm-test.sh` machine store section passes with TPM=1, and fails closed after a changed PCR |
| **P3** | user root key mix (§4.2), recovery key required, passcode change, NV counter (§4.4) | unlock, wrong-passcode lockout, rollback refusal and TPM-loss recovery gates in the VM |
| **P4** | device-bound keys (§4.5): DBSC and device-bound passkeys | rest of Arca A4 |
| **P5** | loader: FreeBSD main's measurement, boot manifest in PCR 9, tunable allowlist; `boot` key role | PCRs match the signed policy in the VM; a changed kernel won't open the machine store |
| **P6** | secure boot: uefisign with PKCS#11, verified loader, enrolment in Curia | tier T in the secure-boot OVMF VM; then on the R9700 box with Ubuntu still booting |
| **P7** | system disk encryption: its own spec (tessera-encryption.md §11) | — |

## 9. Not in this spec

- Remote attestation (proving boot state to a server).
- A TPM-backed passcode-free unlock (TPM-only, like BitLocker's default). Atrium always
  asks for the passcode.
- Secure Enclave use on Macs, for either the Insula host or bare metal.
- `mac_veriexec` for base binaries; a Microsoft-signed shim (§6.4).

## 10. Decisions for Shankar

1. **arcad is the sole TPM owner.** *Recommend yes.* Alternative: a separate `tpmd`
   brokering for several daemons.
2. **Our own Rust TPM client** (§4.1). *Recommend yes.* Alternative: tpm2-tss through
   `tss-esapi` (BSD, but a large C stack in arcad's jail).
3. **User root = full Argon2id mixed with a TPM secret** (§4.2), with no PCRs on the user
   key. *Recommend yes.*
4. **Machine key sealed to a signed boot policy,** with a new offline `boot` key role
   (§4.3, §5.2). *Recommend yes.* Alternative: raw PCR values re-sealed at each update.
5. **Never take TPM ownership or change its lockout settings** (§4.1). *Recommend yes.*
6. **Recovery key required on TPM machines** (§4.2). *Recommend yes.*
7. **Secure boot: measured first, then our own keys with Microsoft's UEFI CA kept in db;
   shim later** (§6). *Recommend yes.*
8. **Apple Silicon and SBCs stay tier N;** no Secure Enclave work. *Recommend yes.*
