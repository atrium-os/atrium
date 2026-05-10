# Atrium — naming reference

Canonical vocabulary for the Atrium platform. Every component in this table has a Latin / classical-architecture name and a clearly-defined scope. New components added to the platform should follow the same pattern.

## The OS and its core layers

| Component | Name | Latin meaning | Scope |
|---|---|---|---|
| **The OS** | **Atrium** | central courtyard of a Roman house | the integrated platform |
| **Display protocol** | **Fresco** | wall painting in lime plaster | retained-mode scenegraph protocol; what apps speak to render |
| **UI toolkit** | **Pergola** | ornamental garden structure with cross-beams | the framework apps grow widgets on; emits Fresco scenegraph messages |
| **IPC substrate** | **Aqueduct** | Roman water-distribution structure | OS-agnostic envelope + class registry + CAS upload that all Atrium services ride; portable across BSDs / Linux / non-POSIX |
| **Content-addressed filesystem** | **Tessera** | a single mosaic tile | per-file content-addressed dedup |
| **Kernel/userspace GPU ABI** | (unnamed; just "Atrium GPU ABI") | — | the boundary the Fresco server uses to talk to native FreeBSD GPU drivers |

## System services (daemons)

| Service | Name | Latin meaning | Role |
|---|---|---|---|
| **Jail launcher** | **Portcullis** | (medieval English) heavy iron gate | reads `atrium.toml` manifest, builds the jail, enforces capability gating |
| **System IPC bus** | **Castellum** | aqueduct distribution junction | message bus between system services and apps; capability-gated, per-slot ring shape |
| **Display manager** | **Vestibulum** | entry hall | login screen + session handoff |
| **Audio server** | **Lyra** | stringed instrument | per-stream submission, content-addressed sample buffers |
| **Clipboard** | **Tabula** | wax tablet | clipboard service; entries are CAS blobs, multi-format |
| **Notifications** | **Praeco** | town crier / herald | toast notifications + history |
| **Package manager** | **Opifex** | craftsman / maker | fetch + verify + install + rollback of jail trees |
| **Settings** | **Curia** | senate house | system + per-app settings store |
| **File manager** | **Scrinium** | document chest | jailed file picker + browser |
| **Shell (wallpaper + statusbar + dock)** | **Forum** | public plaza | the visible desktop chrome |
| **Persistent session service** | **Stoa** | (Greek) covered colonnade — public gathering space where people came and went | long-lived shell sessions on the host; clients attach/detach/roam, scrollback persists in Tessera; subsumes terminal emulator + remote-shell client |

## Foundation apps

User-facing apps don't take Latin names — they keep plain descriptive names with an `atrium-` prefix to namespace their binaries:

- `atrium-edit` — text editor
- `atrium-term` — graphical terminal (implemented as `stoactl-gui` against Stoa; see [`spec/stoa.md`](spec/stoa.md))
- `atrium-files` — file browser (built atop Scrinium)
- `atrium-image` — image viewer
- `atrium-pdf` — PDF viewer
- `atrium-clock` — clock widget
- ...

The user-facing display name can be unprefixed ("Edit", "Term", "Files"); the binary and package always use the prefix to avoid PATH collisions with traditional Unix tools.

## Cdevs and well-known paths

| Path | Owned by | Purpose |
|---|---|---|
| `/dev/fresco0` | kernel module | transport cdev for the Fresco protocol; jailed apps connect here |
| `/dev/atrium-gpu0` | kernel module | GPU memory + command submission (privileged) |
| `/dev/atrium-display0` | kernel module | modesetting + scanout (privileged) |
| `/dev/atrium-display-tap0` | future, capability-gated | read-only display capture |
| `/dev/atrium-gpu-compute0` | future, capability-gated | per-jail compute access |
| `/var/run/atrium/portcullisd.sock` | Portcullis | jail-management IPC |
| `/var/run/atrium/castellumd.sock` | Castellum | bus admin |
| `/var/run/atrium/{lyrad,tabulad,praecod,opifexd,curiad,scriniumd,vestibulumd,stoad}.sock` | respective services | service-specific |
| `/var/db/atrium/stoa/<user>/<sess>/` | Stoa | per-session metadata + WAL pointers (blobs live in Tessera) |
| `~/.local/share/atrium/apps/*.toml` | user | installed-app manifests |
| `/var/lib/tessera/cas/*` | Tessera | content-addressed blob store |
| `/etc/atrium/` | system | platform-wide config |

## Service users / groups

| User | Purpose |
|---|---|
| `_atrium` | core platform services (display server, Portcullis, Castellum, etc.) |
| `_tessera` | tessera-fs daemon if FUSE-shaped |
| jail UIDs | per-jail isolation; assigned by Portcullis from a configurable range |

Root is needed only for initial hardware bring-up; services drop to `_atrium` at the earliest opportunity.

## Wire-format constant prefixes

| Prefix | What it labels | Scope |
|---|---|---|
| `FRESCO_*` | Fresco protocol opcodes, completion types, blob types, status codes | protocol layer |
| `ATRIUM_*` | platform-level kernel ABI: GPU ioctl numbers, BO flags, display structs | kernel/userspace boundary |

So:
- `FRESCO_CMD_UPLOAD_DMA` (protocol opcode — what apps send)
- `ATRIUM_GPU_IOC_SUBMIT` (kernel ABI — what the server invokes)

These two layers are independent. Apps never touch `ATRIUM_*` names; the kernel never sees `FRESCO_*` opcodes (the server translates).

## Repository names

GitHub org: **`atrium-os`**. Repo names mirror service names where applicable.

| Repo | Contains |
|---|---|
| `atrium-os/atrium` | platform integration: docs, rc.d scripts, default config, `atrium-info` CLI, the umbrella |
| `atrium-os/fresco` | Fresco protocol implementation: server, libfresco, fresco-rs, fresco-text, examples |
| `atrium-os/fresco-spec` | wire-format spec + conformance tests (for vendors implementing Fresco) |
| `atrium-os/atrium-kmod` | platform kernel modules (Fresco transport cdev, GPU drivers, display, Tessera) |
| `atrium-os/portcullis` | jail launcher |
| `atrium-os/castellum` | IPC bus |
| `atrium-os/vestibulum` | display manager / login |
| `atrium-os/lyra` | audio server |
| `atrium-os/tabula` | clipboard |
| `atrium-os/praeco` | notifications |
| `atrium-os/opifex` | package manager |
| `atrium-os/curia` | settings |
| `atrium-os/scrinium` | file manager |
| `atrium-os/forum` | shell |
| `atrium-os/tessera` | CAS-FS userspace tooling |
| `atrium-os/atrium-edit`, `-term`, etc. | foundation apps |
| `atrium-os/freebsd-ports` | fork with Atrium ports added |

## Style guide

- **System services use Latin names.** They feel coherent and grep-able.
- **User-facing apps use plain descriptive names with `atrium-` prefix.** Avoids PATH collisions, doesn't burden users with vocabulary.
- **Cdevs and ioctls use `atrium-` / `ATRIUM_*` prefix.** They're platform-level, not protocol-level.
- **Fresco stays "Fresco" — it's the protocol, not the OS.** When in doubt, ask: is this about how apps render, or how the platform is configured? Rendering = Fresco, platform = Atrium.
- **Daemons end in `d`.** `portcullisd`, `castellumd`, `lyrad`, `tabulad`, `praecod`, `opifexd`, `curiad`, `scriniumd`, `vestibulumd`, `stoad`. The dock is a UI app, no `d` suffix needed for `forum`.

## How this reads to a user

```
$ atrium-info
  OS:           Atrium 0.1
  Kernel:       FreeBSD 16.0-CURRENT
  Display:      Fresco 0.1.0 via fresco-virtio-gpu
  Storage:      Tessera (12,847 tesserae, 4.2 GB unique, 18.7 GB referenced)
  Sandboxing:   Portcullis (3 jails active: edit-1.2.3, term-1.0.1, files-2.1.0)
  IPC:          Castellum, 8 services registered
  Audio:        Lyra
  Notifications: Praeco
  Package mgmt: Opifex (last sync: 2 hours ago)

$ doas service portcullisd start

$ ls /var/run/atrium/
  castellumd.sock   lyrad.sock        portcullisd.sock     vestibulumd.sock
  praecod.sock      tabulad.sock      scriniumd.sock       curiad.sock
  opifexd.sock      stoad.sock

$ pkg install atrium-edit
  Installing atrium-edit-1.2.3...
  Tessera: 47 new tesserae, 312 referenced.
  Manifest installed at ~/.local/share/atrium/apps/atrium-edit.toml.
```

Reads with personality, stays grep-able, and the vocabulary tells you what something does once you've learned the metaphor.
