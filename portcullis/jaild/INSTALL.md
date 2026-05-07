# Installing `atrium-jaild` on FreeBSD

This is the operational install path for the privileged jail-creation
broker. Spec: `docs/spec/portcullis.md` §0.5,
`docs/spec/login-handoff.md`.

## Why root, not a dedicated user

`atrium-jaild` runs as `root`. There is no `_jaild` system uid:

- `jail_set(2)` requires `PRIV_JAIL_SET`, granted only to root.
- `pdfork(2)` + `setuid(2)` in the child path also require root
  (the child drops to a user uid only *after* attaching the new
  jail).

Privilege containment is via the **socket mode (0600)**, validated
by the kernel and refused at the application layer too: jaild
calls `getpeereid(3)` on every connection and rejects non-root
peers. Only `portcullisd` (also root) is expected to connect.

## Files

| File                              | Owner       | Mode | Purpose                          |
|-----------------------------------|-------------|------|----------------------------------|
| `/usr/local/bin/atrium-jaild`     | `root:wheel`| 0755 | The broker binary                |
| `/usr/local/etc/rc.d/atrium-jaild`| `root:wheel`| 0755 | rc(8) service script             |
| `/etc/atrium/jaild.policy.toml`   | `root:wheel`| 0640 | The allow-list (root-of-trust)   |
| `/var/run/atrium/`                | `root:wheel`| 0755 | Sockets + state + pidfile        |
| `/var/log/atrium/`                | `root:wheel`| 0755 | Logs                             |

The policy file is `0640` (not `0644`): readable to root only at
runtime; group-readable for audit-tool access if a separate
`atrium-audit` group is configured. Keep this conservative.

## Cross-compile from macOS host

The Atrium dev loop is macOS host → FreeBSD VM via the already-set-up
cross-compile environment:

```sh
cd ~/src/bsd/portcullis
cargo build --release --target aarch64-unknown-freebsd -p jaild
```

Binary lands at:
`portcullis/target/aarch64-unknown-freebsd/release/atrium-jaild`

## Install (in the VM)

```sh
# binary + rc script
install -m 0755 -o root -g wheel \
    /mnt/host/portcullis/target/aarch64-unknown-freebsd/release/atrium-jaild \
    /usr/local/bin/atrium-jaild
install -m 0755 -o root -g wheel \
    /mnt/host/portcullis/jaild/etc/atrium-jaild \
    /usr/local/etc/rc.d/atrium-jaild

# policy file
install -d -m 0755 -o root -g wheel /etc/atrium
install -m 0640 -o root -g wheel \
    /mnt/host/etc/jaild.policy.toml \
    /etc/atrium/jaild.policy.toml

# runtime + log dirs
install -d -m 0755 -o root -g wheel /var/run/atrium
install -d -m 0755 -o root -g wheel /var/log/atrium

# enable + start
sysrc atrium_jaild_enable=YES
service atrium-jaild checkpolicy
service atrium-jaild start
service atrium-jaild status
```

## rc.conf knobs

```
atrium_jaild_enable="YES"
# Optional overrides; defaults shown.
# atrium_jaild_policy="/etc/atrium/jaild.policy.toml"
# atrium_jaild_socket="/var/run/atrium/jaild.sock"
# atrium_jaild_state="/var/run/atrium/jaild.state.toml"
# atrium_jaild_logfile="/var/log/atrium/jaild.log"
# atrium_jaild_log_level="info"
```

## Subcommands

```sh
service atrium-jaild start          # bring up
service atrium-jaild stop           # SIGTERM, then SIGKILL after 5s
service atrium-jaild status         # liveness check
service atrium-jaild checkpolicy   # parse policy without starting
```

## Operations

### Reloading the policy

There is no SIGHUP handler in V1b/c. To apply policy changes:

```sh
service atrium-jaild stop
service atrium-jaild checkpolicy
service atrium-jaild start
```

A restart drops in-memory state but reloads the persistent
state file at `/var/run/atrium/jaild.state.toml`, so the broker
re-claims jails it had created. (No procdesc fds survive — the
`exec` path expected the original requester to hold those.)

### Persistent state file

`/var/run/atrium/jaild.state.toml` is the durable record of
*persistent* jails (those without `ExecSpec`). Format is TOML;
human-readable. Atomically replaced on every create/remove.

If the file is missing, jaild starts fresh (assumes no jails
known). If schema_version doesn't match, jaild refuses to start
— migrate by hand.

### Audit log

Every accept + every dispatch is logged. Default level `info`
goes to `/var/log/atrium/jaild.log`. Set `RUST_LOG=debug` (or
edit `atrium_jaild_log_level` in rc.conf) for more detail.

## Troubleshooting

| Symptom                                    | Likely cause                                           |
|--------------------------------------------|--------------------------------------------------------|
| "policy file failed validation"            | TOML syntax error or schema_version mismatch          |
| "non-root peer; refusing"                  | Some non-root process tried to connect                |
| "name.duplicate"                           | Persistent jail with that name is already in state    |
| "frame too large"                          | Caller sent > 64 KiB request — protocol bug           |
| `jail_set: Operation not permitted`        | Not running as root, or kernel jail support stripped  |
