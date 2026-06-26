# Stoa — persistent, flaky-tolerant session service

`ssh in and run tmux`, collapsed into one host service. SSH is a one-time
auth handoff that hands the client a MAC'd-UDP capability and exits; the
shell session then lives in `stoad`, independent of any client. Network
drops, roams, and client restarts all preserve the session — the property
`ssh + tmux` can't give (ssh owns the shell's lifetime).

Design spec: [`../docs/spec/stoa.md`](../docs/spec/stoa.md). This file is the
operational runbook.

## Crates

| Crate         | Role |
|---------------|------|
| `stoa-proto`  | wire envelope (`ver|type|seq|payload|MAC[16]`, truncated HMAC-SHA256) + 128-seq anti-replay window |
| `stoa-net`    | UDP datagram transport (per-direction seq + replay over the envelope) |
| `stoa-spawn`  | the one OS seam: `ShellSpawner` — `DirectSpawner` (forkpty, dev/macOS) vs `BrokerSpawner` (FreeBSD: portcullisd `ExecInJail`) |
| `stoa-term`   | server-side VT/ANSI grid emulator + `StateDiff` (diff/apply/wire codec) + renderers (`render_snapshot`/`render_diff`/`compose_grid`) |
| `stoa`        | the daemon + client: bins `stoad`, `stoactl`, `stoa-shell` |

## Build

Host (dev, fast iteration — macOS or FreeBSD):

```sh
cargo build              # debug bins in target/debug/
cargo test               # 72 host tests
```

Cross-compile for the FreeBSD VM (never run cargo *in* the VM):

```sh
cargo build --release --target aarch64-unknown-freebsd
# → target/aarch64-unknown-freebsd/release/{stoad,stoactl,stoa-shell}
```

## Run

```sh
stoad &                                   # daemon (control socket at $STOA_CTL)
stoactl attach work                       # create/resume session "work", local
stoactl attach work --host user@host      # remote: ssh user@host stoa-shell work
stoactl attach dbg  --jail app.example    # jexec into a running jail (jexec, reimagined)
stoactl list                              # sessions on the local stoad
stoactl kill work
```

Detach with **Ctrl-]** (or `Ctrl-B d`); the shell keeps running. Reattach
with the same `attach` command — same shell, fresh key + seq.

## Keymap (tmux-style prefix, default Ctrl-B)

| Key            | Action |
|----------------|--------|
| `c`            | new window |
| `n` / `p`      | next / previous window |
| `l`            | last (toggle) window |
| `0`–`9`        | switch to window N |
| `%` / `"`      | split active window vertically / horizontally (panes) |
| `o`            | switch pane |
| `[` / `]`      | scrollback page up / down (any key returns to live) |
| `r`            | redraw (repaint from the server grid mirror) |
| `d` / `Ctrl-]` | detach |
| prefix twice   | send a literal prefix byte |

## Environment

| Var                 | Meaning |
|---------------------|---------|
| `STOA_CTL`          | `stoad` control socket path (mint happens here). Default: per-uid temp path. |
| `STOA_STATE`        | session-state **directory** (`meta.json` + `scrollback/`) — sessions, layout, cwd, and scrollback are restored from it on `stoad` start and snapshotted on change, so a `stoad` restart / host reboot brings sessions back (shells respawn at their cwd, scrollback restored; S3a). `off` or empty disables persistence. Production points it at stoad's `/atrium-data` volume. Default: per-uid temp dir. |
| `STOA_PREFIX`       | override the Ctrl-B prefix, e.g. `C-a` or a decimal byte. |
| `STOA_SYNC=1`       | **grid-sync streaming** — OUTPUT carries encoded `StateDiff`s painted with `render_diff`; **self-healing** on a flaky link (corrupt → repaint, lost/reordered → resync). Supersedes the predictor. |
| `STOA_PREDICT=1`    | predictive local echo (raw byte mode only; hides round-trip latency). Mutually exclusive with `STOA_SYNC`. |
| `STOA_SSH`          | the `--host` transport command (default `ssh -T`). |
| `STOA_SESSION_JAIL` | for a *jailed* `stoad`: route a plain `attach` (no `--jail`) into this session jail via the broker. |
| `STOA_BROKER`       | portcullisd socket the `BrokerSpawner` connects to (FreeBSD). |
| `STOA_DROP=N`       | **test-only** fault injector: drop 1-in-N OUTPUT datagrams. Off unless set. |
| `STOA_REORDER=N`    | **test-only** fault injector: swap every Nth OUTPUT datagram with its successor. Off unless set. |

## Tests / harnesses

- `cargo test` — 72 unit/integration tests (proto, net, term, spawn, predictor, codec).
- `scripts/loss_recovery_test.py` — drives real `stoad`+`stoactl` over loopback
  with injected loss/reorder and asserts a `$STOA_SYNC` client **converges**
  on the correct screen (proven through 1/2 loss + 1/2 reorder). Run after a
  `cargo build`:

  ```sh
  python3 scripts/loss_recovery_test.py
  # or against deployed bins: STOA_BIN=/path/to/bins python3 scripts/loss_recovery_test.py
  ```

## Deploy to the FreeBSD VM

`stoad` runs **jailed + non-root** (`_stoad`, manifest
`etc/services.d/50-atrium-stoad.toml`); shells into jails go through the
portcullisd → jaild `ExecInJail` broker. The jail nullfs-mounts host
`/usr/local/bin` read-only, so the jailed service execs the host binary.

```sh
# 1. cross-compile (above), then deploy (mv-aside the busy binary, sha-verify):
for b in stoad:atrium-stoad stoactl:stoactl stoa-shell:stoa-shell; do
  src=${b%%:*}; dst=${b##*:}
  cat target/aarch64-unknown-freebsd/release/$src \
    | ssh VM "mv -f /usr/local/bin/$dst /usr/local/bin/$dst.old; cat > /usr/local/bin/$dst && chmod +x /usr/local/bin/$dst"
done
# 2. reboot the guest so bootstrap relaunches the jailed atrium-stoad on the new binary.
# 3. verify shas match before/after; never `kill -9` QEMU (use the QMP `quit`).
```

Never run `cargo` inside the VM (it hangs hard and corrupts the qcow2).
