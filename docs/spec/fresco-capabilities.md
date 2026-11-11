# Per-app capabilities in frescod

Status: **proposed** 2026-11-11, for review before any code.
Owner: Fresco thread. Prerequisite for exclusive gamepad grants and the
`input.*` gating in atrium-input.md §6.

## 1. Why

frescod decides who may do what in two ways today:

- **Reachability.** The window-management socket is mounted into a jail
  only when the app holds `window-management`
  (portcullis-jail `apply_window_management`), so a connection on that
  socket *is* the grant (socket_server.rs, portcullis.md §9.0).
- **Uid lists in the environment.** `FRESCOD_SLOT_UIDS`,
  `FRESCOD_NOTIFY_UIDS`, `FRESCOD_ACTIVATION_UIDS` and
  `FRESCOD_FORUM_UID` name system-service uids (limend, praeco, forum).

Neither can say "this app may hold a gamepad exclusively" or "this app
may lock the pointer". These are per-app, per-user grants on the
ordinary client socket, which every graphical app can reach. frescod
needs the lookup that Choragus, limend and Arca already do.

## 2. Identity: the platform's existing chain

No new mechanism. frescod does what Choragus does (choragus/src/daemon.rs
`serve_app`, grant.rs `GrantStore::load_portcullis`):

1. **Peer uid.** `getpeereid` on the client connection (frescod already
   records it for `WM_ENUMERATE`).
2. **App.** `portcullis_peer::AppRegistry` maps the uid (≥ 50000) to
   `(user, app_id, publisher)`. The registry
   (`/var/run/atrium/app-registry`) is mounted read-only into frescod's
   jail.
3. **Grant.** `portcullis_policy::Policy` at
   `<ATRIUM_POLICY_BASE>/<user>/policy.toml` gives
   `grants[app_id].capabilities`. Default-deny: no entry, no capability.

The result is a per-connection `Caps` bitset, computed once at accept
and recomputed on policy change (§6). Ops check the bitset; nothing on
the hot path touches a file.

**Embeds (Limen).** An embed-only connection's peer is limend, and the
embed's owner was named at hand-over. The embed's caps are its *owner
app's* caps, intersected with what an embed may ever have (§3, last
column). That keeps a jailed doc-viewer from inheriting its host's
gamepad grant.

**Split mode.** All checks run in frescod's front end. Renderers never
see or hold capabilities; they only draw.

## 3. Vocabulary and defaults

The manifest keys stay as atrium-input.md §6 wrote them. In TOML,
`input.gamepad = true` is a nested table, so portcullis-toml gains
`input: Option<InputCaps>` with one `Option<bool>` per key.

| Key | Meaning in frescod | Default for a `graphics` app | Granted how | Embeds |
|---|---|---|---|---|
| `input.keyboard` | key events while focused | **on, not gated** | implied by `graphics` | yes |
| `input.pointer` | pointer events over its windows | **on, not gated** | implied by `graphics` | yes |
| `input.gamepad` | pad events while focused | on if declared | install, no prompt | no |
| `input.gamepad_exclusive` | claim a pad regardless of focus (§4) | off | install consent prompt | no |
| `input.pointer_lock` | hide cursor, relative motion | off | install consent prompt | no |
| `input.raw_hid` | raw reports from a named device | off | restricted: policy approval only | no |
| `input.system_keys` | Cmd-Tab, Win and the like | off | never for apps; WM only | no |

Keyboard and pointer are not gated because a window that cannot get
them is useless, and focus already confines them. Gamepad while focused
has the same reach as keyboard, so it needs only a declaration, which
keeps apps that never use a pad from seeing one. The exclusive grant
and pointer lock outlive or escape focus, so they get a consent prompt.

## 4. Exclusive gamepad grants

The point of the exclusive grant is that a pad keeps going to one app
when focus moves: a game behind a chat window, or two apps on one
screen with a pad each.

- **Claim.** New op `PAD_CLAIM(device_id | ANY)` from a window. It
  succeeds only if the app holds `input.gamepad_exclusive`, the window is
  focused at that moment (user intent), and nobody else holds that pad.
  `ANY` takes the pad that most recently sent input.
- **While held.** The pad's events go to the claiming window whatever
  has focus. Other windows see neither events nor `CONNECT` for it.
  Today's follow-focus code (tactus_client.rs `pads_follow`) gains one
  rule: a claimed pad's target is its holder, not the focused window.
- **No stealing.** A second claim on a held pad fails. Only the user
  can move it, from Forum's controller menu or a long press on the
  Guide button. That ends the claim, and the pad follows focus again.
- **Release.** `PAD_RELEASE`, the window closing, the connection
  dropping, the pad disconnecting, or the grant being revoked. Release
  sends the usual held-button releases and a `DISCONNECT` to the holder,
  then the pad follows focus.

## 5. Pointer lock

Pointer lock is listed for completeness, because the same check guards
it. It is not built yet. When it is, the rules are: lock only while
focused; lose focus and the lock ends; the Escape key always ends it
and is never delivered while locked. frescod enforces this, not the
app.

## 6. Revocation and change

frescod watches `<base>/<user>/policy.toml` (kqueue `EVFILT_VNODE`) for
each user with a connection. On change it recomputes each connection's
`Caps`. A lost capability takes effect at once: an exclusive claim is
released as in §4, and a pointer lock ends.

## 7. Unregistered peers and development

A peer uid that is not in the registry is a system service, a
developer's unjailed run, or a test probe.

- It gets the ungated set (keyboard, pointer) plus `input.gamepad`,
  since that is no more than focus already gives.
- Everything else is denied, unless frescod runs with
  `FRESCOD_DEV_CAPS=all`. That variable is refused when frescod runs as
  `_frescod` in its jail, so a production frescod cannot be talked into
  it.

The VM checks (vm-tactus-check.sh, the probes) run unjailed, so they
keep working with no change. The exclusive-grant check adds a registry
file and a policy file to its scratch directory.

## 8. The environment uid lists

`FRESCOD_SLOT_UIDS`, `NOTIFY_UIDS`, `ACTIVATION_UIDS` and `FORUM_UID`
name system services, not apps, so they are not in any user's policy.
This spec leaves them alone. Folding them into Portcullis's
`system_services` manifest is a separate, later change.

## 9. Build order

- **C1.** portcullis-toml `InputCaps`, the `CapabilityDelta` and
  consent-prompt text, and `apply_graphics` mounting the registry
  read-only. Unit tests.
- **C2.** frescod `caps.rs`: peer → registry → policy → `Caps`, kept
  on the front end's client record; embed intersection; dev rule.
  Unit tests with a temporary registry and policy.
- **C3.** Gate `input.gamepad` (undeclared apps stop seeing pads), then
  `PAD_CLAIM` / `PAD_RELEASE` (wire ops plus split records) and the
  holder rule in `pads_follow`. Extend the GAMEPAD=1 VM check: window 1
  claims, focus moves to window 2, window 1 still gets the pad, window 2
  gets nothing; an ungranted claim fails.
- **C4.** The policy watch and revocation, with a VM check that edits
  the policy and sees the claim end.

The Forum controller menu and Guide long-press (§4) belong to the Forum
thread. Until they exist, ending the holder's app is the user's only
override.

## 10. Decisions for review

1. **Identity.** Use the Choragus chain (peer uid → registry →
   per-user policy) for per-app input capabilities, and keep
   reachability for window-management. *Recommended.* The alternative
   is a separate socket per capability, which stops working once a
   capability has to be checked per op.
2. **Defaults.** Keyboard and pointer are implied by `graphics`.
   `input.gamepad` is install-time without a prompt. Exclusive grant and
   pointer lock need a consent prompt. `raw_hid` is restricted.
   `system_keys` is never granted to apps. *Recommended as in §3.*
3. **Exclusive semantics.** Claim only while focused, no stealing,
   and only the user moves a held pad. *Recommended as in §4.*
4. **Unregistered peers.** Ungated set plus gamepad; the rest only with
   `FRESCOD_DEV_CAPS=all`, which a jailed `_frescod` refuses.
   *Recommended as in §7.*
5. **Embeds.** An embed gets its owner's keyboard and pointer only,
   never gamepad, pointer lock or raw HID. *Recommended.*
6. **Uid lists.** Leave the `FRESCOD_*_UIDS` lists as they are for now.
   *Recommended*, since they are system roles, not app grants.
