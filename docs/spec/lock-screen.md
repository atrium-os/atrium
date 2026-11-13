# Lock screen

Status: built 2026-11-13 (§4), not yet gated in a VM. Shankar asked for "unlock with a security key on
the lock screen"; there was no lock screen. This fills in vestibulum.md §2.2 (session
unlock) with what each part does. Sigillum's side is sigillum.md §12 and §16.6.

## 1. What it is

Locking hides the session's windows and its input, and shows the lock screen until the
session's own user verifies again. Apps keep running (vestibulum.md §2.2). Removable media
is unmounted while locked (device-assignment.md §4.6, already built on the lock file).

## 2. Who does what

| part | job |
|---|---|
| Forum (forum-bar) | a **Lock** button; sends `{"op":"lock"}` to ostiarius's lock-only socket, `/atrium/sockets/ostiarius-lock/lock.sock`, which the new `session-lock` capability mounts (restricted: the bar only). |
| ostiarius | owns the lock. `lock` writes the seat's lock file (`/atrium/sockets/ostiarius/session-locked`, naming the user), tears down the old login screen's jail and launches vestibulum with `--lock USER`; if that launch fails the lock is lifted again (nothing could unlock it). `unlock { password }` or `unlock_with_key` clear it. The lock socket passes a `lock`, and only that, on to the control socket as ostiarius's own uid, which the control socket lets do `lock` alone; only vestibulum (owned by `_login`) may do anything else. |
| frescod | enforces it. While the lock file names the active user, it composes and routes input to only the lock screen's windows (vestibulum's uid) and Sigillum's sheets; every other window is off screen (out of the z-order, not hit, not focused), the WM's background is not drawn, gamepad claims pause. |
| vestibulum (lock mode) | "Locked: {user}", the password field, **Unlock**, and **Use a security key**, which sends `unlock_with_key`; Sigillum's sheet then asks for the key (or the passcode). It exits once ostiarius answers ok. |
| sigillumd + sigillum-ui | the key ceremony and its sheet, as for every `VERIFY_USER` (§16.6). |

frescod enforces the lock, not Forum, because Forum is the user's session: a lock that a
compromised or crashed Forum could lift would be no lock. frescod reads the lock file
ostiarius writes; it does not trust any client to tell it the screen is locked or unlocked.

## 3. frescod, in detail

- `Compositor::set_lock(Some(uids))` engages: every window whose owner uid is not in
  `uids` (an unresolved owner, 0, is never in it) leaves the z-order and is marked hidden;
  their pop-ups are dismissed; focus moves off them. Windows created while locked start
  hidden. `set_lock(None)` puts them back in the order they had, and slots follow their
  parents. A window the lock hid is never shown by a raise, a focus change, a slot bind or
  `keep_shown` while the lock holds, since those act only on windows in the z-order or
  refuse hidden ones.
- Each frame frescod reads the lock state (polled at most every 100 ms) and calls
  `set_lock`. The allowed uids are vestibulum's (the launch registry's uid for
  `org.atrium.vestibulum` owned by `_login`) and the sheet uids
  (`activation::sheet_uids`).
- Window 0 (the WM's background) is drawn blank while locked; the pointer over it goes
  nowhere.

## 4. As built (2026-11-13)

- `fresco-scene-server/src/window/lock.rs` (`set_lock`, `locked_out`, the lock's keyboard
  window) and its tests: the session hidden and restored in order; nothing (raise, focus,
  slots, new windows, windows whose uid arrives late) shows a session window while locked;
  no allowed window means keys go nowhere.
- `frescod/src/lock.rs` (the lock files, polled every 100 ms; `FRESCOD_SEAT` overrides the
  seat path), the frame loop (`set_lock`, no window 0 while locked), `socket_server::route`
  (input to the owner alone while locked, nothing to the WM), gamepad claims of hidden
  windows paused.
- ostiarius: `lock_screen`, `control::Access`, `spawn_lock_socket` (`OSTIARIUS_LOCK_SOCK`,
  `none` off) and tests (the lock screen launched, the session may only lock, a failed
  launch lifts the lock, the lock socket passes on `lock` and nothing else).
- vestibulum `--lock USER`; forum-bar's Lock icon; `session-lock` in portcullis-toml,
  -policy and -jail (mounts `ostiarius-lock/`), tests.
- Not yet run: the FreeBSD tests of frescod and ostiarius, and a VM gate with frescod up
  (lock, check the session is not composed and gets no keys, unlock with a key).

## 5. Not in this step

Auto-lock on idle and on lid close; a lock keyboard shortcut; fast user switching from the
lock screen; showing the time and notifications on it.
