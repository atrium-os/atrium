# Codebase review, 2026-11-24: bad code and agent slop

Requested by Shankar: a skeptical pass over Atrium's own code for bad code and
the patterns people expect from agent-written code, with a prioritized fix
plan. Phase 1 is read-only except fixes Shankar OKs. The thread stays on as the
adversarial reviewer for new code (§6).

## 1. Scope and method

Atrium's own code is about **940,000 lines of Rust in 185 crates**, 108,000
lines of C (36,480 of them in one file, `tessera_fs.c`), and 44,700 lines of
shell in `scripts/`. FreeBSD base, Ladybird, and vendored code
(`third_party/`, `vendor/`) were out of scope.

Nobody reads 940k lines line by line in one pass, so the review had three
layers:

1. **Mechanical sweeps over the whole tree**: `unwrap`/`expect`/`panic` outside
   tests, `unsafe` blocks against `SAFETY` comments, ignored results
   (`let _ =`, `.ok();`), TODO and stub markers, `allow(dead_code)`, functions
   copied across crates, `recvmsg` implementations, tests with no assertion,
   near-duplicate scripts, crates nothing references, compiler warnings, and
   the licence of every third-party crate version in all 185 `Cargo.lock`
   files.
2. **Reads of the trust boundary**: every place a less-trusted party's bytes
   reach a more-trusted process. That covers jaild, portcullisd and its IPC,
   the launch registry, the jail.conf renderer, Aqueduct, frescod's client
   handling, Arca's framing, forum-wm's control socket, and Tessera's
   superblock checks.
3. **Proof of concept before a claim**: each high or critical finding about
   runtime behaviour was reproduced on the host. The reproductions are
   described with the findings.

Not covered in depth: application logic in Officina, Mail, Navigator, the GPU
compilers and the Vulkan ICD. Those were only swept mechanically.

## 2. Overall verdict

The code is not the slop people expect, and several things are better than
in typical human-written projects:

- **Dependencies are lean.** There is no tokio, lazy_static, once_cell, nix or
  reqwest. Daemons use std plus a few vetted crates.
- **Hostile input is taken seriously in the places that matter most.** Arca
  bounds every frame and keeps secrets in locked, zeroed buffers. jaild
  validates every field against policy. Tessera checks superblock regions
  against the device size before allocating anything. Mail, Crypta, Capsa and
  Arca have fuzz targets.
- **The tests are real.** Of 6,458 `#[test]` functions, 80 contain no visible
  assertion. Nearly all of those call a helper that asserts, or are
  "never panics on junk" tests, which is legitimate. Only two are hollow
  (L3).
- **There are almost no TODO stubs.** The whole tree has 5 TODO/FIXME markers
  and no `todo!()` or `unimplemented!()`.

The real problems are of a different kind: **a few sharp security bugs at
trust boundaries**, **one privileged daemon that has absorbed every feature**,
**the same security-sensitive helper copied a dozen times**, and **docs and
comments that read like a changelog**. These are the findings worth fixing.

## 3. Findings

Severity: **Critical** = a less-trusted party gets root or another party's
authority. **High** = a less-trusted party can take down a trusted service, or
an authority check fails open. **Medium** = a latent bug, or drift that makes
the next bug likely. **Low** = hygiene.

Effort: **S** = under a day, **M** = a few days, **L** = a design change.

| ID | Sev | Component | Finding | Effort | Status |
|---|---|---|---|---|---|
| C1 | Critical | portcullis-jail, portcullis-toml | An app manifest's entry could add jail.conf parameters (host root) | S | **Fixed `5bf45cf`** |
| H1 | High | aqueduct | CAS uploads are unbounded: any peer can make the server buffer without limit | S | **Fixed `e06c48a`** |
| H2 | High | portcullis-ipc, portcullisd | Request line length and connection count are unbounded in the root broker | S | **Fixed `7ab7b5e`** (connection cap not yet VM-tested) |
| H3 | High | portcullisd, spec | portcullisd creates jails itself as unconfined host root, contrary to the spec | L | Open: Shankar chose (a), route through jaild (2026-11-24) |
| H4 | High | portcullis-peer | A launch registry that fails to read is treated as empty (fails open) | M | Open |
| H5 | High | forum-ctl, forum-wm | The WM control socket has no frame cap and no timeout | S | **Fixed `05b0e6a`** |
| M1 | Medium | 25 crates | SCM_RIGHTS receive code is hand-rolled 25 times, with inconsistent hardening | M | Open |
| M2 | Medium | netconf, navigatord | Socket permission changes that act as access control ignore their errors | S | **Fixed `da28210`** |
| M3 | Medium | portcullisd | One root daemon carries 71 request kinds, with app features inside the broker | L | Open |
| M4 | Medium | jaild, many crates | Security predicates and helpers are copy-pasted | M | Open |
| M5 | Medium | tessera, gpu-rs, vk-icd, atrium-net | `unsafe` without justification; FFI entry points with no panic boundary | M | Open |
| M6 | Medium | atrium-tessera | One 36,480-line C file in the kernel | L | Open |
| M7 | Medium | repo | The licence policy describes enforcement that does not exist | S | Open |
| M8 | Medium | frescod, tabula, others | A poisoned mutex takes down every client | M | Open |
| L1 | Low | everywhere | 2,304 "★" war-story comments, dangling ticket numbers, misplaced doc comments | ongoing | Open |
| L2 | Low | portcullis-peer | A four-level wrapper chain for one function | S | Open |
| L3 | Low | repo | Dead crates, a dead CI workflow, fake tests, tracked `scratch/` | S | Open: needs an OK to delete |
| L4 | Low | scripts | Copy-pasted gate boilerplate (44.7k lines of shell) | M | Open |
| L5 | Low | docs | HANDOFF cells of 83 KB, a stale README, spec drift | S | Open |
| L6 | Low | several | Compiler warnings, silent parse drops, a personal path in tests | S | Open |

### C1. A manifest entry could inject jail.conf parameters (fixed)

portcullisd renders each app jail as a jail.conf section and runs
`jail -c -f`. The manifest's entry went into `exec.start` through
`portcullis-jail`'s `quote()`, which only had a `debug_assert!` against `"`, so
release builds escaped nothing. `[app].entry` was checked only for a leading
`/` and for `..`. `[bundle].entry`, which overrides it, was not checked at all.

**Proof of concept.** The manifest
`entry = 'bin/y"; exec.prestart = "/usr/bin/touch /root/pwned'` passed
`validate()`. A release build rendered it as:

```
exec.start = "/bin/y"; exec.prestart = "/usr/bin/touch /root/pwned";
```

jail(8) runs `exec.prestart` on the host as root. Any signed app could do
this, and so could an unsigned one where trust is not configured.

The conf file was also written as root to a predictable name in `/tmp`
(`portcullisd-<pid>-<name>.conf`) with `fs::write`. That write follows
symlinks, and it writes into a file another user planted at that name, which
that user can rewrite before jail(8) reads it.

**The fix (`5bf45cf`).**

- All three entry fields must be plain paths.
- `render_jail_conf` returns `Result`. It refuses `"`, `\`, `$` and control
  characters in values, names, keys and symbolic values that are not plain
  tokens, and mount fields that contain whitespace.
- A new `write_jail_conf` creates the file with `O_EXCL` and mode 0600 under a
  fresh name.

Tests replay the proof of concept. portcullisd, portcullis-cli and
atrium-session pass `cargo check` for aarch64-unknown-freebsd.

### H1. Aqueduct CAS uploads are unbounded

`aqueduct/src/connection.rs:395` (`UPLOAD_DATA`) appends a chunk when its
offset equals the current length, but never checks it against the declared
`total_size`. Separately, `incoming_uploads` has no limit on how many uploads
can be pending at once, and each `UPLOAD_BEGIN` reserves `total_size` (up to
16 MiB) at once.

**Proof of concept** (host unit test against an unmodified copy):

- An upload that declared 16 bytes buffered 4 MB.
- 1,000 `UPLOAD_BEGIN`s reserved 16,004 MiB.

Every Aqueduct server is exposed, frescod included, and frescod's clients are
apps.

**Fix:**

- Reject `UPLOAD_DATA` once `len + data.len()` would exceed `total_size`.
- Cap pending uploads per connection (for example 8) and the bytes they hold
  (for example 32 MiB).
- Reserve capacity lazily.
- Make each limit fire once in a test as a positive control.

### H2. The root broker reads an unbounded request line on unbounded threads

`portcullis-ipc/src/lib.rs:1141` `read_request` calls `read_line` with no
limit. portcullisd uses it for every connection, from any local uid, jailed
apps included, and spawns one thread per connection with no cap
(`portcullisd/src/main.rs` around line 193). A peer that sends bytes without a
newline grows root's memory without bound, and a peer that opens connections
exhausts threads. The response reader in the same file already caps lines at
1 MiB, so the hardening went into the client side only.

**Fix:**

- Read through `take(MAX_REQUEST + 1)` and reject a line over the limit.
- Cap live connections per uid.
- Add a read timeout on the handshake.

### H3. portcullisd creates jails itself, as unconfined host root

`docs/spec/portcullis.md` §2 describes jaild as the "sole caller of jail_set /
jail_remove", with portcullisd Capsicum'd inside its own jail.
`navigator-islands.md` restates that rule. In practice:

- portcullisd runs on the host as root, with no `cap_enter`.
- It shells out to `jail -c -f` for every app, workspace and console
  (`launch.rs` `run_one_jail_inner`, `create_persistent`).
- It makes 51 other `Command::new` calls (`jail -r`, `pkill`, `mount`, `pw`,
  `pwd_mkdb`, and others) and parses their text output.

jaild's careful validator (`jaild/src/validator.rs`) therefore gates only
network allocation and the one-shot lane, not ordinary app jails. C1 was
reachable because of this.

**Options, for Shankar to choose:**

- (a) Route app, workspace and console creation through jaild
  `CreateJail`, which the spec describes.
- (b) Change the spec to match reality and harden portcullisd as the real
  TCB.

(a) is the stronger design. It is L effort and touches the launch path that
the Sheets, Navigator and VPN threads use daily.

### H4. The launch registry fails open

`AppRegistry::load(path).unwrap_or_default()` appears at about 25 call sites
(portcullisd, ostiarius, forum-wm, share-sheet, portcullis-peer). `load` reads
the whole file with `read_to_string`, so **any read error, or one non-UTF-8
byte, turns the registry empty**. With an empty registry:

- `portcullis_peer::allocate` (`portcullis-peer/src/lib.rs:46`) returns
  `APP_UID_BASE` or the next free uid, which **may already belong to another
  app**. Two apps then share a uid, and with it each other's grants, overlay
  and Arca items.
- In `portcullisd serve()`, `registry.resolve(uid)` returns `None` for every
  jailed app. The check "an app may start only its own id"
  (`ExecInstance`, `main.rs` around line 430) is skipped, and the app is
  treated as a person's CLI.

`parse` also drops a malformed line silently, so one bad line hides a binding
with no error.

**Fix:**

- `load` returns an error for anything except `NotFound`, and every caller
  denies on error. `allocate` returns `io::Result`.
- Read the file as bytes and void only the bad line, with a log message.
- portcullisd loads the registry once per request through one helper, not 25
  ad-hoc reads.
- Add a test with a corrupt registry that must deny.

### H5. forum-wm's control socket has no frame cap and no timeout

`forum-ctl/src/lib.rs:580` `read_frame` allocates whatever the 4-byte length
says, up to 4 GiB. forum-wm serves the socket (mode 0666, reachable by jails
that hold `forum-control`) with a **serial** accept loop and blocking reads
(`forum-wm/src/main.rs:731`, `:788`). One chrome client that sends nothing
stalls window-manager control, and one bad length aborts the process.

**Fix:**

- Cap frames at the largest `Intent` (a few KiB).
- Set a read timeout on accepted streams.
- Consider sockmux's `LengthPrefixed`, which already does both.

### M1. SCM_RIGHTS receive code is hand-rolled 25 times

25 files call `libc::recvmsg` directly:

- Only 5 check `MSG_CTRUNC`.
- About 13 do not set `MSG_CMSG_CLOEXEC`.
- Several read only the first fd and leak any others a peer sends. For
  example, `lyra-protocol/src/fdpass.rs:33` takes one fd from the first cmsg
  and ignores the rest.

`send_fd` has 12 copies in 9 crates. `portcullis/sockmux/src/fdrecv.rs` is the
best of them: it checks truncation and closes extras.

**Fix:** one audited fd-passing module (sockmux's, extended), with callers
moved over one crate at a time by the thread that owns each crate.

### M2. Permission changes that act as access control ignore errors

These are fail-open: if the call fails, the socket keeps its default mode.

- `atrium-net/netconf/src/bin/atrium-netconf.rs:401,406`: the `root_only`
  0700/0600 directory and socket modes.
- `navigatord/src/main.rs:2205`: the control socket's 0600, which keeps
  another persona's Navigator out.

**Fix:** treat failure as fatal, or bind under a umask that already gives the
right mode.

### M3. The root broker has absorbed app features

- `portcullisd/src/main.rs` is 4,320 lines.
- `serve()` alone is 740 lines.
- `portcullis_ipc::Request` has 71 variants, including Sheets workbook
  recovery, calendar updates, IDE runs, consoles and links.
- App ids are matched as special cases inside the broker (Sheets 9 times,
  Nuntius 9 times, Hospes CDM 6 times, Mail 5 times).

Every feature added there is attack surface in a root process, and each
hand-written special case is a place the next check can be forgotten.

**Fix (L):**

- Split request handling into one module per area behind a small dispatch.
- Move app-specific flows (workbook recovery, calendar) into the services
  that own them, which then ask portcullisd only for generic primitives.
- Do this per area, coordinated with each owning thread.

### M4. Security predicates and helpers are copy-pasted

- `jaild/src/validator.rs:136/161/240/289`: `is_courier`, `is_share_server`,
  `is_print_worker` and `is_crypta_engine` are near-identical, with the
  same prefix, uid-range, path and uid/gid checks. A fix to one will miss the
  others. They should be one generic "service class" check driven by policy
  data.
- The peer-uid helper has 16 copies in 13 crates. `fn hex` has 43 copies in
  18 crates.
- `portcullis_peer::username` (`lib.rs:103`) uses non-reentrant `getpwuid`
  and is called from threaded daemons, while portcullisd has its own
  reentrant copy (`username_for_uid`). Keep one reentrant version.

### M5. `unsafe` without justification, and FFI with no panic boundary

| Crate | `unsafe` uses | `SAFETY` comments |
|---|---|---|
| atrium-tessera | 392 | 10 |
| atrium-gpu-rs (kernel) | 151 | 1 |
| atrium-net | 132 | 15 |
| atrium-vk-icd | 2,839 | 18 |

`atrium-vk-icd` exports 517 `extern "C"` functions with no `catch_unwind`, and
its library has 101 `unwrap`/`expect` outside tests. A panic aborts the host
app.

**Fix:**

- Enable `clippy::undocumented_unsafe_blocks` crate by crate, starting with
  the kernel crates.
- Add a panic guard at the ICD's entry points that returns
  `VK_ERROR_UNKNOWN`.

### M6. One 36,480-line C file in the kernel

`atrium-tessera/kmod/tessera_fs.c` holds the VFS glue, journal, allocator,
garbage collection, snapshots, encryption hooks and registry overlay in one
file. LANGUAGE-POLICY.md reserves C for VFS and newbus glue.

**Fix (L):** split the file by subsystem first, a mechanical step that is easy
to review. Then move non-glue logic to Rust over time, which the existing
differential fuzzing between `TESSERA_RUST_DECODE` and C makes safe.

### M7. The licence policy describes enforcement that does not exist

LICENSING-POLICY.md says `cargo deny` runs in CI, that every repo ships a
`LICENSES.md`, and that CI fails on a forbidden licence. There is no
`deny.toml`, no `LICENSES.md`, and no such CI job. The only workflows are
`hospes-version-watch.yml` and a dead `insula.yml` (L3).

The 185 separate lockfiles also drift: syn has 3 patch versions and
thiserror is at both 1.x and 2.x. A security advisory has to be fixed 185
times.

**Licence sweep (this review):** clean (§5); no tool keeps it that way.

**Fix:**

- Add `scripts/licence-audit.sh`, which walks every `Cargo.lock` against the
  policy's allowlist (the sweep in §5, kept).
- Add a `cargo audit` pass over the same set.
- Either correct the policy text or add a CI job.

### M8. A poisoned mutex takes down every client

frescod and tabula hold shared state behind `Mutex` and call
`lock().unwrap()` throughout (frescod's `socket_server.rs` alone has dozens).
A panic while one client's request holds the lock poisons it, and every later
request from every client then panics. One bad request becomes a
compositor-wide outage.

**Fix:**

- Recover the guard from a poisoned lock (`lock().unwrap_or_else(|p|
  p.into_inner())`) where the state stays consistent.
- Or catch panics per request and drop only that client.

### L1. Comments that read like a changelog

- **War stories.** 2,304 "★"/"★★"/"★★★" comments in 328 files narrate
  history ("It used to…", "Measured 2026-09-22…"). Some record a real
  invariant. Most belong in the commit message or a review doc, and they
  bury the code.
- **Dangling ticket numbers.** "#118", "Task #33" and "#73" point at a
  tracker that does not exist in this repo.
- **Misplaced doc comments.** At `frescod/src/socket_server.rs:508` a doc
  comment for `window_owner_uid` was left above `client_uid`, so both
  functions are documented wrongly.

**Fix:** don't churn old code. For new code, comment the invariant, not the
history (§6 checklist). Convert war stories when a file is touched anyway.

### L2. A wrapper chain for one function

`portcullis_peer::register` → `register_with_publisher` →
`register_instance` → `register_document`: four public functions that each
add one `None` argument. Use one function with an options struct.
`register_document` also does not reject whitespace, `#` or newlines in
`user` or `app_id`. It relies on every caller having validated them, and one
newline would forge a registry line.

### L3. Dead code and fake tests

- **Crates nothing references** (no Cargo path or script mentions them):
  `memprobe`, `atrium-pt-demo`, `forum-demo`, `aqueduct-echo`,
  `kmod-rust-spike`.
- **A dead CI workflow.** `.github/workflows/insula.yml` watches `insula-*`,
  `atrium-netd-macos`, `atrium-fetch` and `atrium-mon`, none of which
  exist. `scripts/build-insula.sh` builds them.
- **Tracked `scratch/`.** It is in git (3,385 lines of Rust and 60 or more
  files) even though AGENTS.md says never to commit it.
- **`allow(dead_code)`** appears 105 times. For example,
  `frescod/src/socket_server.rs:652` `live_clients`.
- **Fake tests.** `fresco-aqueduct-bridge/tests/end_to_end.rs:274`
  `_unused_idnamespace_warning_suppressor` is a test that exists to silence a
  warning. `fresco-compute/tests/fill.rs:275` `cache_speed` is a benchmark
  that prints and asserts nothing.

Deleting crates and the workflow needs Shankar's OK.

### L4. Copy-pasted gate boilerplate

There are 258 shell scripts with 44,700 lines between them. The same blocks
recur: SSH options in 53 scripts, `bad()`/`check()` in 53 and 34, ROOT
detection in 137, and the portcullis/opifex install block in 25. There are
356 `|| true` (most legitimate cleanup). `scripts/lib/` exists but is
under-used.

**Fix:** a `scripts/lib/gate.sh` (ok/bad/check/summary and exit code, VM
access, deploy of main's build), adopted when a script is next touched. No
mass rewrite.

### L5. Docs that drift

- `docs/HANDOFF.md` calls itself "the short list", but its Mail cell is
  83 KB and its Sheets cell 63 KB. It says "Last updated: 2026-11-19". A
  handoff row should be a few lines that link to the spec's as-built
  section.
- README's status section is "As of 2026-10-02".
- The spec is wrong about portcullisd's confinement (H3).

### L6. Small things

- `cargo check` warnings: frescod 22; portcullisd has unused imports
  (`supervisor.rs:1031`); portcullis-sig has an unused import
  (`lib.rs:192`).
- jaild's tests hardcode `/usr/home/girivs`.
- `portcullis-toml` depends on `thiserror` but does not use it.
- `fresco-aqueduct-bridge`'s tests no longer compile: `RectParams` gained a
  `radius` field the tests never set. A suite that doesn't build checks
  nothing.

## 4. Fix plan

The plan avoids threads that are actively committing today (Navigator, VPN,
Sheets/Officina, Mail, Wi-Fi, dev tools, printing). Shared files those
threads touch, mainly `portcullisd/src/main.rs` and `launch.rs`, are changed
only in small, quick commits, announced to the coordinator first.

| Wave | Items | Who | Collision risk | Effort |
|---|---|---|---|---|
| 0 | C1 | this thread | done | done |
| 1 | H1 (aqueduct), H2 (portcullis-ipc read limit, plus the portcullisd connection cap), H5 (forum-ctl/forum-wm), M2 (two permission calls), L6 | this thread | Low: library-level and self-contained. H2's cap touches portcullisd `main.rs` around line 193 only | 1–2 days |
| 2 | H4 (registry `Result`, deny on error, one loader), L2 | this thread, one commit | Medium: about 25 call sites in portcullisd, ostiarius, forum-wm and share-sheet. Do it when the coordinator says the portcullisd threads are idle | 2–3 days |
| 3 | M1 (shared fd passing), M4 (jaild service-class predicate, one peer-uid/username/hex helper), M7 (licence and audit script), M8 (poison handling in frescod and tabula) | this thread for the shared crate; owning threads migrate their own call sites | Low per crate | about 1 week, spread out |
| 4 | L3 (delete dead crates, workflow and `scratch/`), L4 (`gate.sh`), L5 (HANDOFF trim, README) | this thread | Low; HANDOFF trim done with each row's owner | 2 days |
| 5 | H3 (jail creation through jaild, or a spec change), M3 (break up the broker), M6 (split `tessera_fs.c`), M5 (`unsafe` documentation lint) | design first, then the owning areas | High: launch path, Tessera | weeks; needs Shankar's decisions |

Each fix lands with a test that fails on the old code (a positive control),
per AGENTS.md "How to verify".

## 5. Licence sweep

All 981 distinct (crate, version) pairs from the 185 lockfiles were checked
against crates.io's licence field. Result: **clean**.

- Every one of the 981 pairs offers a licence the policy allows.
- `r-efi` is MIT OR Apache-2.0 OR LGPL-2.1-or-later. We take it under MIT,
  and it only builds for UEFI targets anyway.
- `libbz2-rs-sys` uses the bzip2-1.0.6 licence, which is BSD-style and
  permissive. Add it to the allowlist explicitly.
- No crate is GPL-only, MPL or EPL.

Vendored code (`hospes/vendor`, `navigator-fonts/vendor`,
`vinculum/third_party`) and the Navigator engines were not part of this
sweep. M7 still stands: the check exists now only because this review ran
it.

## 6. Ongoing adversarial review of new code

Proposal:

1. **Review on request.** Say "review <commit range | area | today's
   commits>" in this thread. I read the diffs against the checklist below and
   report findings ranked by severity, each backed by a reproduction or a
   file:line reference. Fixes go back to the owning thread, or I make them
   when they are small and that thread agrees.
2. **Optional daily pass.** Once a day, review the previous day's commits to
   main that touch a trust boundary (`portcullis/`, `jaild`, `arca/`,
   `aqueduct/`, `frescod/`, `sockmux`, `atrium-tessera/kmod`, `crypta/`,
   `sigillum/`). Post only when there is a finding.
3. **A checklist in AGENTS.md**, added 2026-11-24 with Shankar's OK
   (text below).

The AGENTS.md section:

```markdown
## Review checklist for new code

Before pushing, check your diff against these. An adversarial reviewer
thread checks them too.

- Every value from a less-trusted party is bounded before it is buffered:
  frame and line lengths, counts of pending items, declared sizes.
- No `unwrap`, `expect`, slicing or `as` casts on bytes a peer controls.
  Return an error and drop that peer.
- An error on an authority check denies. Never `unwrap_or_default()` a
  file or answer that grants access; never ignore the result of a
  `set_permissions`/`chown` that is the access control.
- Text handed to another parser (jail.conf, fstab, shell, pf, TOML,
  registry lines) is built from validated tokens, and the renderer refuses
  rather than escapes anything else.
- Root never writes a predictable path in a shared directory: `O_EXCL`,
  0600, fresh name, or a root-only directory.
- Reuse the shared helper (fd passing, peer uid, framing, hex) instead of
  writing a new one.
- Every `unsafe` block has a `SAFETY:` comment saying why it holds.
- A comment states the invariant, not the history; history goes in the
  commit message.
- A test asserts something, and fails on the code before the fix.
- No new code in portcullisd for one app's feature: ask whether the
  owning service can do it with a generic primitive.
```
