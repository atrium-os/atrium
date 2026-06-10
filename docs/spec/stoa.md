# Stoa — persistent session service

Status: design (D2.7).
Last updated: 2026-05-10.

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
  Runs as `_atrium`; spawns user shells via `jaild` into the
  user's Portcullis session jail.
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

- `stoad` runs as `_atrium`. Spawning a shell as `<user>` requires
  asking `jaild` (Portcullis's privileged broker; spec/portcullis.md
  §0.5) to do it.
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
stoactl new <name> [-- <cmd>]      Create a session, optionally with a command
stoactl attach <name>              Attach to a session (creates if not exists)
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
| S0    | Skeleton: stoad + stoactl + sshd handoff + 1 window, TCP, no prediction | 2k      | 1–2 wk |
| S1    | UDP envelope + MAC + replay window + clean-room SSP predictor | 2k      | 2 wk |
| S2    | Multi-window/pane + tmux-compat keymap + layout serialization | 1.5k    | 2 wk |
| S3    | Tessera-backed scrollback + WAL + restart replay              | 1k      | 1 wk |
| S4    | Multi-client mirror + `stoactl push/pull` + clipboard         | 1k      | 1 wk |
| S5    | UDP-over-TCP fallback + corporate-NAT story                   | 0.5k    | 1 wk |
| S6    | `stoactl-gui` (Fresco surface renderer) + atrium-term wrapper | 1.5k    | 2 wk |

Total: ~9.5k LoC, 10–12 weeks focused. Independent of D-track work
above it (D2.7 sits between Portcullis D2.5 and Forum D3); doesn't
block any current milestone.

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
- **Per-jail vs per-user sessions**: a user's sessions all share a
  single Portcullis session jail (matches current Forum/D3
  behavior). Could change if we ever want per-session jails.
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
| **Aqueduct**    | local control plane (`stoactl list`); CLASS_STOA=9                  |
| **Tessera**     | scrollback persistence + dedup                                      |
| **Portcullis**  | session shells run in user's session jail; jaild spawns them        |
| **sshd**        | one-time userauth handshake; otherwise uninvolved                   |
| **Vestibulum**  | the desktop login also triggers a stoa session for that seat        |
| **Forum (D3)**  | "Sessions" panel; atrium-term is `stoactl-gui` underneath           |
| **Praeco**      | notifications when a watched session emits bell or process exits    |
| **fresco-protocol** | Phase S6 GUI client renders into Atrium compositor              |

Stoa is the point at which "remote shell" stops being a
foreign-imported Unix idiom (ssh+tmux) and becomes a first-class
Atrium service.
