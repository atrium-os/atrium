# App, workspace and console jails through jaild

**Status: ACCEPTED 2026-11-25. J0 and J1 built (§3.1, §4.1); J2 next** (codebase review
H3, option (a), chosen by Shankar 2026-11-24). portcullisd does not use any of it yet.

## 1. Why

[portcullis.md](portcullis.md) §2 says jaild is the only caller of
`jail_set`/`jail_remove`, and that portcullisd, the large policy
interpreter, only asks it. In practice portcullisd writes a jail.conf and
runs `jail -c` as host root for every app, workspace and console. jail(8)
runs that file's `exec.prestart`/`exec.created` strings as host root
through `sh -c`. So the strict validator in `jaild/src/validator.rs` gates
system services, components and the one-shot lane, but not ordinary apps.
Review finding C1 was a manifest entry that reached `exec.prestart`.

The goal is that every jail on the machine is created by jaild from a typed
request that its validator checks. portcullisd keeps its other root work
(overlay mounts, `pw`, registries) for now; that is M3.

## 2. What portcullisd does today

| Site | What it does | jaild today |
|---|---|---|
| `launch.rs` `run_one_jail_inner` (apps, setup and run phases, workers, debug) | `jail -c -f conf name` with the caller's stdio, waits for exit, then `jail -r` | `CreateJail` with `ExecSpec { stdio: true }` returns a procdesc; portcullisd already uses this for session components (`main.rs` `launch_session_component`) |
| `launch.rs` `create_persistent` (workspaces `workspace.rs`, console jails `console.rs`, empty consoles `empty.rs`) | `jail -c` with `persist`, entry `true` | `CreateJail` with `exec: None` |
| `remove_persistent`, `full_teardown`, the sweep in `launch.rs:2265`, `egress.rs`, `main.rs:1408` | `jail -r NAME` | `RemoveJail` |
| `console.rs:320`, `ide.rs:249/337`, `egress.rs:57` | `fork` + `jail_attach` + drop privileges in portcullisd | `ExecInJail` (pty only) |
| `jls` in `launch.rs`, `empty.rs`, `workspace.rs`, `console.rs` | parse text output for jid, path, persist | nothing: jaild's state has it but no query |

What the rendered jail.conf carries, and its jaild equivalent:

| jail.conf | Set by | jaild |
|---|---|---|
| `path`, `host.hostname/hostid/hostuuid`, `children.max` | `portcullis-jail` `build` | fields exist |
| `mount` lines (nullfs, tmpfs, sockets by capability) | capabilities, workspaces | `mounts: Vec<MountSpec>`, sources checked against `mount_sources` |
| `mount.devfs`, `devfs_ruleset` | `build` | `devfs_ruleset` |
| `exec.prestart = devfs -m … rule apply …` (capability devices: audio, input, …) | `build` | `devfs_grants` by name, applied by jaild; **mapping from capability to grant name needed** |
| `vnet = new` + `exec.created` ifconfig/route strings (loopback, full) | capabilities, `attach_routed_net` | `NetworkConfig::Loopback`, `Routed { mac, grants, fib }`: jaild builds the epair itself, so the shell strings go away |
| `exec.start = /<entry>`, `exec.jail_user` (host passwd), `exec.clean` | `build` | `ExecSpec { path, argv, env, uid, gid }`; **no supplementary groups** (jaild sets exactly `{gid}`) |
| `sysfil.*` | `app_sysfil` resolves the class in portcullisd | `syscall_profile`: jaild resolves the class from its own policy |
| `allow.unprivileged_proc_debug`, `allow.mount/raw_sockets/chflags/set_hostname = false`, `enforce_statfs = 2` | workspaces | **missing**: no `allow.*` fields |
| exec as the jail's root (`exec.jail_user = root`) | workspaces, consoles | **refused today** unless a service class allows it |
| `copies` (`/etc/localtime`) | `build` | not a jail parameter: portcullisd writes it into the overlay before the request, as now |

## 3. Changes to jaild

1. **An `app` class in the validator**, with the same shape as the
   service classes in `service_instance`. Policy gives an apps root
   pattern (for example `/var/lib/atrium/apps/*/*`), the app uid range,
   and the mount sources an app may get. jaild also reads the launch
   registry read-only and refuses an exec uid that is not registered for
   the jail's app and owner. This is an independent check, so a confused
   portcullisd cannot run an app as another app's uid.
2. **Supplementary groups** in `ExecSpec` (`groups: Vec<u32>`, at most
   16). Today apps get their groups from `initgroups` when jail(8) starts
   them: `home.rs` `grant_group`/`grant_persona_group` add the app's
   account to `files-<person>` and the persona groups. jaild refuses any
   group other than the exec gid and those two kinds for the jail's
   owner, and the privilege drop checks itself afterwards (`getgroups`
   equals the list).
3. **Named device grants for capabilities.** `jaild.policy.toml` maps each
   capability's device set (audio: `dsp*`, `mixer*`; input:
   `input/event*`; …) to a grant name. portcullisd sends names, never
   devfs rule text, and no shell runs.
4. **A `develop` class for workspaces**: `allow.unprivileged_proc_debug`,
   the four `allow.*` refusals and `enforce_statfs = 2` are set by jaild
   for that class, not sent as free parameters. An exec as uid 0 inside
   the jail is allowed only for `develop` and `terminal`, and only with
   `children.max = 0`, as today.
5. **`ExecInJail` with stdio** (no pty): three descriptors in, procdesc
   out, for the IDE's LSP/DAP servers and egress probes.
6. **`JailInfo { name }` → `{ jid, path, persist }`**, answered from
   jaild's state, so portcullisd stops parsing `jls`.

Every new field is bounded and validated as the existing ones are, and
anything the validator does not know is refused.

### 3.1 As built (J0)

- `CreateJailRequest.class`: `app { owner, app_id, debug }`, `develop
  { owner }`, `terminal { owner }`. Any class needs a login-name owner and
  `children_max = 0`. An app needs an exec whose uid and gid are not 0,
  whose uid the launch registry binds to that owner and app (read fresh
  per request; an unreadable registry denies), whose gid is that
  account's, and whose `groups` (at most 16, user range) are the owner's
  files groups (`files-<owner>`, `files-<owner>-<persona>` or home.rs's
  hashed form). `develop` and `terminal` are created empty.
- A classed jail gets `allow.mount/raw_sockets/chflags/set_hostname = 0`
  and `enforce_statfs = 2` set explicitly, not left to the kernel's
  defaults (which allow `set_hostname` and `unprivileged_proc_debug`), and
  `allow.unprivileged_proc_debug` only for `develop` or `app` with `debug`
  (an app under lldb-server, M10). Unclassed service jails are unchanged.
- `devices`: `fresco` (`fresco0`), `input` (`input/event*`), `camera`
  (`video*`), unhidden in the jail's own devfs by `devfs rule apply` with
  an argument vector, no shell. Audio has no class: Lyra serves it.
- `ExecInJail` gains `identity { uid, gid, groups }`, `cwd`, `umask` and
  `stdio` (exactly three descriptors, no pty, `setsid`). Into an app jail
  only as its recorded uid. Into `develop`/`terminal`: the jail's root as
  0:0 with no other groups, or the owner's own account or an app uid the
  registry binds to the owner, with that account's gid and the owner's
  files groups; with no identity and no `want_root` it is refused. The
  directory is entered after the drop, and failing to enter it is fatal.
- `JailInfo { name }` answers `{ jid, path, persist }` from jaild's state
  for jails jaild made (jid `None` when nothing runs in it).
- `ListenInJail { name, port }` (for `egress.rs`'s proxy and the M10
  debugger): a child attaches, binds 127.0.0.1:`port`, listens and passes
  the socket back; `develop` jails and `app` jails with `debug` only.
- Mounts: jaild now makes the mounts of a classed jail created empty (it
  ignored them before, which a workspace's would have hit); an unclassed
  jail with no exec still gets none, and says so in the log. Every nullfs
  into an `app` jail is `nosuid`, and `MountSpec.noexec` maps jail.conf's
  `noexec`. A destination that resolves outside the jail root once
  symlinks are followed is refused (a writable root can hold a link from
  an earlier run, and nmount follows it).
- An `app` jail may have `<apps.sockets_dir>/<name>` read-write for each
  name in `jaild.policy.toml` `[apps] sockets`, beside the global
  allowlist; a test keeps the list equal to what portcullis-jail mounts.

## 4. Changes to portcullisd and portcullis-jail

- `portcullis-jail` gets `JailConfig::to_create_request(&self) ->
  Result<CreateJailRequest, String>`, which **refuses** any parameter it
  cannot map instead of dropping it. A test runs every sample manifest
  through both renderers and checks each jail.conf parameter has its typed
  counterpart.
- `run_one_jail`, `create_persistent` and `remove_persistent` call jaild.
  They wait on the procdesc jaild returns, as `launch_session_component`
  already does, with the caller's stdio passed as `ExecSpec { stdio: true }`.
- The `jail_attach` sites call `ExecInJail`. portcullisd no longer forks
  into jails.
- When the last caller is gone, `render_jail_conf`, `write_jail_conf` and
  the jail(8) code are deleted, which removes C1's whole surface.

### 4.1 As built (J1)

`to_create_request` lives in portcullisd (`src/jail_request.rs`), not in
portcullis-jail: portcullis-jail is also a dependency of memfed, which would
otherwise take jaild and its dependencies into its lockfile. It takes the
class, who the entry runs as (uid, gid, groups, arguments, environment,
stdio), the `full` network's resolved grants and the system call class,
and maps each jail.conf parameter or refuses the request:

- `exec.start` must be one plain absolute path; arguments come only from
  the caller, so J2 stops formatting `--persona …` into the string.
- `exec.prestart` is accepted only as exactly the devfs grants `build`
  renders, each of which must be a jaild device class.
- `exec.created` is accepted only as the loopback capability's string;
  a network already attached for jail(8) is refused.
- `allow.*`, `enforce_statfs` and `children.max` are accepted only at the
  values jaild itself sets for the class.
- `sysfil.*` needs a named class, which jaild resolves again.
- `mount.devfs = false` means no devfs; mounts must be nullfs `ro` or
  `rw`, under the root, with `noexec`, or `nosuid` in an app jail.
- `persist`, `exec.clean`, `exec.system_jail_user` and `exec.jail_user`
  carry nothing jaild needs: an exec'd jail ends with its entry, an
  empty one persists, and the identity is numbers from the caller.

Its tests run every shipped app manifest through `build` and the mapping
and then through jaild's validator with the shipped policy.

## 5. Order of work

One path only (Shankar, 2026-11-25): there is no flag and no fallback. Each
kind of jail moves to jaild in the commit that deletes its jail(8) code.

| Step | What | Gate |
|---|---|---|
| J0 | jaild: `groups`, `JailInfo`, stdio `ExecInJail`, typed device classes, `develop`/`terminal`/`app` classes; validator tests for each refusal | jaild validator tests on the host; the existing service gates still pass in a VM |
| J1 | `to_create_request`, refusing anything it cannot map | host tests over every sample manifest |
| J2 | consoles, workspaces and apps through jaild in one sweep; `render_jail_conf`, `write_jail_conf` and the `jls`/`jail -r`/`jail_attach` code deleted; portcullis.md §2 and §3.3 updated | `vm-console-check.sh`, `vm-workspace-check.sh`, `vm-artifex-check.sh`, the Sheets, Navigator and Mail cloud gates (`cloud-vm.sh test`), `vm-personas-check.sh`, `lyra-vm-check.sh` |

J2 changes the launch path that the Sheets, Navigator and VPN threads use
every day, so it lands in one push that the coordinator announces.

## 6. Decisions (2026-11-25)

1. jaild refuses an app uid that the launch registry does not bind to that
   app and owner (§3 item 1).
2. Workspace and console jails keep running as the jail's uid 0 for now,
   with no mounts, raw sockets or children; jaild allows that only for the
   `develop` and `terminal` classes. Running them as the person's uid is a
   separate change.
3. One path only: no side-by-side flag (§5).
