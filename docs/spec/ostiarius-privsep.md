# Ostiarius privsep — de-rooting the session launcher

Status: DESIGN (2026-06-25). Implements the last step of the §9 TCB model
(docs/spec/portcullis.md): *only* the irreducibly-privileged core runs as
root; every other component is jailed and non-root.

## 1. The gap

After frescod and the session apps (vestibulum, Forum chrome) were moved to
jailed, non-root execution, the processes still running unjailed as root are:

| Process    | Root? | Justified TCB?                                            |
|------------|-------|----------------------------------------------------------|
| jaild      | yes   | YES — the sole `jail_set(2)` caller; irreducible.         |
| portcullisd| yes   | YES — the capability broker; reaches jaild's root socket. |
| bootstrap  | yes   | YES — the supervisor; pdforks + holds procdescs.          |
| **ostiarius** | yes | **NO — re-architectable.** This spec closes it.         |

`ostiarius` is root for exactly two reasons, and only two:

1. **jaild access.** Its `JaildLauncher` connects directly to
   `/var/run/atrium/jaild.sock` (mode 0600, root-only) to create the session
   jails (vestibulum at boot; the human's Forum session on login).
2. **Authentication.** `authenticate()` today runs a dev stub (any non-empty
   credential, **no privilege**); the production PAM path needs root to read
   shadow hashes.

The naive fix — grant `_ostiarius` access to jaild's socket — is **not** a
de-rooting: anything that can call jaild can create *arbitrary* jails, which is
escape-equivalent. Real de-rooting requires that ostiarius's privileged
operations be **brokered through a capability check**, exactly like the memory
governors' `GovernReap`/`GovernSetRctl`. The broker is portcullisd (already
root, already the mediator).

## 2. Design

ostiarius becomes a **jailed, non-root `_ostiarius` service** that obtains its
two privileges as portcullisd capabilities. It keeps all *policy* (session
composition, the seat, the vestibulum↔login protocol, lifecycle); portcullisd
performs the privileged *mechanism* after a cap-check.

### 2.1 Capability: `LaunchSessionComponent`

ostiarius does not build jail specs that jaild blindly executes. Instead, the
session components are **declared** (a session-component registry, the same
shape as `services.d` manifests but session-scoped — e.g.
`/etc/atrium/session.d/{vestibulum,forum-wm,forum-bar,forum-dock,choragus}.toml`).
Each declares the component's exec path, caps, mounts envelope, and devfs
ruleset — the trusted, operator-curated definition.

ostiarius (as `_ostiarius`) sends portcullisd:

    LaunchSessionComponent { component_id, owner_name }

portcullisd:
1. Confirms the peer is `_ostiarius` (getpeereid) and holds the
   `session_launch` grant (its services.d manifest capability).
2. Looks up `component_id` in the session-component registry. **Unknown id →
   refused.** This is what bounds `_ostiarius`: it can launch only the declared
   session set, never an arbitrary jail.
3. **Allocates + registers** the per-session uid itself — `owner_name` is the
   only ostiarius-supplied parameter. `portcullis_peer::allocate` picks a free
   uid from the 50000+ range AND writes the launch registry
   (`/var/run/atrium/app-registry`, root-owned), so the registry write stays in
   the TCB and a jailed `_ostiarius` never touches it. (vestibulum uses the
   reserved `_login` identity.) The allocated uid replaces the registry spec's
   placeholder `exec.uid`.
4. Forwards the fully-formed `CreateJail` to jaild and returns the pid/jid to
   ostiarius, holding the procdesc (below).

Note: the step-3 implementation (commit 5dccaea) currently takes `owner_uid`
from ostiarius — the move to portcullisd-side allocate+register is the small
refinement step 4 makes when ostiarius is switched onto the broker.

jaild returns the session jail's procdesc fd to portcullisd (`Client::send` ->
`(Response, Option<fd>)`). **portcullisd holds that fd in daemon state**, keyed
by jail name (it is the long-lived root TCB daemon, already `Arc<Mutex>`-stated)
— so the procdesc, and thus the session jail's lifetime, lives in the TCB, not
in ostiarius. A `TeardownSessionComponent { jail_name }` verb (logout, or the
component exiting) closes the fd, killing the persist=0 jail, and is gated on the
same `session_launch` grant. This preserves the die-with-holder safety inside the
TCB and means a crashed/compromised `_ostiarius` can neither leak nor silently
keep session jails alive.

### 2.2 Capability: `VerifyCredential`

ostiarius forwards the credential it received from vestibulum:

    VerifyCredential { user, password }  ->  Ok(user) | Err

portcullisd performs the check (PAM, or the dev stub) — it is root and can read
shadow / drive PAM — and returns only a yes/no plus the canonical username.
**The shadow hashes never leave portcullisd; `_ostiarius` never reads them.**

Rationale for putting this in portcullisd rather than a dedicated auth helper:
portcullisd is already the root mediator, so this adds no new root process and
no privilege it doesn't already have, and the OpenSSH "isolate the complex
pre-auth parser" argument doesn't apply (the input is a clean pair from an
identified peer, not a network protocol). **Revisit trigger:** if the PAM stack
ever loads complex/third-party modules, move *only the PAM call* into a minimal
helper to bound the blast radius of a buggy module — the protocol above is
unchanged, the implementation of the verify step moves.

### 2.3 ostiarius jailed

- A `_ostiarius` system uid (e.g. 50093).
- A `40-... `-style services.d manifest (started by bootstrap, before the
  session): real root, `_ostiarius`, rootfs mounts, the portcullisd cap socket
  (`/atrium/sockets/portcullis.sock`) and its own control socket dir under
  `/atrium/sockets/ostiarius/` (so jailed vestibulum keeps reaching it — already
  moved there, commit 41e8c16).
- No jaild socket, no `/etc/master.passwd`, no devfs beyond basics.

## 3. Security analysis

A compromised `_ostiarius` can:
- Request launches of the **declared session components only** (registry-bounded)
  — not arbitrary jails. It cannot escalate to jail-creation primitives.
- Request credential verification — i.e. act as a **brute-force oracle** against
  portcullisd's verify. Mitigation: portcullisd rate-limits / delays failed
  verifies (same as any login surface); this oracle already exists implicitly at
  the login UI and is not widened.
- It **cannot** read shadow hashes, call jaild, see the host filesystem, or
  touch another user's session (structural — per §9 each session is a distinct
  jail it can only ask the broker to create).

Net: ostiarius drops from "full root, direct jaild" to "non-root, two bounded
broker capabilities." The root TCB shrinks to **jaild + portcullisd +
bootstrap** — the three genuinely-irreducible components.

## 4. Migration

1. portcullis-ipc: add `Request::LaunchSessionComponent` + `Request::VerifyCredential`
   and their responses.
2. portcullisd: `handle_launch_session_component` (registry lookup + param fill +
   forward to jaild) and `handle_verify_credential` (PAM/stub); gate both on the
   peer holding `session_launch` (services.d capability), mirroring `memory_govern`.
3. Session-component registry loader (`/etc/atrium/session.d/`), reusing the
   services.d manifest parser.
4. ostiarius: replace `JaildLauncher`'s direct jaild calls with broker calls;
   replace `authenticate()`'s privileged path with `VerifyCredential`.
5. `_ostiarius` uid + the jailed services.d manifest; bootstrap launches it;
   disable the root `atrium-ostiarius` rc.d.
6. Verify: login path end-to-end with ostiarius non-root; confirm `_ostiarius`
   cannot create a non-registered jail (negative test).

Implement in this order; steps 1–3 are additive (no behaviour change until
ostiarius is switched in step 4), so the risky cutover (5) lands last on a
snapshot.
