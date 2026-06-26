# Stoa — persistent session service

Status: **S0–S2 + jail-login + S3a BUILT & verified live** (D2.7). S3a
(history-only persistence — sessions, layout, cwd, and scrollback survive a
`stoad` restart + host reboot by respawning shells) is host-verified; the
production `/atrium-data` volume is declared but **VM-verification is parked**
on two storage-subsystem prerequisites — the volumes allocator isn't
commissioned on the dev VM (bootstrap runs with an empty `--volumes-socket`),
and Tessera `size_max` quota isn't yet enforced (so the scrollback-bloat
backstop is currently stoad's own retention only). Resume after the Tessera
quota kmod + volumes commissioning. S3b (live-process survival via the broker,
§5.5), ack-based retransmit, and multi-client are aspirational TODOs.
Last updated: 2026-06-26.

> **Implementation status (2026-06-25).** Built in `stoa/` (macOS-first
> behind a `ShellSpawner` seam): the wire protocol (`stoa-proto`: MAC'd
> envelope + anti-replay), the datagram transport (`stoa-net`), and
> `stoad`/`stoactl` with a per-session **mint handshake** (each session its
> own UDP port + random `K_sess`) and a session table — so the **shell
> survives client disconnect** (proven). **Jail-login (§4.5)** is built and
> **verified live in the VM**: `stoactl attach --jail <jail>` runs a shell
> *inside* a running jail through jaild `ExecInJail` → the portcullisd
> `jail_exec`-gated broker → `BrokerSpawner`, with the shell as the jail's
> non-root app-uid (`cap.jail_exec.denied` for an unauthorized caller).
> **stoad itself runs jailed + non-root** (`_stoad`, own root, `ip4=inherit`
> — the gated network model for a network-facing daemon; see §1). Pending:
> the real SSH-anchored handshake (§2 — today the mint key rides the SSH
> channel mosh-style, not yet KDF'd from the SSH session id) and
> S3 Tessera scrollback (§5 — survives client disconnect, not yet `stoad`
> restart).
>
> **S2 multiplexer BUILT & verified live (2026-06-25).** Multi-window
> (`Ctrl-B c`/`n`/`p`/`l`/`0`-`9`) with per-window titles; **panes**
> (`Ctrl-B %`/`"` split, `Ctrl-B o` switch — v1 is a flat 2-pane split per
> window, not yet the full §4 layout tree); in-memory scrollback
> (`Ctrl-B [`/`]`); the clean-room SSP **predictor** (§3.4, opt-in
> `$STOA_PREDICT`); and **grid-sync streaming** (§3.3, opt-in `$STOA_SYNC`):
> OUTPUT carries an encoded `StateDiff` (compact cell-run codec) the client
> paints with `render_diff`, instead of raw bytes. It is **self-healing on a
> flaky link** — a corrupt datagram (decode fails) or a lost/reordered one
> (seq gap / non-advancing seq) triggers a resync, where raw bytes
> permanently desync. Proven on macOS *and* FreeBSD with injected loss +
> reorder (1/3 each): the screen always converges. **Deltas from this spec's
> design:** the diff is a flat cell-run format (not the per-pane §3.3 struct);
> recovery is **resync-on-gap (full repaint)**, not the ack-based
> last-acked diffing of §3.3/§3.4 (a future bandwidth refinement); sync and
> the byte-stream predictor are mutually exclusive today.
>
> **Jailed `SessionJail` handling (fixed 2026-06-25, verified
> live):** a jailed stoad must not `forkpty` a session shell in its *own*
> jail. `resolve_session_target` now: routes `SessionJail` through the
> broker into `$STOA_SESSION_JAIL` if set; else, if stoad is itself jailed
> (`security.jail.jailed`), **refuses** with a clear message (use `--jail`,
> or configure a session jail); else (dev/unjailed) `DirectSpawner` as the
> user. `$STOA_SESSION_JAIL` is one operator-scoped jail for now — the seam
> generalizes to a per-user session jail when the session model wires it.

The piece of Atrium that turns "ssh in and run tmux" into a single
coherent service. **Stoa owns long-lived shell sessions on the
host**; clients (terminal or graphical) attach, render, and detach.
Network drops, device roams, daemon restarts, and host reboots all
preserve the session's *conversation history*; only live process
state dies on host reboot.

A **stoa** in classical Greek architecture is a covered colonnade —
a public gathering place where people came and went, but the
colonnade itself stood for centuries. The metaphor is exact:
clients are visitors, the daemon is the colonnade.

## 0. The problem and the inversion

Every existing solution splits "persistent remote shell" across two
layers with an awkward seam:

| Tool         | Owns transport          | Owns multiplexer | Persists scrollback | Multi-client | Roams |
|--------------|-------------------------|------------------|---------------------|--------------|-------|
| ssh + tmux   | ✗ (TCP, dies on drop)  | ✓                | in-memory only      | manual       | ✗     |
| mosh         | ✓ (SSP/UDP)            | ✗                | client-side, lost   | ✗            | ✓     |
| mosh + tmux  | partial                | ✓                | in-memory only      | manual       | partial |
| Eternal Term | ✓ (TCP+resync)         | ✗                | ✗                   | ✗            | partial |

The split produces seam bugs: mosh+tmux can't predict pane switches
because mosh doesn't know they're pane switches; tmux's scrollback
vanishes on a daemon kill because it never touches disk; reattaching
from a second device while the first is still attached "works" but
isn't a designed-for property.

Stoa's inversion: the SSH connection is **not** primary. SSH is a
one-time auth handshake that hands the client a UDP capability;
everything after is direct between client and daemon. The session
lives on the host, owned by `stoad`, until the user destroys it.

## 1. Architecture

```
   ┌──────────────────┐  one-time SSH userauth   ┌──────────────────┐
   │  stoactl client  │ ────────────────────────►│ sshd → stoa-shell │
   │   (terminal /    │                           │   (auth handoff)  │
   │    fresco GUI)   │ ◄── UDP capability ───────│   exits after     │
   └──────────────────┘                           └──────────────────┘
            │
            │   UDP, MAC'd, sequence-numbered state diffs
            │   (or UDP-over-TCP fallback through SSH if NAT'd)
            ▼
   ┌──────────────────────────────────────────────────────────────┐
   │                            stoad                              │
   │                                                                │
   │  per-user session table:                                       │
   │    session 'work-build'  →  windows[3], layout, env, cwd       │
   │    session 'irc'         →  windows[1]                         │
   │    session 'atrium-vm'   →  windows[2]                         │
   │                                                                │
   │  for each window: pty(4) ──► shell process (in user's          │
   │                                  Portcullis session jail)      │
   │                                                                │
   │  scrollback ring tail (per window) ──► WAL ──► Tessera CAS    │
   │                                                                │
   │  attached clients[N] ──► state-diff fanout                     │
   └──────────────────────────────────────────────────────────────┘
```

Three binaries:

- **`stoad`** — system-wide daemon (one per host, like sshd).
  Owns sockets, ptys, multiplexer state, scrollback, MAC keys.
  Runs **jailed + non-root** (a dedicated `_stoad` uid, like
  `_frescod`/`_ostiarius`); it never calls `jail_set`/`jail_attach`
  itself. Shells are spawned through the **portcullisd broker**
  (§4.5, §11) — the sole path to `jaild` after the TCB de-rooting —
  behind a `ShellSpawner` seam so a dev/macOS build can spawn
  directly, no jail, for testing (§11.1).
- **`stoactl`** — the client. CLI (`stoactl attach work-build`) +
  TUI rendering. Phase 4 adds a Fresco-protocol GUI variant that
  renders into Atrium's compositor.
- **`stoa-shell`** — a tiny helper invoked by sshd as the user's
  login shell when connecting via Stoa. Negotiates a UDP session
  with `stoad`, hands the client the capability over the SSH
  channel, then exits. The SSH connection is free to drop.

## 2. Auth handoff

> 2026-06-10: this handoff is now the platform-wide pattern —
> generalized in [aqueduct-remote.md](aqueduct-remote.md)
> (`aqueduct-shell`, `OP_AQUEDUCT_HANDSHAKE`, session capability
> sets). Stoa is its first instance and, in v1, the only user of
> the raw-datagram (MAC'd UDP) integrity profile. `stoa-shell` may
> become a thin wrapper over `aqueduct-shell` at implementation
> time; the wire shape below is unchanged either way.

Stoa **does not invent new credentials**. It rides FreeBSD's `sshd`
for userauth and key management:

```
client: ssh -T user@host stoa-shell attach work-build
sshd:   verify user's authorized_keys → fork stoa-shell as user
stoa-shell:
  1. open UNIX socket to stoad
  2. authenticate to stoad via SO_PEERCRED (uid trusted from sshd)
  3. ask stoad to allocate a UDP port + per-session MAC key
  4. derive MAC key from SSH session id (RFC 4253) ⊕ stoad's nonce
  5. write {host_addr, udp_port, mac_key, session_id} to stdout
  6. exit 0
client: read those params from ssh's stdout, switch to UDP
```

Properties:

- Existing `~/.ssh/authorized_keys` and ssh agents Just Work.
- The MAC key is anchored in the SSH handshake's session id, so
  an attacker who can MITM the UDP path post-handoff still cannot
  forge inputs without breaking the SSH handshake.
- After handoff, the SSH connection can drop immediately. The
  daemon doesn't care.
- Re-handshake required after MAC key expiry (default 7 days);
  the underlying session is untouched.

## 3. Wire protocol

Two channels, both over the same UDP socket pair:

### 3.1 Datagram envelope

```
┌──────┬──────┬──────┬──────────────┬──────────┐
│ ver  │ type │ seq  │   payload    │ MAC[16]  │
│ u8   │ u8   │ u32  │   variable   │  HMAC    │
└──────┴──────┴──────┴──────────────┴──────────┘
```

- `ver = 1` for v1.
- `type ∈ {INPUT, STATE_DIFF, ACK, CONTROL, KEEPALIVE}`.
- `seq` — monotonic per direction; lets us detect drops and
  reorder. The session is not connection-oriented in the kernel
  sense; reattach uses the same MAC key with a fresh seq base.
- `MAC` covers ver+type+seq+payload; truncated HMAC-SHA-256.
- Anti-replay: receiver tracks (last_seq, sliding window of N=128
  past seqs). Out-of-window or duplicate-in-window → drop silently.

### 3.2 Input typing (the multiplexer-aware predictor)

The crucial bit that mosh+tmux gets wrong: input has **typed
disposition**, not raw bytes.

```rust
enum Input {
    PtyBytes { window_id: u32, bytes: Vec<u8> },   // → pty
    DaemonCmd(DaemonCmd),                          // → stoad
    Resize { window_id: u32, cols: u16, rows: u16 },
}

enum DaemonCmd {
    SwitchWindow(u32),
    SplitPane { dir: Horiz | Vert },
    ClosePane(u32),
    Detach,
    Scrollback { window_id: u32, op: ScrollOp },
    Push { local_path, remote_path },
    Pull { remote_path, local_path },
    ClipboardWrite(Vec<u8>),
    ClipboardRead,
    ListSessions,
    NewSession { name: String, cmd: Option<String> },
    Kill { session: String },
    // ...
}
```

The client decides: keystrokes inside a pane → `PtyBytes`. The
configured prefix (default `Ctrl-B`) followed by a known mapping →
`DaemonCmd`. Predictive echo applies *only* to `PtyBytes`. Pressing
`Ctrl-B 1` produces zero local echo; the client awaits the server's
state diff for the new focused window. This eliminates mosh+tmux's
characteristic "ghost echo of pane-switch sequences."

### 3.3 State diff

Server-authoritative grid + cursor + attribute state per visible
pane. Diffs are computed against the client's last-acked state:

```
StateDiff {
    base_seq: u32,                    // diff applies after this seq
    window_id: u32,
    cells: Vec<CellRun>,              // (row, col, count, glyph, attr)
    cursor: Option<Cursor>,
    bell: bool,
    title: Option<String>,
    scrollback_advance: u32,          // lines added to history
}
```

A client can request a full snapshot by sending `CONTROL{
ResyncWindow(window_id) }` — used after long disconnects or
reattach.

> **Implemented (S2, opt-in `$STOA_SYNC`).** The built `StateDiff`
> (`stoa-term`) is a flatter form than the struct above: `{ resized:
> Option<(cols,rows)>, runs: Vec<CellRun{row,col,cells}>, cursor }`, with
> the **window already composited** server-side (`compose_grid` blits all
> panes + dividers into one grid) so there is no `window_id`/per-pane split
> on the wire — one diff repaints the whole active window. `title` rides
> inside the grid (re-emitted by the snapshot). The wire codec is a compact
> bounds-checked big-endian encoding (`StateDiff::encode`/`decode`); a
> decode failure is dropped, never mis-applied. Recovery is **resync-on-gap**
> (the client tracks the high-water seq; a forward gap → `CONTROL{Redraw}`
> → full repaint; a non-advancing reordered diff is dropped) rather than the
> `base_seq` last-acked diffing above — equivalent convergence, simpler, at
> the cost of a full repaint per gap (ack-based minimal-resend is the future
> refinement). `bell`/`scrollback_advance` not yet on the wire.

### 3.4 Predictive echo (clean-room SSP)

The mosh predictor's behavior, reimplemented in Rust under a
permissive license. The model:

```
client local state =
    last_acked_server_state
  ⊕ apply(predicted_inputs since last ack)
```

When the server's `ACK { up_to_seq, resulting_state }` arrives:

1. Drop predictions with `seq ≤ up_to_seq` from the queue.
2. If `resulting_state` ≠ what we predicted: fade the divergent
   region (mosh-style underline), wait for `STATE_DIFF`.
3. If converged: clean rendering, no flicker.

Predictions are heuristic (we don't run a full terminal emulator
client-side). v1 covers: printable ASCII (echo at cursor), `\b`
(backspace), `\r` (CR). Anything else suppresses prediction for
that input. Same envelope as mosh; the implementation is ours.

## 4. Multiplexer model

A **session** has 1..N windows. A window has 1..N panes in a tree
layout (split horizontally / vertically, like tmux). Each pane has
a pty + child process.

```
Session "work-build"
├── window 0 "edit"      — single pane: zsh in ~/src/bsd
├── window 1 "build"     — single pane: cargo build (running)
└── window 2 "logs"
    ├── pane 0 (left half):  tail -F /var/log/messages
    └── pane 1 (right half): journalctl -f
```

The default keymap is **tmux-compatible** for muscle memory
(`Ctrl-B c` new window, `Ctrl-B "` split horizontal, `Ctrl-B %`
split vertical, `Ctrl-B 0..9` switch). We don't reimplement tmux's
full command grammar — just the bindings that map to our
`DaemonCmd` set. Configurable via `~/.config/atrium/stoa.toml`.

Layout state (tree shape, splits, focused pane) is part of session
state; it persists.

> **Implemented (S2).** Windows are a `Vec<Window>` (closed = tombstone so
> `Ctrl-B <n>` indices stay stable); nav `c`/`n`/`p`/`l`/`0`-`9` + titles
> are live. **Panes are v1 = a flat 2-pane split per window** (`Window` holds
> `Vec<Pane>` + an `Option<Divider>` of `Vertical(col)`/`Horizontal(row)`),
> not yet the arbitrary layout tree above — `Ctrl-B %`/`"` split the active
> single-pane window into two halves, `Ctrl-B o` switches pane, and a pane
> exit un-splits (the survivor takes the full window). The renderer already
> supports N panes + multiple dividers (`compose_grid` takes vdiv/hdiv
> lists), so extending to the tree is a layout-management change, not a
> rendering one. Layout is in-memory (not yet persisted across `stoad`
> restart — that rides S3).

## 4.5 Session targets and jail exec (jexec, reimagined)

> Added 2026-06-25, reconciling Stoa with the TCB de-rooting
> (jid-0-root is now exactly jaild + portcullisd + bootstrap; every
> jail launch brokers through portcullisd). The original spec
> (2026-05-10) predates this and had `stoad` talk to jaild directly.

A Stoa session has a **target** — *which* jail its shells attach to:

```rust
enum Target {
    SessionJail,        // the user's per-user session jail (default)
    Jail(String),       // a specific running jail, by name
}
```

`SessionJail` is the common case (§17 used to call it the *only*
case): a normal login, shells in the user's own session jail.
`Jail(name)` is the new capability — **a persistent, flaky-tolerant
shell *inside a specific running jail*.** This is `jexec(8)`
reimagined: `ssh host jexec app-foo sh` dies on every drop and keeps
no scrollback; a Stoa jail-target session survives drops, roams, and
persists its history like any other Stoa session.

```
stoactl attach --jail org.atrium.editor        # jexec, persistent
stoactl new dbg --jail app-foo -- /bin/sh
```

Over the network this is just the aqueduct-remote session scope
([aqueduct-remote.md](aqueduct-remote.md) §4) pointed at a jail:
`aqueduct-shell mint stoa --jail org.atrium.editor` mints a token
whose `scope = "app:org.atrium.editor"`. No new subsystem — the
session-capability machinery already carries it.

### 4.5.1 Two spawn paths, two lifetimes

`stoad` never calls `jail_set`/`jail_attach` itself (it is jailed +
non-root). It asks the **portcullisd broker**, the sole path to
jaild after the de-rooting. There are two broker verbs, and the
lifetime distinction between them is the crux:

| | create a session jail | exec into an existing jail |
|---|---|---|
| broker verb | `LaunchSessionComponent` (exists) | **`ExecInJail`** (new) |
| target | `SessionJail` | `Jail(name)` |
| action | `jail_set` + exec a fresh jail | `jail_attach` an already-running jail |
| jail lifetime | Stoa/portcullisd owns it (holds the procdesc) | belongs to **whoever launched it** — Stoa is a *guest* |
| Stoa teardown | kills the jail | kills **only the exec'd shell**; the jail keeps running |

Tearing down a debug session must never take the target app down
with it.

### 4.5.2 The new jaild primitive

`ExecInJail { jid_or_name, exec, uid }` — jaild `pdfork`s a child
that `jail_attach`es the *existing* jid (no `jail_set`), drops to
`uid`, and execs. It is `CreateJail`'s child path minus the
`jail_set`. Gated **jaild-created-only** — the same protection
`Reap`/`SetRctl` already enforce ("refuses any jail it did not
itself create") — so a jail-exec can never attach into the TCB or
jid 0. Returns a procdesc for the exec'd shell so the broker (and
through it, Stoa) reaps exactly that process, never the jail.

### 4.5.3 The capability gate

Jail-exec is jail-escape-adjacent, so portcullisd mediates it,
default-deny, reusing identity already on hand:

- **Who may target jail X?** The launch registry already records
  `uid → (owner, app-id)` for every jaild-created jail
  (`portcullis-peer`). Requester **owns** X → allowed (debug your
  own app). Requester holds an **operator `jail_exec` capability**
  → allowed into any jaild-created jail (admin). Neither → denied.
- **Which uid inside?** Default = the jail's **app-uid (non-root)**:
  you get the app's exact view (the debugging case), and the
  de-rooting invariant holds — a jexec shell is non-root by
  default. **Root-inside-the-jail is a separate, higher cap**
  (`jail_exec_root`): an explicit, gated escalation, never implied.

**Blast radius** is correct by construction: a shell attached into
jail X runs *inside* X, bounded by X's chroot root, devfs ruleset,
and network. You gain the jail's authority — not the host's.

## 5. Persistence and Tessera-backed scrollback

Scrollback is **the** distinguishing property. tmux's scrollback is
in-memory and lost on kill. Stoa's is content-addressed on Tessera
and survives daemon restart, host reboot, anything Tessera survives.

### 5.1 Layout

For each window:

```
window state {
    tail: BytesMut (in-memory, last <= 64 KiB),
    history: Vec<BlobRef>,    // ordered list of CAS hashes
    wal_offset: u64,
}

BlobRef { hash: [u8; 32], len: u32, lines: u32 }
```

- Pty output goes first into `tail`.
- When `tail` ≥ 64 KiB or 5s elapsed: hash and write as a Tessera
  blob, append `BlobRef` to `history`, drain `tail`. Update WAL.
- The session's metadata (window list, layouts, history vectors,
  cwd, env) is itself written as a Tessera object on every change.

### 5.2 Dedup wins

Build logs and repeated `cat` of large files dedup naturally.
Concrete shape: a 200 MB `cargo build` that re-runs after a
one-line edit produces ~50 KB of unique blobs (the changed crate's
output) + reused refs to the rest.

### 5.3 WAL and crash recovery

Per-session WAL on Tessera (small, append-only):

```
wal record { seq, window_id, op: Append(bytes) | RotateBlob(hash) | Layout(...) }
```

On `stoad` startup:

1. Enumerate sessions from `/var/db/atrium/stoa/<user>/<sess>/meta.json`
   (which itself points to a Tessera object).
2. For each session: replay WAL from last checkpoint → reconstruct
   in-memory window state.
3. Live shells were children of the previous `stoad` and are gone;
   on first reattach, prompt user (default: respawn shells in
   last-known cwd; flag preserved scrollback as "session resumed").

Tessera's "blob is whole or doesn't exist" CAS semantics mean a
crash mid-blob-write either commits or doesn't; no torn-page
corruption is possible.

### 5.4 Retention

Per-session retention policy (default: keep all unique blobs up to
500 MB per window, drop oldest blobs first when over). Configurable.
Tessera's GC handles the actual reclamation when the last reference
to a blob is dropped — Stoa does not need to know about it.

### 5.5 Live-process survival across `stoad` restart (S3b — aspirational TODO)

§5.1–5.3 is **history-only**: a `stoad` restart (crash/upgrade) or host
reboot loses the live shells; on reattach they **respawn** at the last-known
cwd with scrollback restored. This is the foundation (S3a) — and the *only*
thing that can survive a host reboot, since no process survives losing RAM.

A second, aspirational layer (**S3b**) would keep the live shells running
across a `stoad` restart *without a host reboot* (so an in-flight `vim` /
`cargo build` continues through a `stoad` upgrade or crash). This is **beyond
tmux** (whose sessions die with their server) and must **not** be built by
making `stoad` a fat tmux-style process holder — that fights the thin,
jailed, restartable `stoad` design. The Atrium-native vehicle is the
**broker as session-holder**: the portcullisd→jaild `ExecInJail` chain
already produces a **procdesc + pty master over SCM_RIGHTS**. Have the broker
(the durable TCB) **retain** the procdesc + a pty master (buffering output
while `stoad` is down) keyed by a session handle, and expose a
`ReacquireSession(handle)` verb; a restarted `stoad` re-acquires the master
and resumes. The shell never sees EOF because the broker's master stays open.

Scope/known wrinkles for S3b when built: only **broker-backed (jailed)**
sessions survive — dev `DirectSpawner` sessions have no broker and don't
(acceptable asymmetry); the broker must bound its output buffer + GC orphaned
handles; reacquire must re-key the session's UDP/`K_sess` (clients re-mint).
**Not scheduled — recorded so the procdesc/SCM_RIGHTS hooks aren't designed
out.**

## 6. Multi-client coherence

Two stoactl instances attached to the same session: both receive
the same authoritative state diffs from `stoad`. Both can input.

- The daemon serializes inputs in arrival order; there is one
  authoritative pty stream.
- Each client predicts only its own input. Client A does not see
  Client B's predictions, only the converged state diffs.
- Cursor position is server-authoritative; both clients see the
  same cursor.
- Resize: `stoad` picks `min(cols)` and `min(rows)` across all
  attached clients. Disconnected clients don't count.

Use cases enabled: pair debugging without screen-sharing software;
checking on a long-running build from your phone while your laptop
is also attached; handing off mid-session from device to device.

## 7. Daemon restart and host reboot

| Event                          | Live processes | Scrollback | Layout | Cwd/env |
|--------------------------------|----------------|------------|--------|---------|
| Network drop                   | ✓              | ✓          | ✓      | ✓       |
| Client kill / device sleep     | ✓              | ✓          | ✓      | ✓       |
| `stoad` restart (no host reboot) | ✗ (children)   | ✓          | ✓      | ✓       |
| Host reboot                    | ✗              | ✓          | ✓      | ✓       |
| Tessera unmount                | depends*       | freezes    | ✓      | ✓       |
| Host crash mid-write           | ✗              | ✓ (WAL)    | ✓      | ✓       |

*If Tessera unmounts while the daemon is running: existing live
sessions continue (in-memory state, no scrollback persistence);
new sessions refused; a warning is logged and exposed via
`stoactl status`.

The "✗ live processes" rows are **not** a regression vs tmux —
tmux loses its processes on host reboot too. The win is the "✓
scrollback" column, which tmux loses every time.

Future (post-V1): integrate with checkpointed jails (FreeBSD
process checkpoint/restore is not currently mature; revisit when
it is). Out of scope for the D2.7 deliverable.

## 8. NAT / corporate firewall fallback

UDP is often blocked on corporate networks. Mosh users hit this
regularly. Stoa's fallback:

1. Client tries UDP for 5 seconds after handoff.
2. If no datagram round-trip succeeds: re-handshake with sshd,
   establish a TCP-tunneled UDP-frame channel through the SSH
   connection (envelope unchanged, transport changed).
3. Performance is worse (TCP head-of-line blocking; SSP's
   loss-tolerance benefits gone) but always works.

`stoactl status` exposes the current transport.

## 9. Resize semantics

```
client → stoad: Resize { window_id, cols, rows }
stoad: per-window negotiated_size = min(over attached clients)
       if changed:
         tcsetwinsz(pty_fd, negotiated_size)
         broadcast STATE_DIFF { resize: ... } to all clients
```

Clients render at `negotiated_size`; if their viewport is bigger,
the rest is empty space (no letterboxing-mode in v1). When the
last client with the smallest viewport detaches, the size grows.

## 10. File and clipboard transfer

First-class daemon ops, not separate tools.

```
stoactl push  ./big.tar  ~/big.tar     → DaemonCmd::Push
stoactl pull  ~/result.zip  ./         → DaemonCmd::Pull
```

Bytes are CAS-uploaded via the same UDP envelope (chunked,
sequenced, MAC'd). Optionally short-circuited via Tessera if the
source already exists in CAS (zero-copy push).

Clipboard:

- OSC52 sequences emitted by tools inside the session are
  intercepted by `stoad` and exposed via `DaemonCmd::ClipboardWrite`.
- `stoactl` mirrors that to the local OS clipboard (macOS `pbcopy`,
  X11 `xclip`, Atrium → Tabula).
- `Ctrl-B y` requests the local clipboard to inject into the pty.

This means cross-host copy/paste actually works without xforwarding
or tmux-hacks.

## 11. Security

- `stoad` runs **jailed + non-root** (a dedicated `_stoad` uid). It
  holds no privilege of its own: every shell spawn — creating the
  user's session jail (`LaunchSessionComponent`) or exec'ing into an
  existing jail (`ExecInJail`, §4.5) — goes through the
  **portcullisd broker**, the sole caller of `jaild` (itself the
  sole `jail_set`/`jail_attach` caller) after the TCB de-rooting.
  portcullisd capability-checks each request (ownership /
  `jail_exec` / `jail_exec_root`); granting `stoad` jaild-socket
  access directly would be escape-equivalent, so it never gets it.
- Each session's MAC key is derived from a per-session nonce ⊕
  SSH session id; not stored across daemon restarts (re-handshake
  is required after restart, even if the underlying session
  resumes).
- All datagrams are MAC'd; replay window of 128 seqs.
- No raw memory of input bytes is logged; scrollback is *output
  only*. Type-passwords-into-terminal is no more or less leaky
  than today (the bytes hit the pty, the pty's tty driver decides
  echo, which is the same as ssh+tmux).
- `stoactl` shows the active session's MAC key fingerprint (`stoactl
  status`) so a user can verify they're attached to the right
  session, not a spoofed one.

### 11.1 The `ShellSpawner` seam (dev + portability)

The privileged spawn sits behind a `ShellSpawner` trait, mirroring
ostiarius's `Launcher` seam (the de-rooting pattern):

- **`BrokerSpawner`** (FreeBSD, production) — brokers through
  portcullisd (`LaunchSessionComponent` for `SessionJail`,
  `ExecInJail` for `Jail(name)`). The real, jailed,
  capability-checked path.
- **`DirectSpawner`** (macOS + FreeBSD dev) — `openpty` + `fork` +
  `execve` a shell as the current user, no jail. Errors on a
  `Jail(name)` target (no jails off-Atrium). This is how the
  transport / SSP predictor / multiplexer — the bulk of the code,
  and the flaky-connection behavior that motivates Stoa — are
  testable on the macOS host with no VM in the loop.

A macOS `stoactl` is a **shipping** client (TUI; the Fresco-GUI
variant of §13 is Atrium-only). A macOS `stoad` is a **dev harness**
— invaluable for fast protocol iteration, not a product (Atrium is
FreeBSD). The wire is identical; only the far-end spawner differs.

## 12. Aqueduct integration

Stoa's local control plane (not the wire-to-stoad protocol; that's
direct UDP for performance) rides Aqueduct:

- `stoactl list-sessions` → Aqueduct call to `stoad` over
  `/var/run/atrium/stoad.sock`.
- `CLASS_STOA = 8` (next free; see [aqueduct.md](aqueduct.md) class
  registry).
- The Forum (D3 dock/launcher) gets a "Sessions" panel for free —
  it speaks Aqueduct, queries stoad's class, lists sessions, lets
  the user pick one to attach.
- Praeco (notifications, D3) can post a notification when a
  long-running session emits a bell or completes a flagged
  command; that's a `DaemonCmd::WatchExit { window_id }` returning
  via Aqueduct event.

The wire to-stoad-from-stoactl-over-UDP and the local-control-plane
over-Aqueduct are independent. Both are needed.

## 13. Why this enables the graphical terminal

The Phase 4 client variant: instead of rendering to a tty, render
to a Fresco surface inside the Atrium compositor.

```
                 ┌──────────────────────────────┐
                 │    Atrium compositor (D2)     │
                 │    ┌─────────────────────┐    │
                 │    │  stoactl-gui        │    │
                 │    │  (terminal pane)    │    │
                 │    │  ─────────────────  │    │
                 │    │  zsh % cargo b...   │    │
                 │    └─────────────────────┘    │
                 └──────────────────────────────┘
                              │
                              │ UDP to local stoad
                              ▼
                          stoad
                              │
                              ▼
                          pty + zsh
```

Properties:

- Same wire protocol; only the renderer is different.
- The "terminal app" (atrium-term in D3) is just a `stoactl-gui`.
  `atrium-term <session>` opens or attaches.
- Closing the window detaches; the session continues. Reopen with
  the same arg → reattach, scrollback intact.
- This is **the** answer to "what's a graphical terminal in Atrium?"
  We don't need a separate terminal emulator with its own pty
  ownership and its own scrollback; we have stoa, which already
  does both.
- Running atrium-term against `stoad` over a network (the host
  is a remote machine) and against the local stoad uses the same
  binary; the address selects.

This collapses two designed-separately components (terminal
emulator + remote-shell client) into one.

## 14. CLI surface

```
stoactl new <name> [--jail <id>] [-- <cmd>]   Create a session; --jail targets a specific jail (§4.5)
stoactl attach <name> [--jail <id>]           Attach (creates if not exists); --jail = jexec into that jail
stoactl list                       List my sessions on this host
stoactl detach                     Detach the active client (session continues)
stoactl kill <name>                Kill a session and its processes
stoactl push <local> <remote>      Push a file
stoactl pull <remote> <local>      Pull a file
stoactl status                     Show transport, MAC fingerprint, etc.
stoactl rename <old> <new>         Rename a session

# In-session keybindings (default, tmux-compatible):
Ctrl-B c        new window in current session
Ctrl-B "        horizontal split
Ctrl-B %        vertical split
Ctrl-B 0..9     switch window
Ctrl-B d        detach
Ctrl-B [        scrollback mode
Ctrl-B y        paste from local clipboard
```

Across a network:

```
ssh -T user@host stoactl attach work-build       # one-step attach
```

`stoa-shell` is invoked transparently by the user's `~/.ssh/rc` or
as their login shell when stoa is set up; `stoactl` over SSH
detects "I'm being run via stoa-shell" and switches modes.

## 15. Phase plan

| Slice | Scope                                                         | LoC est | Time |
|-------|---------------------------------------------------------------|---------|------|
| S0    | Skeleton: stoad + stoactl + sshd handoff + 1 window, TCP, no prediction. `ShellSpawner` seam with `DirectSpawner` — **fully testable on the macOS host, no VM** (§11.1) | 2k      | 1–2 wk |
| S1    | UDP envelope + MAC + replay window + clean-room SSP predictor (macOS host, injected loss/reorder) | 2k      | 2 wk |
| S2    | Multi-window/pane + tmux-compat keymap + layout serialization | 1.5k    | 2 wk |
| Sj    | **Jail awareness** (§4.5): `BrokerSpawner` (portcullisd `LaunchSessionComponent`); `ExecInJail` broker verb + jaild primitive + the ownership/`jail_exec`/`jail_exec_root` cap gate; session `Target` + `--jail`. **First VM-only slice.** | 1.5k | 2 wk |
| S3    | Tessera-backed scrollback + WAL + restart replay              | 1k      | 1 wk |
| S4    | Multi-client mirror + `stoactl push/pull` + clipboard         | 1k      | 1 wk |
| S5    | UDP-over-TCP fallback + corporate-NAT story                   | 0.5k    | 1 wk |
| S6    | `stoactl-gui` (Fresco surface renderer) + atrium-term wrapper | 1.5k    | 2 wk |

Total: ~11k LoC, 12–14 weeks focused. Independent of D-track work
above it (D2.7 sits between Portcullis D2.5 and Forum D3); doesn't
block any current milestone. **S0–S2 build + test entirely on the
macOS host** (transport, predictor, multiplexer — the bulk and the
flaky-connection win); **Sj is the first slice that needs the VM**
(the jailed broker path).

## 16. Clean-room SSP — license note

Mosh's `src/statesync/` is GPL'd. We reimplement under
permissive license (Atrium policy, see `feedback_atrium_licensing_policy.md`).
Approach:

1. Specify the predictor's behavior from mosh's *paper*
   (Winstein & Balakrishnan, USENIX ATC 2012) and observable
   behavior, not from reading mosh's source.
2. Implementer must not have read mosh's `statesync/` code (we
   document this in the commit history).
3. Implementation lives in `stoa/predict/` as a self-contained
   crate.

Mosh's wire format is conceptually similar but we are not
constrained to match it; our envelope (§3.1) is our own.

## 17. Open questions (for v1)

- **Scrollback search across sessions**: do we expose `stoactl
  search "needle"` that walks all sessions' Tessera blobs? Tessera's
  CAS makes this cheap (the blobs are deduped), but the index is
  per-session for now. v1 ships per-session search; cross-session
  is a follow-up.
- **Per-jail vs per-user sessions** — RESOLVED (§4.5): a session's
  *default* target is the user's single per-user session jail
  (matches Forum/D3); a session may instead **target a specific
  jail** (`--jail`, the jexec case). Per-user-session-jail is the
  default, not the only option.
- **Logging**: `stoad` logs (connect/disconnect events, errors)
  go where? Lean: `/var/log/atrium/stoad.log`, rotated by
  `atrium-log` (D3 service-management).
- **Live tail to a file**: tmux has `pipe-pane`. We expose
  `DaemonCmd::PipeWindowTo { path }` — pty output also writes to
  `path` (in the user's jail). Useful for "I want everything from
  this window also written to `build.log`."
- **Session naming collisions**: two users with a session named
  "build" — namespaced per-user, no collision.
- **HID input to a graphical stoa client**: the GUI variant
  receives keys as HID events from the compositor (Atrium's input
  shape, not evdev). The translation to pty bytes happens in
  `stoactl-gui`, same as a tty client translates terminfo.

## 18. Relation to other Atrium components

| Component       | Relationship                                                        |
|-----------------|---------------------------------------------------------------------|
| **Aqueduct**    | local control plane (`stoactl list`); CLASS_STOA=8                  |
| **Tessera**     | scrollback persistence + dedup                                      |
| **Portcullis**  | shells spawned via the **portcullisd broker** (sole jaild caller): `LaunchSessionComponent` for the session jail, `ExecInJail` for jail-target sessions (§4.5); `stoad` is jailed + non-root |
| **sshd**        | one-time userauth handshake; otherwise uninvolved                   |
| **Vestibulum**  | the desktop login also triggers a stoa session for that seat        |
| **Forum (D3)**  | "Sessions" panel; atrium-term is `stoactl-gui` underneath           |
| **Praeco**      | notifications when a watched session emits bell or process exits    |
| **fresco-protocol** | Phase S6 GUI client renders into Atrium compositor              |

Stoa is the point at which "remote shell" stops being a
foreign-imported Unix idiom (ssh+tmux) and becomes a first-class
Atrium service.
