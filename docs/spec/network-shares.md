# Network shares: SMB, SFTP, WebDAV and NFS in Scrinium

**Status:** accepted 2026-11-14 (Shankar: all §9 decisions as recommended).
He asked whether the file manager mounts network shares and, if it belongs
there, for a security audit of doing it properly with the credentials. Built
so far: the S0 mount options, fusefs fuzzer and share-server syscall
profile (§4.4); S1 SFTP (2026-11-14, §10). Decisions are in §9.
**Scope:** connecting to a file server on the LAN or the internet, showing it
in Scrinium next to the sticks, handing its files and folders to apps through
the picker, and where the passwords and keys live.

## 1. Short answer

- **Today Scrinium has no network mounts.** The kernel can't do them either:
  `std.atrium` removes the NFS client, krpc and kgssapi, and `smbfs` can't be
  loaded (kernel-hardening.md §2). That stays.
- **It belongs in Scrinium as a place, not as code inside Scrinium.** The user
  adds a server in Scrinium and sees it in the sidebar; the protocol, the
  network and the password are handled by pieces Scrinium can't reach.
- **Each share is served by its own jailed user-space FUSE server**, the
  same shape as a stick's Capsa server (device-assignment.md §6.4): Rust,
  one share per process, Capsicum, a sysfil profile, a vnet whose pf lets it
  reach that one server and nothing else. The server's bytes are hostile
  input, and the worst a server exploit gets is that one share's own
  credential.
- **Passwords and keys stay in Arca.** The user types them into Arca's own
  credential sheet, never into Scrinium. Arca releases a password to a share
  server only after that server has proved who it is (an SSH host key, a TLS
  certificate, Kerberos), the courier rule of arca.md §5. An SSH key never
  leaves Arca at all: Arca signs.
- **Everything after the mount is what sticks already have:** a 0700 mount
  for the session's user, Scrinium's standing grant, the picker handing an
  app one file or one folder (scrinium.md §4), gone on lock and on
  disconnect.
- **The weak spot is SMB with a password and no Kerberos:** NTLM can't
  authenticate the server, so anyone who can pose as it on the LAN can try
  to crack the password offline. SMB 3.1.1 with encryption and a strict
  dialect floor limits what they get, and the UI says so (§5.3).

Order to build, by how well each protocol can be secured: **SFTP**, then
**WebDAV over HTTPS**, then **SMB 3.1.1**. **NFS** only as NFSv4 with
Kerberos (`krb5p`), later, or not at all.

## 2. What is at stake

| Asset | Where it is | Who must not get it |
|---|---|---|
| A share's password or token | Arca, class while-unlocked | every app, Scrinium, every other share's server, a server posing as this one |
| An SSH private key | Arca, use-not-read | everyone; only signatures leave Arca |
| The files on the share | the server; in flight; in the mount | apps the user did not pick them for; other users of the machine; the LAN |
| The user's identity and machine name | the user's head and the machine | LAN observers, servers that don't need it |
| The machine itself | kernel, other jails | a hostile server sending crafted replies |

## 3. Threats

| Attacker | Wants | What stops it (section) |
|---|---|---|
| **Hostile or compromised server** | code execution on the client from crafted replies; the user's other credentials | parsing only in that share's jail, Rust, fuzzed, bounded allocations (§4.1); it is only ever given its own credential (§5.1); FUSE is the only way its data reaches the kernel, already used for sticks |
| **LAN attacker** (same Wi-Fi, café, hotel) | pose as the server to get the password; read or change files in flight; downgrade the protocol | server identity checked before any credential is released (§5.1); pinned SSH host keys, TLS, SMB 3.1.1 pre-auth integrity and encryption, no SMB1/2.x, no guest fallback (§5.3); pf pins the server's address (§4.3) |
| **Spoofed discovery** | the user picks "NAS" that is really the attacker | discovery lists names only, and a name never inherits a saved credential; the identity check is what counts (§6.4) |
| **Malicious app** | the share's password; files it wasn't given; to make the system connect somewhere and leak a hash | apps never see credentials or the mount, only picked items (§6.2); no app or document can start a connection: only the user in Scrinium can (§6.3) |
| **Malicious link or document** (`smb://evil/x`, `\\evil\x`, a `.desktop`-style shortcut) | the classic NTLM hash leak: make the client authenticate to the attacker | there is no URL handler that connects with credentials; a link can at most open Scrinium's "Connect to server" form filled in, and a server never gets a credential saved for another (§6.3) |
| **Compromised Scrinium** | passwords; other users' shares | it never holds a password (§5.2); it sees the files of mounted shares, as it sees sticks |
| **Compromised share server** | other shares, other credentials, the user's files | it has one share's credential and network reach to one host; no filesystem but its own `/dev/fuse` (§4.1) |
| **Someone at the locked or stolen machine** | saved passwords; cached files | credentials are while-unlocked and Arca's keys are evicted at lock; shares are unmounted at lock; nothing is cached on disk (§7) |
| **A server that hangs or lies** | freeze apps; exhaust memory or disk | every FUSE reply has a deadline and becomes `EIO`, never a hang in the kernel; sizes and names are checked (§4.2) |

Out of scope: a server the user really trusts doing bad things with the files
the user gave it, and an attacker who already has root on this machine.

## 4. The share server

### 4.1 Shape

One process per mounted share: `atrium-share-sftp`, `atrium-share-dav`,
`atrium-share-smb` (names to be decided). It is started by the mount manager
(§6.1) through jaild, like a Capsa server, and holds:

- an already-mounted `/dev/fuse` descriptor (jaild's `fuse` request mounts
  fusefs from outside the jail, device-assignment.md §6.4), so the server
  can't choose where it is mounted or with what options;
- one socket to Arca, through which it can ask only for the items whose
  recipient is this share (§5.1);
- a network: its own vnet (`Routed`), with a pf anchor passing only the
  server's address and port, as Arca's courier jails do (arca.md §5.4).

No other files, no `/dev` beyond the fuse fd, Capsicum after setup, a
`networked` sysfil profile, racct limits on memory and descriptors. It runs as
its own uid from a fixed range, not as the user.

### 4.2 What it checks in the server's replies

Everything from the network is hostile:

- **Protocol parsing** in Rust with no `unsafe` beyond the FUSE and socket
  calls, every length bounded before it is allocated, every parser a fuzz
  target (SSH/SFTP packets, HTTP and PROPFIND XML, SMB2 headers, NTLM
  messages, NFS XDR).
- **Names.** Refused or hidden: `/` or NUL inside a name, `.` and `..`,
  names over 255 bytes, invalid UTF-8 (shown escaped later, as on sticks),
  control characters. Two names that differ only by Unicode normalisation or
  case on a case-insensitive server: the second is hidden, so one name
  never means two files.
- **Symlinks on the server** are not followed by the server and are shown as
  links Scrinium won't open (as on sticks, scrinium.md §2.2). Mounts are
  `nosuid,noexec` with no device nodes, so nothing on a share can become
  setuid or a device here.
- **Sizes and counts.** A directory listing is capped (entries and bytes);
  a read returns at most what was asked; a file whose size changes under a
  read is reported as such, not trusted.
- **Time.** Every request to the server has a deadline (connect, first byte,
  idle). When it passes, the FUSE reply is `EIO` (or `ETIMEDOUT`), so an app
  reading a file on a dead server gets an error instead of a process stuck
  in the kernel. The mount stays, marked disconnected in Scrinium, and
  reconnects on the next access.

### 4.3 The network

- The address is resolved once, through the system resolver, and the share
  server's pf anchor is set to that address alone before the server starts
  (no DNS rebinding: the name can't later point at something else).
- LAN addresses are allowed for a share because the user typed or picked
  that server: adding a share is the private-network grant nuntius.md §4
  describes, scoped to one address and port. The cloud metadata address is
  never allowed.
- Nothing listens. The client announces nothing on mDNS.
- What the client tells a server is the minimum: SSH client banner
  `SSH-2.0-Atrium`, SMB workstation name empty (NTLM allows it), no domain
  unless the user gave one, WebDAV `User-Agent: Atrium`.

### 4.4 The kernel side

Jails, Capsicum and sysfil confine the share server's process, but the
server still talks to the kernel through `/dev/fuse`, and fusefs parses its
replies. With sticks an attacker first had to break the stick server; with
shares, whoever breaks a share server over the network can send crafted
FUSE replies to the kernel. fusefs is therefore the one kernel boundary a
remote attacker can reach, and the kernel work is there:

1. **Fuzz fusefs from the server side** under the KASAN kernel: a hostile
   FUSE server sending malformed replies (lengths, attributes, sizes that
   change, invalidations at bad times, replies out of order). Bugs are
   fixed in the fork; fusefs can also be narrowed to the FUSE features our
   servers use. Built 2026-11-14: `atrium-fuse-hostile` (capsa-fuse) and
   `scripts/vm-fuse-fuzz.sh` (rounds of one seed each, then the VM must
   answer, the mount must be gone and dmesg clean; a seed reproduces a
   round). First find, on the INVARIANTS kernel: a process killed while
   fusefs waits on a `READLINK` inside `namei()` trips the
   `error != ERESTART` assertion and panics
   (docs/reviews/2026-11-14-fusefs-namei-erestart.md, reproducer
   `--repro readlink-kill`; release kernels unaffected). Second find: a
   server naming a directory's ancestor as its child makes lookups
   deadlock on vnode locks for good, release kernels included
   (docs/reviews/2026-11-14-fusefs-dir-cycle-deadlock.md, `--repro
   dir-cycle`). Third: fsync reused a request still queued when the server
   didn't answer, corrupting fusefs's request lists
   (docs/reviews/2026-11-14-fusefs-fsync-ticket-reuse.md, `--repro
   fsync-timeout`). All three fixed in the fork (`fbsd:a7f02efce`,
   `fbsd:52048d225`, `fbsd:47dfe5c94`); on that kernel the stock fusefs
   suite passes (690/771, 0 failed, 81 skipped) and 13 fuzz rounds (12,916
   requests, 3,754 mutated) ran clean. fusefs's own
   "FUSE protocol violation" warnings are the kernel catching the server
   and are expected.
2. **Mount options** set by jaild, done 2026-11-14 for every FUSE mount
   (sticks today): `nosuid` and `noexec` (nothing served runs here or gains
   privilege), plus `allow_other` and `default_permissions` as before; and
   an explicit answer timeout, `timeout=30` (jaild
   `FUSE_ANSWER_TIMEOUT_SECS`; fusefs reads the key `timeout=` and defaults
   to 60 s, at most 600). Every request the kernel sends waits at most that
   long and then fails with `ETIMEDOUT`, so a server that stops answering
   (a compromised one included) can't hold apps in the kernel; the session
   is not marked dead and recovers when the server answers again. `intr` is
   left off: fusefs only honours it for servers that implement
   `FUSE_INTERRUPT`, ours don't, and `SIGKILL` already ends a waiting
   process. Gate `vm-automount-check.sh`: the flags, a script on the stick
   that won't run, a stopped Capsa server making `ls` fail after two 30 s
   waits (DTrace on `fdisp_wait_answ` shows `timeout=30`) and working again
   once resumed.
3. **A sysfil profile** for share servers: `networked` minus what a FUSE
   server doesn't need; log-only first, like the others (jail-sysfil.md).
   Done 2026-11-14: `share-server` in etc/jaild.policy.toml, base plus
   inet/inet6 and the TCP keepalive options, no disk ioctls and none of
   `networked`'s IP options; learn.sh refines it once S1's server runs.
4. **Kernel memory a server can pin** (queued requests, buffers) that racct
   doesn't count: measured during the fuzzing, capped if it can grow.

Nothing else new is reachable: the share jail's vnet and pf are the shape
Arca's courier jails already use, and the NFS client, smbfs and kgssapi
stay out of the kernel.

## 5. Credentials

### 5.1 The rule: identity first, then the secret

Each saved share is an Arca item whose **recipient** is the share:
`(protocol, host, port, share or path, server identity)`. The server
identity is:

| protocol | identity | first connection |
|---|---|---|
| SFTP | the SSH host key (Ed25519 or ECDSA preferred; RSA ≥ 3072 with SHA-2; no DSA, no SHA-1) | shown as a fingerprint for the user to accept, then pinned in the item. A changed key later is a hard stop with its own sheet, never a click-through |
| WebDAV | the TLS certificate for the host name, through the system trust store (docs/TLS.md); optionally pinned | a certificate that doesn't validate is refused; a self-signed NAS can be pinned by fingerprint, shown as such |
| SMB with Kerberos | the service ticket for `cifs/host` | via the user's realm, later |
| SMB with a password | **none that a password protocol can prove** (§5.3) | the host's address and the server's GUID are recorded and a change is shown, but that is a hint, not proof |
| NFSv4 with `krb5p` | the service ticket for `nfs/host` | via the realm, later |

The share server connects, completes the handshake up to the point where
the server's identity is known, and **only then** asks Arca for its item.
Arca releases it only if the identity the server reports matches the item's
recipient. Arca knows which share server is asking from its uid (one uid per
running share, registered by the mount manager the way couriers are,
arca.md §5.3), so a share server can never ask for another share's item.

- **Passwords** go to the share server for the one login, are wiped after
  it, and are fetched again on a reconnect.
- **SSH keys** never leave Arca: the share server asks Arca to sign the
  SSH authentication challenge (Arca's `arca_sign`, the ssh-agent model of
  arca.md §9), bound to that share's recipient.
- **Tokens** (WebDAV services with OAuth, later) are courier-attached like
  any HTTP bearer item.

### 5.2 Entering a password

Scrinium's "Connect to server" form takes the address and the user name. The
password field is not Scrinium's: it is a Limen `credential` slot filled by
`arca-ui` (arca-autofill.md), so the text the user types goes to Arca and
Scrinium only learns "saved" or "wrong password". "Remember" is Arca's
choice to keep the item; without it the item lives until the share is
disconnected.

A password is offered for a share only to the recipient it was saved for.
Saving the same password for two servers is two items, each bound to its
own.

### 5.3 SMB without Kerberos, honestly

Home NAS boxes and Windows shares almost always use NTLMv2 with a password.
NTLMv2 proves the client to the server, never the server to the client. So a
LAN attacker who can answer for the server's address gets an NTLMv2
response, which is an offline-crackable function of the password, and can
try to relay it to another server.

What the share server does about it:

- **SMB 3.1.1 only**, with the pre-authentication integrity hash
  (SHA-512): an attacker in the middle can't change the negotiation, so no
  downgrade to an older dialect, an unsigned session or a weaker cipher.
- **Encryption required** (AES-128-GCM or AES-256-GCM); a server that won't
  encrypt is refused, with a sheet that says why. Signing alone is not
  enough because it leaves the files readable on the wire.
- **No SMB1, no 2.x, no guest or anonymous fallback**, no plaintext or LM
  or NTLMv1, ever.
- **No credentials to a server the user didn't add**, and the server is
  pinned by address at mount (§4.3), so an attacker has to win the address
  on the LAN, not just a name.
- **The UI says what this protects.** The first time a password-only SMB
  share is added, Scrinium shows one line: someone on this network could
  try to guess this password; use one you use nowhere else. Kerberos,
  when there is a realm, removes this.

What it can't fix: an attacker on the LAN who takes the server's address
gets one NTLMv2 response per connection to crack. That is the protocol, and
macOS, Windows and Linux clients have the same exposure.

### 5.4 NFS

NFS with `AUTH_SYS` (the default nearly everywhere) has no authentication:
the client states a uid and the server believes it. Any process that can
send packets from an allowed address can be anyone. There's nothing a client
can do to make that safe. Recommendation: no NFS in the first versions; if
it comes, NFSv4.1 with `krb5p` only, through a user-space client in the same
jail shape (the kernel's stays removed).

## 6. How it fits Atrium

### 6.1 Who mounts

The **mount manager** is the piece that today serves sticks: dispensatord's
media side (device-assignment.md §6.5, §6.11). A share becomes another
volume next to sticks, so the session mount (0700 at `/media/<name>`),
Scrinium's standing grant, the picker's file descriptors and folder grants,
eject (here: disconnect), lock and logout all work unchanged.

What changes in dispensatord is small and has no network code in it: it
gains a "share" volume source with `CONNECT {protocol, host, port, path,
user}` and `DISCONNECT`, it starts the share server through jaild with a
mounted `/dev/fuse` and the vnet of §4.1, and registers its uid with Arca.
dispensatord never parses a byte from the network and never holds a
credential. Alternative: a separate daemon for shares; it would duplicate
the grant code, so not recommended (§9, D3).

`CONNECT` is accepted from the session's user's file manager only
(`removable-files` holder, or a new capability `network-shares`; D4), with an
unlocked session, like `ATTACH_ALL`.

### 6.2 Apps

Apps never see a share's mount or its credential. An app gets a file or a
folder from a share exactly as from a stick: the user picks it in the
picker, the file comes as a descriptor (scrinium.md §4.2) and the folder as
a grant mount in the app's jail (§4.3). A folder grant on a share is the
same FUSE server serving a second mount rooted at that folder, owned by the
app's uid.

### 6.3 Who may start a connection

Only the user, in Scrinium's "Connect to server" form or by opening a saved
share in its sidebar. Nothing else can make the system connect to a file
server:

- no `smb://`, `sftp://` or `\\host\share` handler that mounts or
  authenticates; a link to one opens the form with the address filled in,
  and the user still presses Connect;
- no connection at login unless the user ticked "connect at login" for
  that share (D8), and then only to the pinned identity;
- an app can't ask for a share to be mounted, only for the picker.

This is what closes the SMB hash-leak class of attack (a document or page
that makes the client authenticate to the attacker's host).

### 6.4 Discovery

Browsing the LAN for servers (mDNS/DNS-SD `_smb._tcp`, `_sftp-ssh._tcp`,
`_webdavs._tcp`) is untrusted input and a privacy question. Not in the first
version: the user types the address. When it comes, it runs in its own
small jail, shows names and addresses only, and a discovered name never
inherits a saved credential: adding it is a new share with its own identity
check.

## 7. At rest, lock, and the end of a session

- **No disk cache** in the first version: reads go to the server. If a
  cache comes later it is in the session's encrypted area and dropped at
  logout.
- **Lock:** shares are unmounted like sticks, and Arca evicts the
  while-unlocked keys. Unlocking remounts the shares that were mounted,
  through the same identity-first login (D7).
- **Logout, user switch:** unmounted; the share servers' jails are removed.
- **Disconnect** (Scrinium's eject button on a share) ends the share
  server, its grants and its session with the server.
- Files open from a share when it goes get `EIO`, as on an unplugged stick.

## 8. What the gate must show

A VM gate in the style of `vm-scrinium-check.sh`, with servers on the host:
OpenSSH sftp-server, a WebDAV server over TLS, Samba with SMB 3.1.1
encryption. It checks:

- the password reaches Arca only through arca-ui, and Scrinium's log and
  memory never hold it;
- a server with a changed SSH host key, a bad certificate, or an SMB server
  that refuses encryption or offers only SMB2: refused before any
  credential leaves Arca (Arca logs no release);
- a share server can't fetch another share's item (asks, is refused);
- the share server's jail can reach the server's address and port only
  (a connect elsewhere fails);
- a fuzzed or stalling server: `EIO` within the deadline, no hang, the
  server process bounded;
- the picker hands a file and a folder from a share to an app, as for a
  stick; lock unmounts; unlock remounts with a fresh login.

## 9. Decisions, with recommendations

| # | Decision | Recommendation |
|---|---|---|
| D1 | Kernel clients (smbfs, NFS) or user-space? | User-space, one jailed FUSE server per share; kernel clients stay removed |
| D2 | Which protocols, in what order | SFTP, then WebDAV over HTTPS, then SMB 3.1.1; NFS not now (NFSv4.1 `krb5p` only, if ever) |
| D3 | Who mounts | dispensatord's media side, as another volume source (no network code in it); not a new daemon |
| D4 | Which app may connect shares | the file manager, via a new restricted capability `network-shares` (separate from `removable-files`, so a stick-only file manager can't reach the network) |
| D5 | Credentials | Arca items bound to protocol, host, port, path and server identity; released only after the identity check; SSH keys sign inside Arca and never leave |
| D6 | Password entry | arca-ui in a Limen credential slot inside Scrinium's form; Scrinium never sees it |
| D7 | Lock | unmount at lock, remount at unlock (like sticks) |
| D8 | Connect at login | off by default; per share opt-in, to the pinned identity only |
| D9 | SMB floor | 3.1.1 only, encryption required, no NTLMv1/LM/guest; Kerberos later; the one-line warning for password-only shares |
| D10 | Links (`smb://`, `\\host`) | open the Connect form filled in; never mount or authenticate on their own |
| D11 | Discovery | not in the first version; later in its own jail, names only |
| D12 | Disk cache | none in the first version |
| D13 | Protocol code | own Rust clients for SMB and WebDAV; for SSH, an existing pure-Rust SSH crate audited and fuzzed, or our own transport if review finds it wanting |

## 10. Build order once accepted

1. **S0 kernel side** (§4.4): the mount options (done 2026-11-14), the
   fusefs fuzzer under KASAN and the share-server sysfil profile, before
   any network code.
2. **S1 SFTP:** share-volume source in dispensatord, the jail/vnet/pf
   setup, `atrium-share-sftp` with Arca host-key pinning and signing,
   Scrinium's Connect form with the arca-ui slot, the gate's SFTP part.
   **Built 2026-11-14.** Arca: share items keyed by `Recipient::Share`
   (host, port, path and the SHA-256 of the host key), the file manager
   (`ARCA_FILE_MANAGER`) saves what was typed in its own credential field
   (a handle; it never sees the value), mints an Ed25519 key, lists and
   forgets; a share-server uid gets items only through its registry line
   (`/var/run/atrium/share-registry`, retired when the server ends) and
   only for the key it names. `atrium-share-sftp` checks the pin before
   asking Arca, logs in with the password or by Arca's signature (the key
   never leaves Arca) and serves owner-only modes. jaild's
   `[share_servers]`: FUSE with no device at `/media/share-<uid>`, fd 3.
   dispensatord resolves the host once, refuses loopback and link-local,
   runs `--learn` and the server in a routed vnet granted that one
   address and port (jaild's pf also keeps it off the machine itself), and
   ends a share on eject, lock or logout. Scrinium: "Connect to server"
   (address, the key to accept, Arca's password field) and saved servers
   in the sidebar. Gates on the cloud amd64 VM: `vm-share-sftp.sh` (Arca
   and the server, 19 checks) and `vm-share-jail.sh` (the jailed path
   through dispensatord and jaild), both 0 failed. Then, the same day:
   a saved server is asked for its key before connecting, and a changed
   key stops on its own sheet (no "connect anyway": Close, or forget the
   server and connect afresh, accepting the new key); a share's folders
   are granted as a stick's are (the file manager's standing grant,
   picked folders, read-only), once its server answers; Scrinium asks a
   share's free space off its window thread every 30 s.
   **Hangs and stuck jails** (Shankar 2026-11-14): nothing that serves a
   desktop waits on a FUSE server. jaild and dispensatord unmount by
   file-system id from the mount table (MNT_BYFSID), never by path, since
   a path lookup asks the mount for its root and a wedged server holds
   that; dispensatord's statfs of a mount runs on a side thread for at
   most 2 s, one at a time per mount; a share whose server doesn't answer
   is ended before it is unmounted (FUSE_DESTROY would wait on it), a
   stick's eject is refused instead; a share server's exit is seen at once
   on its process descriptor; a server whose SSH peer goes silent ended
   itself (25 s read timeout) rather than serve errors (since S4 it fails
   that request after 20 s and connects again); dispensatord at
   start removes every share jail and orphaned grant an earlier run left.
   Readers on a wedged mount fail after fusefs' 30 s timeout (S0).
   `vm-share-jail.sh` checks each of these: 47 checks, 0 failed.
   **The Connect sheet on screen** (`vm-scrinium-connect.sh`: frescod,
   limend, arca-ui, arcad, dispensatord and Scrinium jailed, driven by
   injected input against a real sshd; 18 checks, 0 failed): address, the
   server's real key, trust, Arca's field, connect, listing, disconnect,
   the changed-key sheet (no jail started, nothing released by Arca, Enter
   doesn't pass it) and forget. Found and fixed on the way: Limen's poll
   with a zero timeout was refused (EINVAL) and read as losing limend, so
   Arca's field never came; a typed password reaches the parent only as a
   handle (`Filled` is for Arca's chip), held when the field loses focus,
   so a press on Connect waits up to 1.5 s for the handle its own press
   brings, and says "Type the password in Arca's field first" if none
   comes; dispensatord showed a share before its server answered, so the
   file manager opened it before its grant existed: a share is now listed
   only once ready.
3. **S2 WebDAV over HTTPS**, reusing the TLS stack and courier rules.
   **Built 2026-11-14.** `atrium-share-webdav` (capsa-share: its own
   HTTP/1.1 client, WebDAV over roxmltree with DTDs off and a node limit,
   rustls with ring). Identity first: a server is either **trusted** (its
   certificate validates for the name through `/etc/ssl/cert.pem`; Arca's
   item is pinned to the name alone, key all zeros) or **pinned** (a
   self-signed NAS, by the SHA-256 of its public key, shown as
   `SHA256:…` with the certificate's subject, issuer and expiry, and why
   it doesn't validate). An unauthenticated OPTIONS proves the identity
   before Arca is asked; a different key, or a "trusted" server whose
   certificate stops validating, ends the server (status 3) with nothing
   released. `--learn` reports `trusted` or `untrusted <why>` and the
   certificate. Basic auth sends the password with every request, so the
   server holds it for the session in locked memory (jaild now gives
   share servers `allow.mlock`, as it does couriers). The process that
   parses TLS, HTTP and XML enters capability mode: a forked dialer, the
   only part that may `connect`, hands it sockets to the one address and
   port (so the `share-server` profile lists `fork` and `wait4`). Writes:
   a file written in order from empty streams up as a chunked PUT on its
   own connection; other writes are gathered in memory (at most 64 MiB a
   file) and PUT whole on flush, release or fsync. Reads use Range; a
   server that ignores it is cached whole for 5 s. Folders are MKCOL,
   rename is MOVE with Overwrite, `rmdir` checks the folder is empty
   first, quota comes from RFC 4331 properties when the server has them.
   The share jail adds `/etc/ssl` read-only; jaild's `[share_servers]`
   takes `exec_paths` (both servers). Arca's items carry the protocol
   (`ShareProto::Webdav`), the audit label is `share:webdav:host:port@user`.
   Scrinium's Connect takes `https://user@host[:port]/path` (or `davs://`;
   plain `http://` is refused): a trusted server goes straight to Arca's
   field, an untrusted one to the certificate sheet. Not yet: Digest
   auth, OAuth tokens, files over 64 MiB edited other than front to back.
   Gate `vm-share-webdav.sh` (Apache mod_dav over TLS on the host,
   `scripts/webdav-test-server.sh`; 31 checks, 0 failed): learn both
   ways, a trusted and a pinned share through dispensatord and jaild,
   read, list, write, a 1 MiB stream, rename and delete, another user
   refused, Arca filled once, eject; a wrong pin and a self-signed server
   saved as trusted both refused with nothing from Arca.
4. **S3 SMB 3.1.1** with NTLMv2 and encryption; Kerberos when a realm
   exists.
   **Built 2026-11-14** (password shares; Kerberos still later).
   `atrium-share-smb` (capsa-share `smb`, `ntlm`): our own SMB2 client,
   one TCP connection, one request at a time. Negotiation offers dialect
   3.1.1 alone with the SHA-512 pre-authentication hash and AES-256-GCM
   then AES-128-GCM; a server that answers another dialect, no SHA-512 or
   no AES-GCM is refused before any login (`--learn` prints `refused
   <why>`, and Scrinium shows it: "Not connecting to nas: the server won't
   encrypt"). The login is NTLMv2 in SPNEGO (no LM, NTLMv1, key exchange or
   anonymous; a guest or null session is refused); its final reply must be
   signed (AES-CMAC) with the key derived from the password and the
   pre-authentication hash, and every message after it is sealed with
   AES-GCM (unsealed replies are refused). The password is used for that
   one login and wiped (§5.1). The identity a share is pinned to is the
   server's GUID (the digest of it in Arca's item): a different GUID stops
   the server before Arca is asked, on the changed-server sheet. It is a
   hint, as §5.3 says, not proof. Scrinium's Connect takes
   `smb://[domain;]user@host[:port]/share[/folder]` and, before the
   password, shows that the server encrypts, its ID, and the one-line
   warning that someone on the network could try to guess the password.
   Names Windows can't use are hidden, a second name differing only by
   case is hidden, reparse points (links on the server) are neither shown
   nor followed. Writes go straight to the server (no gathering, unlike
   WebDAV); up to 8 files stay open, closed on flush, and the share's root
   stays open so the server keeps the session. A lost connection ended the
   share (status 4) until S4. Found on the way: capsa-fuse read
   SETATTR's mtime from the wrong offset (48, ctime, instead of 40), so
   `touch -t` and `cp -p` set every file's time to the epoch, on sticks
   too. Gate `vm-share-smb.sh` against Samba 4.19 on the host
   (`scripts/smb-test-server.sh`: 3.1.1 with encryption required, a 2.1
   server and an unencrypted one; 32 checks, 0 failed): learn and the two
   refusals, connect through dispensatord and jaild, read, list, write,
   edit in place, 3 MiB both ways, set a time, rename, delete, mkdir,
   rmdir (and a full folder kept), another user refused, a folder as the
   root, eject; a wrong GUID gets nothing from Arca, an unknown user gets
   no guest session.
5. **S4** disconnected-state UI, reconnect, "connect at login". *Built
   2026-11-14.* A lost connection no longer ends a share: its server keeps
   the mount, the request that found the loss fails (`EIO`), and the next
   request connects again at once; while the server can't be reached,
   requests fail at once and tries are spaced 2, 5, 10, 20, then every
   30 s (capsa-share `dialer::Backoff`), so no access waits on a connect
   more than once. A silent server fails a request after 20 s, inside
   fusefs' 30 s. Every connection, the first and later ones, comes from
   the server's dialer (`dialer.rs`, forked before anything else, outside
   capability mode): it connects to the one address and port and to
   Arca's socket, and passes each back over a socket pair, so the process
   that parses the network stays in capability mode and still reconnects.
   A reconnect repeats the identity-first login (§5.1): SFTP checks the
   host key and SMB the GUID before Arca is asked, then logs in with a
   credential Arca gives again (used once and wiped); WebDAV checks the
   certificate or pin and uses the password it holds in locked memory. A
   changed key or GUID (status 3), a refused login or a credential gone
   from Arca (status 4) ends the share, which dispensatord reaps. The
   development path without Arca (`--password-fd`) still ends on a loss.
   Scrinium asks a share's size every 30 s off its window's thread, every
   5 s while it fails: those statfs calls are what bring a share back when
   nobody is using it. A share that doesn't answer reads "Reconnecting…" in
   the sidebar, a folder of it that can't be read says the server can't be
   reached, and it is read again when the share answers. Lock (§7, D7):
   dispensatord remembers the user's shares as it ends them; the same
   user's unlock connects each again, through its server's own identity
   check and a new login (Arca, unlocked a moment later, is waited for up
   to 10 s); logout or another user forgets them. Connect at login (D8):
   off unless ticked, in the Connect form ("Connect when I log in", only
   for a share Arca remembers) or for a connected share in Scrinium's
   status line; dispensatord keeps the list (`<state>.shares`: session
   user, protocol, host, port, path, user, the pinned identity, read-only),
   checks each line again when it reads it, resolves off its loop and
   connects them when that user's session starts. Only the session's file
   manager asks or sets it (`AT_LOGIN`, `--at-login PART on|off|ask`). To
   stop one that can't connect any more, connect it and untick, or forget
   its password. Gates: `vm-share-jail.sh` (SSH session silent, sshd down
   and back, lock and unlock, user switch and login), `vm-share-webdav.sh`
   and `vm-share-smb.sh` (server down and back).
