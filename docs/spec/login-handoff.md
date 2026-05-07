# Login privilege handoff — vestibulum → user supervisor

**Status:** spec, 2026-05-07
**Owner:** D2 (vestibulum) + D2.5 (Portcullis)

How an Atrium boot transitions from "kernel up; no users" to
"user N has an authenticated session running their supervisor and
apps." Designed for the privsep architecture committed in
`scratch/jail-smoke/`: jaild is the only thing that calls
`jail_set(2)`; portcullisd is Capsicum'd; everything else runs in
its own jail.

## Components

- **`jaild`** — privileged broker. Sole `jail_set`/`jail_remove`/
  `pdfork`/`execve` caller. Validates each request against
  `/etc/atrium/jaild.policy.toml`. Cannot be Capsicum'd.
- **`portcullisd`** — policy daemon. `cap_enter()`s after init.
  Holds a persistent socket fd to jaild. Tracks sessions. Tracks
  per-jail procdesc fds for exit notification.
- **`vestibulum`** — pre-auth login screen. Owns user-input until
  authentication completes. Per-seat (one vestibulum instance per
  seat).
- **`atrium-supervisor`** — per-user, per-seat session manager.
  Runs as uid=N inside its own jail. Drives session-lifecycle
  events to portcullisd (logout, lock, switch-user). Spawns the
  user's startup apps via portcullisd.

## Process descriptors are the spine

`pdfork(2)` + `EVFILT_PROCDESC` is the FreeBSD-native way for one
process to safely track another's lifecycle, and crucially **it
works in Capsicum mode**. The pattern through the whole protocol:

```
jaild            portcullisd
─────            ───────────
pdfork() ─→ pdfd
fork-child does jail_set + execve
parent sends pdfd via SCM_RIGHTS ──→ recv pdfd
                                     EV_SET(EVFILT_PROCDESC, pdfd, NOTE_EXIT)
                                     kevent() for EOF
                                     pdkill(pdfd, SIGTERM)   for shutdown
```

portcullisd never needs `kill(pid, sig)`, never needs to enumerate
processes globally, never needs a path to access the process. The
procdesc fd *is* the handle. Capability-pure.

## Phase 1: boot, before any users

1. `/etc/rc.d/atrium-jaild` starts jaild. FreeBSD `service(8)`
   supervises. jaild reads `/etc/atrium/jaild.policy.toml`, parses,
   validates, opens `/var/run/atrium/jaild.sock` (mode 0600,
   owner root).

2. `/etc/rc.d/atrium-portcullisd` starts. The rc script is small:
   it `connect(2)`s to `jaild.sock`, sends a `bootstrap_portcullisd`
   request, then exits. jaild does `pdfork`, the child does
   `jail_set(JAIL_CREATE | JAIL_ATTACH)` + `execve("/usr/local/sbin/atrium-portcullisd")`,
   the parent records the procdesc fd. (FreeBSD `service(8)`'s
   own respawn mechanism doesn't directly know about jails;
   jaild becomes the supervisor for atrium-managed services.)

3. portcullisd:
   - inherits the jaild socket fd via env var `ATRIUM_JAILD_FD=<n>`
     (jaild dup2's the socket fd into the child before execve).
   - reads `/etc/atrium/services.d/*.toml` (ro mount in its jail).
   - reads `kern.atrium.seats` sysctl (or whatever the equivalent
     ends up being — for now, hardcoded "seat0" until multi-seat
     work in D5+).
   - opens `/var/run/atrium/sessions/` rw (its own state dir).
   - calls `cap_enter()`. From here on it's pure capability mode,
     using only the fds it already has.

4. portcullisd asks jaild — over the persistent socket — to start
   each system service:
   - `frescod` (devfs ruleset `atrium-gpu`, no network)
   - `atrium-devevents` (devfs ruleset `atrium-input`, no network)
   - `vestibulum` per seat (devfs ruleset `atrium-baseline`, no
     network, mounts of `/etc/master.passwd` and `/etc/login.conf`
     ro for in-process auth)

   For each, jaild returns a procdesc fd via SCM_RIGHTS.
   portcullisd kqueues on each for exit-notification.

5. State at end of phase 1:
   ```
   host
   └── jaild-jail
       ├── portcullisd-jail (Capsicum'd; holds N procdesc fds)
       ├── frescod
       ├── atrium-devevents
       └── vestibulum (seat0)   ← waiting for user
   ```

## Phase 2: user authenticates

6. vestibulum draws its login screen by emitting Fresco scene
   commands over its aqueduct connection to frescod. Receives
   keyboard events on the same connection (frescod → focused
   client, which is vestibulum since no other clients exist on
   seat0 yet).

7. User types username + password. vestibulum validates against
   `/etc/master.passwd` using `crypt(3)` directly. (PAM as a
   later refinement; the v1 path is just FreeBSD's standard
   passwd database. Same approach as a minimal getty.)

   On failure: clear the password buffer, redraw, return to step
   6. No retry counter at this layer — that's a portcullisd-side
   policy concern (rate-limit per seat).

   On success: vestibulum has `uid_N`. The plaintext password is
   zeroed and freed; vestibulum never re-uses it or transmits it.

8. vestibulum sends portcullisd over its admin aqueduct socket:

   ```toml
   [session_start]
   seat   = "seat0"
   user   = 1001              # uid, not username — pre-resolved by vestibulum
   token  = "<32 random bytes>"   # nonce; defends against replayed messages
   ```

   The aqueduct socket is unix-domain; portcullisd validates the
   peer credentials via `getpeereid(3)`. On v1, only vestibulum's
   uid (root) is allowed to send `session_start`. Future: peer's
   PID is in the vestibulum jail, additional check.

9. portcullisd:
   - validates: user 1001 exists in `/etc/master.passwd` (ro
     mount), no concurrent session for `(seat0, 1001)` in
     `/var/run/atrium/sessions/`, vestibulum's claim of seat0
     matches portcullisd's seat assignment.
   - reads `/etc/atrium/services.d/atrium-supervisor.toml` (already
     parsed at startup; reusing in-memory copy).
   - constructs a per-user spec:
     ```
     name           = "user-1001-seat0"
     path           = "/usr/home/<username>"
     uid            = 1001
     children.max   = 64                  # for app jails
     devfs_ruleset  = "atrium-baseline"
     ip4            = disable             # default; supervisor caps may relax
     mounts:
       ro: /usr/local/lib, /usr/local/share/atrium, /usr/local/etc/fonts
       rw: /usr/home/<username>, /var/db/atrium/users/<username>,
           /var/run/aqueduct/frescod (the socket)
     exec_path      = "/usr/local/bin/atrium-supervisor"
     argv           = ["atrium-supervisor", "--seat", "seat0", "--sid", <sid>]
     env_allowlist  = ["USER", "HOME", "LOGNAME", "PATH", "TERM", "LANG", "TZ"]
     ```
   - sends to jaild:
     ```toml
     [create_jail]
     spec = ...
     exec_spec = ...
     ```

10. jaild:
    - validates the spec against `/etc/atrium/jaild.policy.toml`:
      - all mount sources are in the policy's `[mount_sources]`
        allowlist.
      - devfs_ruleset is in `[devfs_rulesets].allowed`.
      - exec_path matches `[exec_paths].allowed_prefixes`.
      - uid is in `[uid].min_user_uid..max_user_uid`.
      - children.max ≤ `[children_max].max`.
    - on policy violation: returns `EPERM` with a structured
      reason. portcullisd logs and surfaces to vestibulum.
    - `pdfork()`. In child:
      - calls `jail_set(JAIL_CREATE | JAIL_ATTACH)` with the spec.
      - `setuid(N)`, `setgid(N's group)`, drop supplementary groups
        per `getgrouplist(3)`.
      - `execve(exec_path, argv, env)`.
    - parent: sends procdesc fd + jid to portcullisd via SCM_RIGHTS.

11. portcullisd:
    - records session in `/var/run/atrium/sessions/<sid>.toml`:
      ```toml
      sid           = "..."
      seat          = "seat0"
      user          = 1001
      jid           = 7
      started_at    = "2026-05-07T12:34:56Z"
      supervisor_pid = 12345           # informational; pdfd is the real handle
      ```
    - registers an `EVFILT_PROCDESC` kevent on the supervisor's
      pdfd, watching for `NOTE_EXIT`.
    - sends vestibulum: `{ kind: "session_started", sid: <sid> }`.

12. vestibulum:
    - on receiving `session_started`: tears down its UI scene
      (sends frescod commands to clear vestibulum's window), exits
      cleanly. Its jail evaporates (no `persist`).
    - portcullisd's procdesc kqueue for vestibulum fires
      `NOTE_EXIT` → portcullisd cleans up vestibulum's session
      record. (Vestibulum is restarted only at logout, not now —
      the seat has an active session.)

## Phase 3: user session active

13. atrium-supervisor (uid=1001, in jail `user-1001-seat0`):
    - connects to frescod via the aqueduct socket mounted into its
      jail.
    - sends `{ kind: "session_attach", sid: <sid> }` so frescod
      transitions input routing for seat0 from "vestibulum (now
      gone)" to "this supervisor connection."
    - reads `~/.atrium/startup.toml`.
    - for each startup app: sends portcullisd a `launch_app`
      request:
      ```toml
      [launch_app]
      app_id      = "atrium-edit"
      sid         = "<this session's sid>"
      argv        = ["atrium-edit", ...]
      ```
    - portcullisd validates (sid exists, peer cred matches the
      session's supervisor uid, the requested app's manifest is
      readable).
    - portcullisd → jaild → pdfork → app jail. App jail is a
      grandchild under jaild-jail; portcullisd records the
      app's procdesc fd associated with the session.

14. While running, the supervisor can ask portcullisd to:
    - launch more apps
    - terminate apps (`pdkill` on procdesc fd)
    - lock the session (replace supervisor's window with a
      lock-screen subprocess; passwords go through the same
      crypt(3) path as vestibulum used)

## Phase 4: logout

15. User triggers logout (Forum menu / supervisor command). The
    supervisor:
    - asks portcullisd to terminate every app in the session
      (portcullisd `pdkill`s each procdesc fd, kqueues for
      EOF, then asks jaild to remove each empty jail).
    - flushes any session state to `/var/db/atrium/users/<N>/`.
    - exits 0.

16. portcullisd's EVFILT_PROCDESC for the supervisor fires
    NOTE_EXIT:
    - asks jaild to `jail_remove` the supervisor's jail (and any
      stragglers — children of the supervisor jail are
      auto-removed by the kernel).
    - writes the session log to `/var/log/atrium/sessions/<sid>.log`.
    - removes `/var/run/atrium/sessions/<sid>.toml`.
    - asks jaild to start a fresh vestibulum on seat0
      (back to phase 2, step 5/6).

## Phase 5: crash paths

| Crash | Detector | Recovery |
|-------|----------|----------|
| app crashes | portcullisd procdesc EOF | jail_remove app jail; the supervisor sees an aqueduct disconnect on its frescod connection-channel for that app; supervisor decides whether to restart |
| atrium-supervisor crashes | portcullisd procdesc EOF | identical to graceful logout (phase 4 step 16) |
| vestibulum crashes pre-auth | portcullisd procdesc EOF | `jaild` re-launches vestibulum on the affected seat |
| frescod crashes | portcullisd procdesc EOF | jaild re-launches frescod; all clients on aqueduct see a disconnect, are expected to reconnect (existing protocol) |
| atrium-devevents crashes | portcullisd procdesc EOF | jaild re-launches; frescod loses keyboard/mouse for the gap (expect <1 s) |
| portcullisd crashes | rc.d service supervision (on host, outside any jail) | rc restarts; new portcullisd reloads `/var/run/atrium/sessions/`, queries jaild for the procdesc fds it lost, **fails over with stale state if jaild's state file is missing** — see jaild crash row |
| jaild crashes | rc.d service supervision | rc restarts. New jaild reloads policy, reads its persistent state file at `/var/run/atrium/jaild.state.toml` (atomically replaced on every state change), reconciles with kernel jail list via the `jail_get` sysctl loop. Jails it can match by name + cred: claim. Jails not matching: leave alone (third-party). procdesc fds **are lost** (they were owned by the dead jaild process) — portcullisd has to give up on EOF for those and use `kill(pid_from_state, 0)` to liveness-check. Fragile; document. |
| host kernel panic | n/a | reboot; cold start; phase 1 from scratch |

The portcullisd-crash + jaild-crash rows are the only ones with
ugly recovery semantics. Mitigation: keep both daemons small,
audit them more aggressively than apps, run them under
service-supervisor with crashlog capture. Long-term: investigate
KCOV / fuzzing both daemons.

## Vestibulum's auth surface

Treating vestibulum as a "small audited login screen" is reasonable
only if vestibulum stays small. Constraints:

- vestibulum binary is statically linked (or at least: links only
  against `libc`, `libcrypt`, `libaqueduct` — all of which are in
  the read-only mount).
- vestibulum's input parser is the smallest possible thing that
  reads two strings (username, password) from frescod-relayed key
  events. No fancy editing modes, no clipboard integration, no
  rich-text input.
- Password buffer is `mlock(2)`'d to prevent swap, zeroed
  immediately after `crypt(3)`.
- vestibulum *never* echoes the password to the wire. The
  password buffer never crosses the aqueduct socket. Only the
  resulting auth assertion goes to portcullisd.
- vestibulum cannot exec other binaries (its jail's exec
  allowlist is empty post-startup).

If those constraints hold, the worst-case vestibulum compromise
is "attacker can run vestibulum's logic with vestibulum's
filesystem view", which gets them: read access to
`/etc/master.passwd` (already public hashes), connection to
frescod (can keylog its own jail-attached events), and the
ability to send a forged `session_start` to portcullisd. The
last is the dangerous one — it would let an attacker start a
session as any user without knowing their password.

Defence-in-depth options:

- **A. portcullisd ratifies the auth.** Vestibulum doesn't
  actually authenticate — it forwards the credential pair to
  portcullisd, which authenticates and then starts the session.
  Smaller vestibulum, but portcullisd now needs `/etc/master.passwd`
  (which it would get via mount). Mostly moves the trust around.

- **B. Separate `atrium-authd`.** Tiny daemon owns
  `/etc/master.passwd` access, exposes a `verify(user, pass) →
  one-shot-token` socket. Vestibulum forwards credentials, gets a
  token, sends token to portcullisd. portcullisd verifies token
  with authd. Very OpenSSH-shaped. Best security; one more
  daemon. **Recommended for v1.5; deferred for v1 simplicity.**

- **C. PAM with privsep.** PAM modules can be heavy and history-
  CVE-prone, but FreeBSD's PAM is reasonable. Run a small
  PAM-using auth process; vestibulum proxies through it. Same
  shape as B with PAM modules under the hood.

For v1: vestibulum does crypt(3) directly, sends portcullisd a
nonce-bearing `session_start` message. **For v1.5 (parallel with
post-D2 hardening), split out atrium-authd per option B.**
Deferred-but-tracked.

## Aqueduct service surface added by this design

| Service socket | Protocol | Peer cred check |
|----------------|----------|-----------------|
| `/var/run/atrium/jaild.sock` | jaild RPC | uid=root only (parent process) |
| `/var/run/aqueduct/portcullisd-admin.sock` | session_start, launch_app, terminate_app | per-op uid policy |
| `/var/run/aqueduct/portcullisd-user-<uid>.sock` | per-user RPC mounted only into that user's supervisor jail | uid match |

These three sockets are the new surface area. Each gets a precise
schema in `docs/spec/aqueduct-services.md` (separate doc).

## Boot order summary

```
init → rc.d/atrium-jaild              # the only thing rc.d knows about us
        └── jaild
             └── (on first request from rc.d/atrium-portcullisd-bootstrap)
                  └── portcullisd
                       └── (asks jaild to start each system service)
                            ├── frescod
                            ├── atrium-devevents
                            └── vestibulum × N seats
```

Equivalent textual sequence: rc → jaild → portcullisd → frescod /
devevents / vestibulum. All but jaild and portcullisd-bootstrap
are children of jaild-jail in the kernel jail tree.
