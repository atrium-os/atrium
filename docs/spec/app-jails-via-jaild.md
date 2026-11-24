# App, workspace and console jails through jaild

**Status: PROPOSED 2026-11-25** (codebase review H3, option (a), chosen by
Shankar 2026-11-24). Nothing here is built yet.

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

## 5. Order of work

Each step lands on its own and passes its gate before the next one starts.

| Step | What | Gate |
|---|---|---|
| J0 | jaild: `groups`, `JailInfo`, stdio `ExecInJail`, named device grants, `develop`/`terminal`/`app` classes; validator tests for each refusal | jaild validator tests on the host; the existing service gates still pass in a VM |
| J1 | `to_create_request` with the comparison test; no caller yet | host tests |
| J2 | consoles and empty consoles through jaild (`terminal` class) | `vm-console-check.sh` |
| J3 | workspaces through jaild (`develop` class) | `vm-workspace-check.sh`, `vm-artifex-check.sh` |
| J4 | apps through jaild, behind `PORTCULLIS_JAILD_LAUNCH=1` for a side-by-side week | Sheets, Navigator, Mail cloud gates (`cloud-vm.sh test`), `vm-personas-check.sh`, `lyra-vm-check.sh` |
| J5 | default on; delete the jail(8) lane, `render_jail_conf` and the `jls`/`jail -r` calls; update portcullis.md §2 and §3.3 | all of J2–J4 again |

J4 touches the launch path that the Sheets, Navigator and VPN threads use
every day, so it starts only when the coordinator says those threads are at
a pause. The flag lets either lane run without a revert.

## 6. Open questions

1. **The registry check (§3 item 1).** Should jaild refuse an app uid
   that the launch registry does not bind to that app? Recommended: yes.
   It costs one read-only file in jaild's jail.
2. **Root inside workspace and console jails.** Today these run as the
   jail's uid 0 (with no mounts, raw sockets or children). jaild would
   keep allowing that only for `develop` and `terminal`. Recommended: yes
   for now. Running them as the person's uid is a separate change.
3. **The side-by-side flag in J4.** Recommended: one week, then delete
   the old lane in J5.
