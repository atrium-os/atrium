# jaild policy file — `/etc/atrium/jaild.policy.toml`

**Status:** spec, 2026-05-07
**Owner:** D2.5 (jaild crate)

The single source of truth for what jaild will allow. Every
request from portcullisd is validated field-by-field against this
file; anything outside the allow-list is refused with `EPERM` and
a structured reason. jaild reads the file once at startup, parses
into typed structures (Rust serde), keeps in memory. Hot-reload is
**not** supported — restart jaild to apply changes (rare; this is
system config).

This is the de-facto root of trust for Atrium's runtime. If
portcullisd is fully compromised, the worst it can do is whatever
this policy permits. So the policy is conservative: each entry is
explicit, no wildcards except where called out, the file ships
under change-control.

## Top-level shape

```toml
schema_version = 1                    # bump on incompatible changes

[mount_sources]
ro_paths       = [...]
rw_paths       = [...]
rw_patterns    = [...]                # glob, narrowly used

[devfs_rulesets]
allowed        = ["..."]              # named rulesets defined in /etc/devfs.rules

[exec_paths]
allowed_prefixes = [...]              # path prefix match

[env]
allowed_keys    = [...]               # exact match
allowed_prefixes = [...]              # prefix match (e.g. "ATRIUM_")

[uid]
min_user_uid   = 1000
max_user_uid   = 65000
allowed_system_uids = [...]           # specific non-user uids (e.g. _frescod)

[gid]
allowed_system_gids = [...]

[children_max]
max            = 64

[network]
allow_disable  = true
allow_host     = false
allowed_addrs_on_lo0 = [...]

[gpu_drivers]
[gpu_drivers.attested.<name>]         # one stanza per kernel driver
status                = "production" | "experimental" | "broken"
isolation_test_passed = true | false
isolation_test_date   = "YYYY-MM-DD"
isolation_test_commit = "<git hash>"
notes                 = "..."

[services]
# Named profiles for system services. Each entry constrains what
# portcullisd may ask jaild to do for that service identifier.
[services.<name>]
exec_path             = "/usr/local/bin/<binary>"
allowed_devfs_ruleset = "atrium-..."
required_mounts_ro    = [...]
required_mounts_rw    = [...]
allowed_extra_mounts_ro = [...]       # optional additional ro mounts
allowed_extra_mounts_rw = [...]
network               = "disable" | "host" | "lo0-only"
uid                   = "root" | "user" | <specific uid>
children_max          = N

[apps]
# Constraints for user-launched apps (per-app jails created at
# user request). Per-app capability schema (atrium.toml) layers
# on top of this; jaild's job is the outer envelope.
allowed_exec_root     = "/usr/local/share/atrium/apps"
default_devfs_ruleset = "atrium-baseline"
max_simultaneous_per_user = 32
```

## Validation rules

For each request that arrives over the jaild socket, jaild
validates:

| Request field | Rule |
|---------------|------|
| `name` | string, ≤ 64 chars, must match `^[a-z0-9-]+$`, must start with one of `atrium-`, `system-`, `user-`, `app-` |
| `path` | exists, is a directory, must equal one of `[mount_sources].ro_paths` ∪ `rw_paths` ∪ a path matching one of `rw_patterns` |
| `mounts.*.source` | each source must be in `[mount_sources]` (matching the `ro` vs `rw` half) |
| `mounts.*.dest` | path string; rejected if traverses `..` or contains symlinks |
| `devfs_ruleset` | must be in `[devfs_rulesets].allowed` |
| `ip4` | one of "disable" / "host" / list of addrs ⊆ `[network].allowed_addrs_on_lo0` |
| `children.max` | integer, 0 ≤ N ≤ `[children_max].max` |
| `exec_path` | must start with one of `[exec_paths].allowed_prefixes` |
| `argv[0]` | basename must equal the basename of `exec_path` (defends against argv0-spoof) |
| `env keys` | each key must be in `[env].allowed_keys` or have a prefix in `[env].allowed_prefixes` |
| `uid` | within `[uid].min_user_uid..max_user_uid` OR in `[uid].allowed_system_uids` |
| `gid` | similar |
| `gpu` requested | requires `[gpu_drivers.attested.<bound_driver>].status == "production"` and `isolation_test_passed == true` |
| `service` (when set) | must be a key in `[services]`; the request's other fields must be a subset of `services.<name>` |

On any rule violation: `EPERM` + a structured response body
naming the failing rule. portcullisd surfaces the failure to
whoever made the request (vestibulum, supervisor, etc.).

## Sample policy file

A complete, working example shipped at `etc/jaild.policy.toml`
(installed to `/etc/atrium/jaild.policy.toml`). Designed for D0+V7
(virtio-gpu attested) and D2 (vestibulum + supervisor system services).

See `etc/jaild.policy.toml` in this repo.

## Schema versioning

`schema_version = 1` is mandatory. jaild refuses to start if the
field is missing or unsupported. Bumping the schema is a deliberate
breaking-change action: write a migration tool that converts old
policy files; ship both the new jaild and the migration in the
same release.

## What's deliberately NOT in the policy file

- **Per-user capability grants** — those live in
  `/var/db/atrium/<user>/policy.toml`, owned by portcullis-policy
  and prompted to the user. They constrain what apps the user has
  granted permission to; jaild policy constrains what portcullisd
  is allowed to ask jaild to do at all.

- **Per-app manifest** — `atrium.toml` lives with the app and
  declares what capabilities it requests. portcullisd reads it to
  build a request to jaild; jaild does not read it.

- **Per-session state** — `/var/run/atrium/sessions/*.toml`,
  managed by portcullisd, ephemeral.

- **GPU isolation test results history** — the `gpu_drivers.
  attested.*` stanza records the *current* attestation. Historical
  test runs go to `/var/log/atrium/gpu-isolation-tests/`.

These layers don't bleed into one another. Each has a single owner
and a single audience.

## Audit + change-control

Because jaild policy is the root of trust, any change must:

1. Land via PR with at least one reviewer.
2. Be validated by `jaild --check-policy /etc/atrium/jaild.policy.toml`
   before service restart.
3. Be logged: jaild logs every policy-decision-affecting startup
   (or hot-reload, if we ever add that) to
   `/var/log/atrium/jaild.audit.log` with a hash of the policy
   file content.

Future work (post-D2.5): consider signing the policy file (e.g.
detached minisign signature alongside it) so that runtime jaild
can verify the file is the one the operator approved. Defer for
v1.
