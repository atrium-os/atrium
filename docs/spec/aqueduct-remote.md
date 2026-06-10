# Aqueduct remote sessions — auth handoff, session capabilities

> Status: design settled 2026-06-10 (architecture review thread #5).
> Normative for any aqueduct service accepting non-`SO_PEERCRED`
> transports. Companions: aqueduct.md §7.2 (tunnel posture),
> stoa.md §2 (the pattern's first instance), transport.md (Fresco
> remote-desktop claims this design backs).

## 0. Position

Aqueduct's local identity is `SO_PEERCRED`, which does not exist
across a network; transport.md's remote-desktop story previously
rested on hand-waved "mutual TLS." Meanwhile stoa.md §2 contains a
complete, worked remote-auth design. This spec promotes that design
from "how stoad works" to "how any aqueduct service accepts remote
clients," and adds the one concept Stoa didn't need: a **session
capability set**.

Principles, in the platform's own idiom:

- **No new credentials, no new PKI.** Authentication rides sshd +
  `~/.ssh/authorized_keys` — 25 years of hardened userauth, the way
  Portcullis rides 25 years of jails.
- **No new daemon.** The service that accepts remote sessions mints
  and validates its own tokens (service-is-policy, as everywhere in
  aqueduct). There is no central session broker in the TCB.
- **Capability, not trust level.** A session token enumerates
  powers, default-deny style, exactly like an `atrium.toml`
  manifest. "Less trusted endpoint" is expressed as a
  user-initiated capability downgrade, not a system-imposed tier.

## 1. Threat model and the no-haircut rule

What is authenticated is **the user** (their SSH key). A holder of
that key can open a shell and exfiltrate anything; therefore a
*system-imposed* trust haircut on remote sessions defends nothing —
the attacker would use the shell instead. Hence:

> **Default rule: a fully-authenticated remote session is the
> user's local session trust domain.** Same capability grants
> (`policy.toml`), same possession-ledger domain (aqueduct §6.6),
> never *more* trusted than a local jailed app.

What genuinely wants less trust is not the user but a **weak
endpoint** — kiosk reattach, borrowed laptop, phone client, sharing
one window into a meeting. There the authentic user *chooses* to
wield a weaker credential: a restricted token (§4). That is the
powerbox consent pattern (cf. Scrinium), not a trust framework.

In scope: network MITM (defeated by the tunnel + key anchoring),
token theft in transit (token never leaves the SSH channel in the
clear), post-handoff transport hijack (tunnel integrity, or MAC'd
envelope on the raw-datagram profile), replay (tunnel, or sliding
window). Out of scope: a compromised client endpoint sees whatever
the session legitimately displays — inherent to remote display;
bound the blast radius with restricted tokens.

## 2. The handoff (generalized from stoa.md §2)

```
client: ssh -T user@host aqueduct-shell mint fresco [--view-only ...]
sshd:   verify authorized_keys → fork aqueduct-shell as the user
aqueduct-shell (runs at user rank, no privileges of its own):
  1. open the target service's local socket
     (reachable iff the user's session may reach it)
  2. service authenticates the request via SO_PEERCRED
  3. request: target classes + requested caps + requested TTL
  4. service mints a session record; key is derived, not chosen:
        K_sess = KDF(ssh_session_id ‖ service_nonce)
     (ssh_session_id per RFC 4253, exported by sshd to the child)
  5. write {connect_addr, transport, session_id, K_sess, caps, expiry}
     to stdout — inside the SSH channel, never in cleartext on a wire
  6. exit 0
client: read params from ssh stdout; connect on the offered
        transport; OP_AQUEDUCT_HANDSHAKE presents the session
```

Properties inherited from Stoa's instance: existing keys and agents
Just Work; `K_sess` is anchored in the SSH handshake so a
post-handoff MITM cannot forge a session without breaking SSH; the
SSH connection may drop immediately after handoff; re-mint after
expiry touches the credential, never the underlying session state.

`aqueduct-shell` is deliberately boring: a user-rank binary that
speaks the mint request over a local socket. The trust anchors are
sshd and the target service; compromising `aqueduct-shell` gains
only what the invoking user already had.

## 3. Wire: `OP_AQUEDUCT_HANDSHAKE` (class 0)

Two new built-in opcodes (aqueduct.md §3.3 table):

| op   | name                | direction | purpose |
|------|---------------------|-----------|---------|
| 0x09 | SESSION_HANDSHAKE   | client→server | first message on any non-SO_PEERCRED transport |
| 0x0A | SESSION_ACCEPT      | server→client | negotiated caps + ledger scope; non-zero status = rejected |

`SESSION_HANDSHAKE` payload: `auth_method` tag + method body.

- `AuthMethod::SshHandoff` (v1): `session_id` + proof of `K_sess`
  possession — an HMAC over a server-supplied nonce on the
  tunneled profile (the bearer secret itself never crosses, even
  inside the tunnel); implicit per-message MAC on the raw-datagram
  profile (§5).
- `AuthMethod::CapToken` (deferred): portcullisd-signed token for
  machine-federation / fleet shapes.
- `AuthMethod::Mutual` (deferred): mTLS identity mapped to a local
  user via an operator policy table.

A service receiving any other opcode first on a remote transport
drops the connection. Local `SO_PEERCRED` transports are unchanged
— no handshake, exactly as today.

## 4. Session capabilities

The mint carries a `caps` set, same vocabulary and default-deny
posture as the app manifest:

```toml
# inside the session record / SESSION_ACCEPT
[caps]
input    = "full"          # or "none" — view-only session
scope    = "session"       # or "app:org.atrium.edit" / "window:<id>"
clipboard = true            # existing capability names as services need them
```

- **Default mint** (plain `aqueduct-shell mint <service>`): full
  user session. Per §1, restricting the default is theater.
- **v1 ships two restrictions** to exercise the machinery
  end-to-end: `input = "none"` and `scope = "app:<id>"`. Further
  vocabulary is added per-service, not invented here.
- **Enforcement is the existing per-principal machinery**: frescod
  and the services already gate per-jail capabilities; a remote
  session is one more principal whose cap set arrives at handshake
  instead of at jail build. A remote Fresco session's slots are
  treated at jailed-app trust regardless of caps.
- **Ledger rule** (composes with aqueduct §6.6): a full-capability
  session's possession ledger is widened to the user's session
  trust domain (the remote-desktop bandwidth story — one upload
  per session — survives intact). A *restricted* session stays
  per-connection: it accrues only what the server actually sent
  it, and dedup negotiation reveals nothing beyond that.
- A `window:<id>`-scoped session is remote Limen in embryo —
  role-typed single-surface sharing across the network. When
  screen-share lands, it is a token mint, not a new subsystem.

Mid-session capability *upgrades* are out of scope: reconnect with
a new token (cheap — recovery and reattach already make reconnect
a first-class path, fresco-recovery.md §3).

## 5. Integrity profiles

| Profile | Who | Mechanism |
|---|---|---|
| **Tunneled** (default) | Fresco remote, thin-client compositor, everything unless stated | Aqueduct plaintext inside SSH channel / WireGuard / QUIC+TLS (aqueduct.md §7.2). Token proof at handshake only; the tunnel carries confidentiality, integrity, replay. No per-message crypto in aqueduct. |
| **Raw-datagram** (opt-in) | Stoa only, in v1 | Stoa's existing envelope: `ver|type|seq|payload|MAC[16]` (truncated HMAC-SHA-256 keyed by `K_sess`), sliding anti-replay window (stoa.md §3). For services that deliberately trade the tunnel for UDP latency. |

A service declares its profile(s); clients cannot negotiate a
tunnel-less connection to a tunneled-only service.

## 6. Revocation and lifetime

- Tokens carry `expiry` (service default; Stoa keeps its 7-day
  reattach key; Fresco default: session-lifetime, capped at 7
  days). Expiry invalidates the credential, never the session
  state — re-mint and reattach.
- The minting service keeps its session table locally;
  `aqueduct-shell list|revoke` (and later a Forum/Curia panel)
  enumerates and kills live remote sessions. Revocation closes the
  transport; state follows the service's own rules (Stoa sessions
  persist; a Fresco remote view just ends).
- Token records never store `K_sess` in the clear at rest —
  service stores `H(K_sess)` and verifies proofs against it.

## 7. Deferred (declared, not designed)

- `CapToken` machine federation (thin-client fleets; portcullisd
  cross-signing).
- `Mutual` mTLS identity mapping.
- QR-code / pairing UX for credential issuance to weak endpoints —
  an issuance problem; it plugs into the `caps` field when it
  arrives.
- Per-device trust memory ("this kiosk always gets view-only").
- Mid-session cap changes (see §4).

## 8. Open questions

- Should `aqueduct-shell mint` for a *restricted* token require no
  prompt (the user typed the restriction) while a *full* token
  minted from an already-remote session (chained remoting)
  requires step-up consent? Lean yes — chaining is where theft
  cascades.
- KDF concrete choice (HKDF-SHA-256 with labeled info string) and
  the export path for `ssh_session_id` on FreeBSD's sshd — verify
  the `SSH_SESSION_ID`-equivalent plumbing before S0 of the
  implementing milestone.
- Does Vestibulum's seat login mint a session token too (unifying
  "local seat" and "remote session" as the same principal shape)?
  Attractive; revisit at D2 integration.
