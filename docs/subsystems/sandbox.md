# Subsystem — Sandbox (Portcullis: jails + capabilities)

> See [NAMING.md](../NAMING.md) for component naming.

## Thesis

Every app runs in a FreeBSD jail. The jail is the security boundary, declared explicitly in the app's manifest. Apps can't escape; can't read other apps' files; can't see other apps' input; can't sniff network unless granted.

This is the **iOS / Android / macOS-App-Sandbox** model on a real Unix, with a kernel-level (not LSM-stitched) isolation primitive, made cheap by content-addressed file dedup.

## Why jails

FreeBSD jails (since 1999) are a kernel-level partition of the operating system. A jailed process sees:

- Its own process namespace (can't `kill -9` outside; can't `ps` outside).
- Its own filesystem root (chroot-strict; mounts visible only inside).
- Its own network stack (or shared with parent — configurable).
- Its own devfs subset (only devices explicitly exposed by `devfs.rules`).
- Its own UID/GID space (or shared, configurable).
- Resource limits via `rctl`.
- No access to host's syscalls that bypass the jail (`mount`, `reboot`, raw sockets, etc.).

This is **stronger than Linux namespaces+cgroups+seccomp** because it's a single kernel primitive, not three layered subsystems with subtle interaction bugs. It's been hardened in production for 25+ years (FreeBSD was used as the basis of much of the original BSD-jail-style virtualization research).

Capsicum (FreeBSD, since 2010) is a complementary capability-based syscall filter. Apps inside jails can additionally `cap_enter()` to drop further privileges.

## Capability manifest

Every app ships with an `atrium.toml` declaring what it needs from the host. Example:

```toml
[app]
name = "atrium-edit"
version = "1.2.3"
binary = "bin/atrium-edit"

[graphics]
needs = "fresco"             # opens /dev/fresco0

[filesystem]
read  = ["~/Documents", "~/.config/atrium-edit"]
write = ["~/Documents", "~/.config/atrium-edit"]

[network]
none = true                  # no network access

[devices]
none = true                  # no extra cdevs (graphics handled separately)

[ipc]
clipboard = true             # may read/write the system clipboard
notifications = true         # may post notifications
```

Capabilities default to **deny**. Anything not declared is forbidden by the jail.

The launcher (D2.5) reads this and constructs the jail accordingly: the right `devfs.rules`, the right mounts, the right network setting, the right rctl limits.

## Privilege boundary

The system has two privilege tiers:

### Trusted (privileged userspace, not jailed)

- `fresco-server` — owns GPU, input devices, scene tree.
- `portcullisd` — the jail launcher / supervisor. Reads manifests, builds jails, supervises their lifecycles.
- `castellumd` — IPC bus admin / policy daemon.
- `lyrad` — owns audio devices, mixes per-client streams.
- `tabulad` — clipboard service.
- `praecod` — notification daemon.
- `opifexd` — fetches CAS blobs, builds tree updates.
- `vestibulumd` — login screen, session handoff.
- `curiad`, `scriniumd` — settings store, file picker.

These run as a dedicated `_atrium` user (or root for those that need it), are not jailed, and form the small Trusted Computing Base.

### Untrusted (apps, jailed)

Every app: editor, terminal, browser, settings, file manager. Each in its own jail. All graphics goes through `/dev/fresco0` to fresco-server. All filesystem access is jail-scoped. All network goes through the jail's network setting (none / shared / dedicated).

## Inter-process channels

Jailed apps need to talk to the host for everything. The Fresco protocol handles graphics; other channels need their own protocols, all designed similarly:

| Channel | Service | Protocol |
|---|---|---|
| Graphics | `fresco-server` | Fresco protocol over `/dev/fresco0` (per-slot rings, content-addressed) |
| Audio | Lyra (`lyrad`) | similar pattern: per-slot rings, content-addressed sample buffers |
| Filesystem (file picker, drag-drop) | Scrinium (`scriniumd`) | request/response over Castellum; user grants per-file access; result is a path the jail can then access |
| Clipboard | Tabula (`tabulad`) | Fresco-style: clipboard is a CAS blob hash + format declaration |
| Notifications | Praeco (`praecod`) | request/response over Castellum; rate-limited |
| Settings | Curia (`curiad`) | per-app settings store; jails see their own slice |
| IPC bus | Castellum (`castellumd`) | shared transport library + admin daemon for the above |

The **Fresco-protocol shape** generalizes well: per-slot rings + content-addressed payloads + capability-gated submission + completion ring. Other system services should reuse this pattern.

## First launch UX

When the user installs an app, the launcher inspects the manifest and either:

1. Auto-grants commonly-acceptable capabilities (graphics, settings store, time/locale, etc.).
2. Prompts on suspicious capabilities (network, raw devices, broad filesystem access).

Subsequent launches don't prompt unless the manifest changed.

The trick is **not making this hostile**. Lessons from macOS / Android:

- Group capabilities into intuitive bundles ("media files", "internet access", "camera & microphone").
- Default-grant the boring ones; prompt only on the ones that matter.
- Capability *inspector* in settings — show the user what every installed app has.
- A "permissive mode" for development (skip all prompts, log every capability use).

## Capsicum integration

After jail entry, an app can additionally `cap_enter()` to drop syscall privileges within the jail. This is belt-and-suspenders: the jail prevents escape; capsicum prevents misuse of permitted resources.

Recommended for cooperative apps: capsicum-aware libraries open files via cap_fdopen, etc. Not required.

## Capability boundary examples

### atrium-edit

Capabilities: `graphics + filesystem(~/Documents)`. Nothing else.

- Open /dev/fresco0: yes (graphics declared).
- Read /etc/passwd: jail rules deny.
- Write /etc/passwd: jail rules deny.
- Connect to a TCP socket: `network = none` denies.
- Open /dev/audio: not declared, denied.
- Fork to /bin/sh: jail allows fork, but jail tree doesn't contain /bin/sh.

### atrium-term

Capabilities: `graphics + filesystem(~/) + network(none) + spawn(/bin/sh)`.

- Spawns a shell *inside the jail*. Shell runs in the jail. The shell's commands see the jail's filesystem (which is the user's home + whatever the manifest declares).
- The shell can't see other jails' files. So `cat /jail/edit/secrets` fails — that path doesn't exist in this jail.

### atrium-browser (future)

Capabilities: `graphics + filesystem(~/Downloads) + network(any) + audio + camera (with-prompt) + microphone (with-prompt)`.

Browsers need everything. The capability list is long but explicit. User can revoke individual capabilities post-install.

## Cross-app interaction

Apps don't interact directly. All inter-app communication mediates through:

- **Fresco protocol** (graphics + window manager events, including focus and click coordinates).
- **Tabula** (`tabulad`) for copy/paste.
- **Scrinium** (`scriniumd`) for file picking (privileged dialog hands path to jailed app).
- **URL handler** (Castellum-routed) for "open this URL in browser" intents.

This mirrors the macOS / Android model. It's restrictive but secure. Open question: how much "drag and drop between apps" works without explicit support — likely needs a dedicated drag-source / drag-target protocol like the clipboard one.

## What about Linux apps?

Linuxulator + a Fresco-X11 / Fresco-Wayland bridge can run Linux apps inside FreeBSD jails. Each Linux app runs in a jail just like a native Fresco app, but its graphics goes through a translation layer that speaks X11 or Wayland on one side and Fresco on the other. Performance penalty + compatibility lossage but viable.

This is **not** the architectural target — Linux apps don't get the dedup + capability + native-graphics benefits. It's a transition tool for ecosystem pull-through.

## Open questions

- **Service discovery.** How does an app find Tabula, Praeco, etc.? Well-known socket paths under `/var/run/atrium/` exposed into the jail by Portcullis based on the manifest's IPC capabilities; Castellum's library handles the lookup.
- **Multi-window apps and the manifest.** A browser opens many tabs as windows. The capability is on the app, not per-window. Fine.
- **Session-scoped vs persistent capabilities.** "Microphone allowed for this session only" — explicit and time-bounded.
- **Privilege escalation paths.** sudo-equivalent doesn't exist inside a jail. For settings that affect the whole host, a separate path (admin app, runs unjailed with a privileged channel).
- **Dev-mode escape hatch.** During development, apps need to debug, attach gdb, etc. Either dev-mode runs unjailed, or there's a "developer capability" that loosens restrictions.

## Why this is FreeBSD's strategic advantage

Linux has flatpak/snap, but the underlying isolation is namespace-stitching with bug-prone interactions. Linux has SELinux/AppArmor, but both are LSM bolt-ons that sysadmins find painful. macOS has App Sandbox, but it's closed, and you can't bring your own libc.

FreeBSD has had jails as a first-class kernel primitive for 25 years. Capsicum since 2010. **The infrastructure is already there** — what's missing is the user-facing model that makes it the default.

Atrium — Fresco + Tessera + Portcullis-jail-per-app — is that user-facing model. The substrate is mature. We're building the model on top.
