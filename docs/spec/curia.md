# Curia: Settings

**Status:** accepted 2026-11-15, all four §6 decisions as recommended (Shankar: "Move it
to Arca"). Shankar chose (2026-11-14) to start Curia with a Network page, so a Wi-Fi
passphrase has somewhere to be typed. C1 built 2026-11-14 (§7), C2 and C3 built 2026-11-15 (§8, §9), C4a the Display page built 2026-11-15 (§11), C4b the Sound page built 2026-11-15 (§12), C4c the Apps page built 2026-11-15 (§13), C4d the Bluetooth page built 2026-11-15 (§14).
**Scope:** the Settings app (`curia/`), its first page (Network), and the page model the
next pages (Devices, then the rest) follow.

## 1. Short answer

- Curia is one Pergola app in its own jail, a sidebar of pages and the page on the right.
  It is a system app: its manifest holds the restricted capabilities its pages need
  (`network-configure` now, `device-list` next), and Portcullis approves them for Curia only.
- A page is a module with a model (pure, tested on the host) and a view. It talks only to
  the service that owns its settings, over that service's Aqueduct class. Curia keeps no
  settings store of its own in v1: each service already stores its own (atrium-net its
  profiles, choragusd its levels, dispensatord its rules).
- **Network** (first): how the machine is online, the Wi-Fi networks around (scan on open
  and every 10 s while the page shows), join, the passphrase, saved networks (auto-join,
  priority, metered, address policy, forget), and the Wi-Fi country. Over
  `CLASS_NETCONF` ([atrium-net.md](atrium-net.md) §7.1, §10.5).

## 2. The shell

| Piece | Where | What |
|---|---|---|
| shell | `curia/src/shell.rs` | sidebar (page list, Up/Down, type-to-find), the page area, the window title; remembers the last page |
| pages | `curia/src/pages/<name>/{model,view}.rs` | one module per page |
| binary | `curia/src/main.rs` | window, event loop, threads for each page's service connection |

Pages run their service calls on threads and hand results to the model through a
channel, as the Forum bar does, so a slow service never stalls the window. A page whose
service is not reachable says so in place ("atrium-net is not running") and offers nothing.

## 3. Network page

- **Status** at the top: Ethernet / Wi-Fi with the network's name and bars / not
  connected; "No internet" or "Sign-in needed" (with the portal's address) when the probe
  says so; metered.
- **Nearby networks** (`wifi-nearby`): name, security, bars; a remembered one is marked.
  Joining a remembered or open one is one click. A new secured one opens the passphrase
  row under it (§4); Join is offered once something is typed. A refusal (wrong passphrase,
  out of range) shows on that row.
- **Saved networks** (`network-wifi`): each with Auto-join, Priority (a number), Metered,
  Address (per network / new each time / the device's own), Forget (asks first). Changes
  go to atrium-net at once (`WIFI_SET_PROFILE`).
- **Details** (`network-details`): per interface its addresses, router and DNS servers,
  folded by default.
- **Wi-Fi country** (`network-configure`): a list of countries. Static addresses and DNS
  servers come later (atrium-netconf answers "not built yet").

## 4. The passphrase

What the user types is a credential, so the rule of
[arca-autofill.md](arca-autofill.md) (decision 1) applies: it belongs in Arca's own
field, not in an app widget. Curia, atrium-netconf and atrium-net then never hold it:

1. Curia places a `CredentialField` configured with purpose `wifi` and the SSID. Arca's
   chip says "Wi-Fi passphrase for <name>". Typing goes to Arca's surface, and on Tab or
   Join Arca holds the value (`U_HOLD`) and gives Curia a handle.
2. Curia's `WIFI_JOIN` carries the handle instead of a passphrase. atrium-netconf and
   atrium-net pass it to aura-keys (`remember ssid= handle=`).
3. aura-keys, the machine store's only writer for Wi-Fi items, redeems the handle at
   arcad with a new service op that releases a held value only to the service its
   purpose names (`wifi` → aura-keys) and only once. It stores the item as today.

C1 used Pergola's `TextField::secret` and sent the passphrase in `WIFI_JOIN`; since C2
(§8) Curia has only Arca's field. `WIFI_JOIN` with a passphrase stays for
`atrium-netctl` (root, tests).

## 5. Build order

| Step | What | Done when |
|---|---|---|
| C1 | Curia shell, Network page with the interim secret field | in the cloud VM with real Wi-Fi (aura-sim-gate): the page lists aura-test, a typed passphrase joins it, Forget removes it; frames of each. **Built 2026-11-14** (§7) |
| C2 | Arca's field for the passphrase (§4): Limen `credential` purpose `wifi`, arcad redeem-for-service op, aura-keys `remember handle=` | the same gate with arcad and arca-ui running; Curia's memory never holds the passphrase (a canary passphrase not found in a core of Curia). **Built 2026-11-15** (§8) |
| C3 | Devices page (dispensatord `CLASS_DEVICES`, `device-list`) | the device-assignment thread's Devices page check. **Built 2026-11-15** (§9) |
| C4 | further pages: C4a Display (light/dark, text size, monitors), C4b Sound, C4c Apps' grants, C4d Bluetooth | each with its own gate. **C4a to C4d built 2026-11-15** (§11 to §14) |

## 6. Decisions

All four accepted as recommended, 2026-11-15.

1. **One app with pages, in one jail**, holding every capability its pages need.
   *Recommend yes.* Alternative: a process per page with only that page's capabilities
   (smaller blast radius, much more plumbing for v1).
2. **No settings store in Curia v1**; each page talks to the service that owns the
   setting. *Recommend yes.* A shared store comes when two services need the same setting.
3. **The Wi-Fi passphrase goes through Arca's field (§4), with the secret text field as
   C1's stopgap.** *Recommend yes.* Alternative: keep the secret field (simpler; Curia is
   a system app, but then a Curia bug can leak a passphrase).
4. **Network first, Devices second.** *Recommend yes* (Shankar's choice of Network first;
   the device-assignment thread waits on Devices).

## 7. C1 as built (2026-11-14)

- `curia/`: a Pergola app laid out by hand. `network.rs` is the page's model (`NetPage`:
  status, nearby list, saved networks, details, the network being asked about, the
  passphrase rules 8 to 63 printable characters, Forget asks twice) and speaks in
  `Want`/`Got`; `main.rs` runs each request on its own thread against atrium-netconf's
  sockets (`CURIA_NET_DIR`, default `/atrium/sockets`), follows status with
  `SUBSCRIBE`, and scans every 15 s while the window is open. A passphrase lives in the
  secret text field and the one `WIFI_JOIN` request; what comes back carries none.
- Page: status card (network, Disconnect), Wi-Fi networks (bars, Connected, security;
  picking a secured new one opens the passphrase row, a remembered or open one joins),
  Saved networks (Auto-join, Metered, Address policy, priority, Forget), details
  (interfaces and addresses).
- `atrium.toml`: `network-read`, `network-details`, `network-wifi`, `wifi-nearby`,
  `network-configure`, no network of its own.
- Live check: `sh scripts/aura-sim-gate.sh vms && sh scripts/aura-sim-gate.sh bar`, then
  `sh scripts/curia-vm.sh shot|click|type|frame|status` (RUNBOOK §13.6e). 2026-11-14:
  the list shows aura-test while connected; Forget, Forget it; pick aura-test, type the
  passphrase, Enter: joined, DHCP bound, saved with auto-join (frames
  `scratch/curia-{start,forget2,typed,joined}.png`, not committed).
- Not yet: a wrong passphrase reports only after the join's 60 s
  timeout (aura-keys does not yet say "wrong passphrase" early).

## 8. C2 as built (2026-11-15)

- Limen: `Purpose::WifiPassphrase`; Pergola `CredentialField::wifi_passphrase`. arca-ui
  offers nothing for it (no chip, no saved items) and holds what was typed with a
  trailing `HOLD_FOR_WIFI` byte on `U_HOLD`.
- arcad: a handle held that way is marked Wi-Fi. No courier, fill or file-manager save
  takes it (`Handles::take` and `item_of` refuse it). The service op `TAKE_WIFI` (0x0A,
  `handle[16]` → `bytes value`) redeems it once, only for the service named `aura-keys` in
  `ARCA_SERVICES`. Any other handle shown there is burned. A lock voids it, as with every
  pending value.
- `Join` gains `handle`. atrium-netconf sends `wifi-remember ssid= handle=`, atrium-net
  passes `remember ssid= handle=` to aura-keys, and aura-keys redeems it at arcad, checks
  it as a passphrase (8 to 63 characters, or 64 hex) and stores it as before. `handle=` is
  hidden in logs like `secret=`.
- Curia: the passphrase row is Arca's field (asked of limend while the row is open,
  detached after). Join pressed with the field focused takes the focus, Arca holds the
  value, and the handle that follows starts the join (`armed`, 3 s, then "Type the
  passphrase in the field first"). After a failed join the field starts afresh, since its
  handle was spent. Without limend the row has a field that can't be typed into. Curia
  never takes a passphrase itself.
- Live check (`sh scripts/curia-vm.sh arca` after `aura-sim-gate.sh bar`, 2026-11-15): forget
  aura-test, pick it, click Arca's field, type the passphrase, press Join. It joined and
  got an address by DHCP. The audit log shows `ui op=0x0063` then
  `service:aura-keys op=0x000a`. `curia-vm.sh core` finds the passphrase 0 times in a core of
  Curia (and 0 in arca-ui's), while the same check finds the page's own "Connected to
  aura-test" text (frames `scratch/curia-{pick,typed,joined}.png`, not committed).

## 9. C3 as built: the Devices page (2026-11-15)

- What dispensatord decided for every device (device-assignment.md §6.10), read-only:
  `curia/src/devices.rs` (`DevPage`) follows `CLASS_DEVICES` (`SUBSCRIBE`, then `LIST` on
  each new generation) at `CURIA_DEVICES_SOCK` (default
  `/atrium/sockets/device-list/device-list.sock`).
- Groups, in this order: **Not in use** (nobody's: no driver and no rule; what someone
  opening the page is after), **Atrium drivers** (a rule's jailed driver, running or not),
  **System services** (a rule's nodes in a service's devfs), then two folded ones:
  **FreeBSD drivers** and **Firmware entries with no driver** (ACPI children nobody took:
  a QEMU arm64 machine lists 33 virtio-mmio placeholders, a real one dozens of stubs).
  A row is the kind in words when the name or USB interface class says it ("Smart card
  reader", "Camera or phone (MTP/PTP)", "Disk"), the device's name, and who has it. A press
  opens the kernel's ids, where it sits, and the rule or reason behind its owner. The list
  scrolls (Pergola `VirtualList`). Without the service, the page says why ("dispensatord is
  not running", "only the person using this machine may see it").
- Sidebar: Network and Devices.
- dispensatord: the list was for root, the session's user and its session components.
  Now an app of the session's user whose installed manifest holds the restricted
  `device-list` may read it too (`DevicesHost::allowed`, as `removable-files` works for the
  file manager). Curia's manifest asks for it.
- Live check in the cloud arm64 VM, 2026-11-15, with the new dispensatord: Curia as `appui`
  (registry `org.atrium.curia` of alice, the active session) lists the machine's 68 devices.
  35 FreeBSD drives and 33 are firmware placeholders, both folded. Unfolding, scrolling and
  opening `pci0` work (frames `scratch/curia-{devices,scrolled,opened}.png`, not
  committed). `nobody` is refused (status 2).

## 10. Services going away (2026-11-15)

Every setting lives in its own service, so Curia must stay up and say what is wrong when
one goes away, hangs, or comes back (Shankar, 2026-11-15).

- Calls time out. `netconf::Client` sets a read timeout per op (join 90 s, nearby 30 s,
  others 10 s) and a timeout becomes `UNAVAILABLE "not answering"`; `CLASS_DEVICES`
  calls have 10 s. Subscriptions have no timeout; instead the follower waits at most 30 s
  for an event (`Changes::next_within`) and then makes a fresh call as a liveness check,
  dropping the stream and reconnecting if it fails.
- Network: while atrium-net is gone or hung the page says "Network settings are not
  available / Can't reach the network service; trying again" and recovers alone.
- Devices: "dispensatord is not running", "not answering" or "went away", then the list
  comes back by itself.
- The passphrase field: if arca-ui or limend dies, Curia drops the field's state, disables
  Join and says "The passphrase field is not available right now; trying again…". limend
  is reconnected every 5 s and the field asked for again at most every 2 s.
- Only frescod's loss ends Curia, with exit status 1, so its supervision restarts it.
- Live check `curia-vm.sh away` in the cloud arm64 VM, 2026-11-15, 11 steps: atrium-netconf
  killed and back, stopped with SIGSTOP and continued, dispensatord stopped and started,
  arca-ui killed and back, limend killed and back. Each time the page showed the right
  notice and recovered without help, and Curia's pid never changed.

## 11. C4a as built: the Display page (2026-11-15)

- The settings are forum-wm's (it remembers them in forum-displays.state and tells
  frescod), so the page asks forum-wm. Not over forum-ctl itself: that socket also
  focuses windows, reserves the bar's and dock's space and takes back pads. forum-wm now
  serves a second socket, `display-settings/display-settings.sock`, that takes only
  `forum_ctl::settings_intent`s (modes, outputs, place, UI size, mirroring, light/dark,
  text size, and the new read-only `GetAppearance` → `Reply::Appearance`) and answers
  "not a settings request" to anything else. A request has 5 s to arrive. Reaching it is
  the new restricted capability `display-settings` (Portcullis mounts the directory;
  Curia only); `window-management` mounts it too, for forum-wm to serve it.
- The page (`curia/src/display.rs`, `DispPage`): Appearance (Light, Dark), Text size (85% to
  200%), then each monitor: its name ("Display 1 (built-in, 14″)"), size and scale, UI
  size (Smaller, Default, Larger, 125%, 150%, 200%; a monitor's own default is sent as
  "unset", so a laptop panel keeps starting at Smaller), the resolution of the main
  monitor (forum-wm switches only that one's mode; one entry per size at its best
  refresh, and the monitor's native mode too when a faster refresh of its size would hide it), and for the others Place (left of, right of, above, below Display 1) and
  mirroring. A new resolution goes back after 15 s unless "Keep" is pressed, since a
  mode the monitor shows black can't be clicked away from. While the page shows, forum-wm
  is asked again every 3 s (a monitor plugged in, the bar's light/dark switch). Without
  forum-wm: "Display settings are not available / Forum is not running; trying again".
- Live check in the cloud arm64 VM, 2026-11-15 (`curia-vm.sh display`): Dark and 130%
  from the page change the whole session (the bar too); forum-wm killed, the page says
  so, and after its restart comes back with 130% kept. `forum-ctl` pointed at the settings
  socket gets `appearance`, `outputs` and `modes` answered and `surfaces`, `zoom` and
  `take-back` refused. That VM's frescod is headless and lists no monitor ("No monitor is
  connected").
- Mac VM, 2026-11-15, four monitors (three 1080p, one 1440p; frescod on gpusim): every
  monitor listed with name, diagonal, size and scale; UI size 125% and back; Change… to
  1280×720 went back by itself after 15 s, and stayed with Keep; Left of; Mirror Display 1
  and Stop mirroring; Light/Dark and text size reach Curia, the bar and the dock;
  `surfaces` refused on the settings socket. Two bugs found and fixed there: a mode switch
  dropped frescod's glyph atlases, so all text vanished (`3a32edc`: a rebuilt renderer keeps
  its textures), and a 120 Hz mode of the native size hid the native 60 Hz one (`dd9f278`).
- Fixed after that run: the page scrolls below its title (a `ScrollView`; at 720p or a
  large text or UI size the last monitors' rows ran off the window). Place on a monitor in
  the middle of a row (three or more in a line) used to move only that one, which frescod
  refuses without an answer ("touches no other"), so nothing happened and no error showed;
  forum-wm now puts the row (or column) in its new order and packs it again, and a move
  elsewhere that would strand a monitor is refused with a reason the page shows.
- Open: mirroring re-lays the other monitors from the remembered arrangement (a Left of
  done just before is lost), and the bar and dock follow the monitor at the desktop's
  origin, not Display 1. Both are forum-wm/frescod behaviour, not the page's.

## 12. C4b as built: the Sound page (2026-11-15)

- The settings are choragusd's (lyra-v1.md §7, lyra-spatial.md, lyra-hires.md §5), over its
  volume-control socket `audio-control/audio-control.sock`. Curia's manifest now asks for
  `audio-control` (restricted to Forum and Curia since A6). No new protocol: `List`,
  `ListOutputs` and the `Set*` messages; `Apps` and `Outputs` arrive unasked on each change.
- The page (`curia/src/sound.rs`, `SoundPage`): Volume (slider, percent, Mute; "Fixed"
  and no slider while the output's volume is fixed), Output (one button per device), and
  for the current output Balance (Left to Right, Centre), Speakers (Stereo, 5.1, 7.1),
  Headphone surround (only on stereo), Volume control (Automatic, In the mix, Device's own
  when it has a control, Fixed (line out)); then Apps: each app with a stream open, its
  level, Mute, and Surround on/off while headphone surround is on. An app shows only while
  it plays (that is what choragusd lists). Levels show at once and choragusd's next `Apps`
  confirms them. The page scrolls below its title.
- The connection (`follow_sound` in main.rs) has a 10 s limit on the hello; a quiet
  connection is asked for its outputs every 30 s and dropped as hung after 10 s more. Gone:
  "Sound settings are not available / choragusd is not running (or not answering, or went
  away); trying again", every 5 s. `lyra_protocol::client::Control::connect_within` is new.
- Found on the way, in Pergola: a `Slider` inside a group or scroll view read the drag's
  window point as if it were in its own coordinates, so it jumped to an end or didn't
  move. `Ctx::on_drag_local` now hands a handler points relative to the node's laid-out
  top-left (scroll offset and zoom undone, `slot::window_origin`), and the slider uses it.
- Live check in the cloud arm64 VM, 2026-11-15 (booted with `lyra-vm-check.sh qemu-args`,
  `cloud-vm.sh lyra`, then `curia-vm.sh sound`): lyrad on QEMU's HDA codec, choragusd, a
  tone from `lyra-play` as `org.test.a`, Curia as `org.test.ui`. Volume dragged to 49%,
  the tone's level to 21% and muted, balance to 0.7 and back to the centre, and Fixed
  (line out) on and off, each read back from `/var/db/choragus`; choragusd stopped (the
  page says so), restarted (it comes back), stopped with SIGSTOP ("not answering" after
  about 40 s) and continued (it comes back); Curia's pid unchanged.

## 13. C4c as built: the Apps page (2026-11-15)

Shankar chose (2026-11-15) "Approve and remove": the page lets the person allow an app
what it asks for, as the prompt at its first launch would, but only after a real press
that frescod confirms; restricted capabilities stay the system policy's alone.

- **Who may ask.** A new restricted capability, `app-grants` (Curia only), mounts
  portcullisd's socket (as `app-launch` does). portcullisd gates the three new verbs on
  the caller's peer uid: the launch registry names the app, and its INSTALLED manifest
  must hold `app-grants`. They act on the person that app was launched for (the
  registry's owner), never on the caller's own uid: Settings changes alice's policy
  because it runs for alice. `Grant`/`Revoke` keep their old rule (the peer's own
  policy), so no other app can reach a person's policy this way.
- **`Access`** answers every installed app (trees whose manifest names another id are
  left out): its name, everything it asks for in the launch prompt's words (with what it
  opens), what is not allowed yet (`decide` against the person's and the system's policy,
  so "Manifest has changed since last grant" after an update), who allowed it (`You`,
  `System`, or nobody), when the person did, and the restricted capabilities it asks for
  (`Capabilities::restricted()`: the system-only set less `net-fetch`,
  `protected-media` and `status-item`, plus `tessera-cas-read`, `usb-hid`,
  `audio-monitor`, `input.raw_hid`, `input.system_keys` and the `terminal` syscall class).
- **`ApproveAccess { app_id, activation }`** writes the person's grant for the installed
  manifest's capabilities and hash, exactly what the launch prompt's Allow Always writes.
  Refused when the app asks for anything restricted, when it already has what it asks
  for, or when the manifest fails the launch's trust check. Last, portcullisd spends the
  press with frescod under a new purpose, `APPROVE` (11): a click, or Enter, in the asking
  app's own focused window, minted by frescod from real input; a refusal above doesn't
  spend it. Curia passes the token of the press on Allow
  (`pergola::clipboard::current_activation()`), so nothing but a press on Allow allows.
- **`RemoveAccess { app_id }`** removes the person's grant; the app asks again at its next
  launch. The system's grants can't be removed from here. An uninstalled app's leftover
  grant can be removed too.
- **The page** (`curia/src/apps.rs`, `AppsPage`; sidebar after Devices): "N apps, M not
  allowed yet", then a row per app with its status ("Allowed by you on 2026-11-15",
  "Part of the system: allowed by the system", "Asks when it opens", "Updated: asks
  again when it opens", "Needs the system's approval; it won't open"), Allow when the
  person may allow it, and Remove access when they have a grant, which asks once more
  ("Remove what you allowed Player? It asks again when it next opens."). A row opens to
  list what the app asks for, marking what is not allowed yet, and what only the system
  can allow. A refusal shows under the row in portcullisd's words. The list is asked for
  when the page shows, every 10 s while it does, and after each change; portcullisd away
  shows "The apps' settings are not available; trying again".
- Live check in the cloud arm64 VM, 2026-11-15 (`vm-apps-access-check.sh` after
  `cloud-vm.sh up`): portcullisd, headless frescod, three signed apps installed with
  opifex (New asking for the clipboard and notifications, Sys asking for
  `audio-control` and granted by the system, and Settings trimmed to `graphics` and
  `app-grants`), Curia launched for alice. Root over the socket is refused `Access` and
  `ApproveAccess` (not a launched app); the page lists the three for alice; a click on
  New's Allow writes her grant and New then launches without asking; a click where
  Sys's Allow would be writes nothing; Remove access, then Remove, takes the grant
  away. 11 checks, 0 failed; frames in `scratch/apps-access/`.
- Seen whole (Shankar, 2026-11-15: "add it for allow"): frescod now watches every
  top-level window as it watches credential slots and Sigillum's sheets (wholly on
  screen, nothing composed above it overlapping it, at least the minimum size, where it
  was last frame), and an `APPROVE` press counts only if Settings' window was so for
  `credential::STILL_NS` (half a second) up to the press; otherwise `OBSCURED`, which
  portcullisd words "the window was covered or had just moved; press again". Ordinary
  windows count for `APPROVE` alone: `FILL` and `IDENTITY` still need a slot or a sheet.

## 14. C4d as built: the Bluetooth page (2026-11-15)

- Over vinculumd's control socket (bluetooth.md §13), `bluetooth-control/control.sock`,
  behind the new restricted capability `bluetooth-control` (Curia and the Forum bar; it
  mounts that socket directory and nothing else). Curia says `Hello { agent: true }`: it
  is the pairing agent, so pairing prompts are drawn by Settings and nowhere else, and
  with Settings closed vinculumd refuses to pair (§7 there). No new protocol.
- The page (`curia/src/bluetooth.rs`, `BtPage`; sidebar second, after Network): the
  adapter ("Bluetooth is on · This computer is …", or not ready), Find devices / Stop
  looking, the pairing prompt when one is open (Confirm: "Does Mouse show 123 456?" with
  Pair and Cancel, or "Pair with Mouse?" when there is no number; Display: "Type 035714 on
  Mouse", Done; EnterPasskey: a code field, six digits, Pair and Cancel), a line for what
  went wrong or "Paired with Mouse", Paired devices (name, Connected or not, "paired
  without a code check", "typing not allowed"; Connect or Disconnect; Forget, which asks
  once more), and while looking, Devices nearby (name or "Unnamed device (address)", a
  signal word, Pair; one pairing at a time; devices already paired are not offered). At
  most 40 found devices are kept. vinculumd answers each request once, in order, so the
  model counts what it asked to match each `Ok` or `Error` (a refused Pair ends that
  pairing). Status and the paired list are asked for when the page shows, every 10 s
  while it does (2 s while the adapter is starting), and after a connect, disconnect or
  pairing.
- The connection (`follow_bt` in main.rs): every 5 s while vinculumd is away ("Bluetooth
  is not available / vinculumd is not running; trying again"); a quiet connection is
  asked for its status after 30 s and dropped as hung 10 s later.
- Not yet: Classic devices (keyboards, headphones) are not found by vinculumd's LE scan;
  `PairClassic` by address has no place on the page yet. Power on/off and per-device
  profiles wait for vinculumd to offer them. Deploying vinculumd on Atrium, its
  `--control-uid` must admit Settings' per-app uid (or vinculumd learns to check the
  launch registry and manifest, as choragusd does for `audio-control`).
- Live check on the Linux host, 2026-11-15 (`scripts/curia-bt-check.sh`): Rootcanal,
  vinculum-hci, vinculumd with its control socket, and a Bumble peer; the page model
  (`curia/examples/bt_drive.rs`) finds the peer, pairs it answering the prompt as the
  page's buttons would, sees it in the paired list connected, disconnects, connects
  again with the stored bond, and forgets it after asking. Three peers, one per prompt
  (numbers to compare; a code Settings shows, typed on the device; a code the device
  shows, typed into Settings): 0 failed. The drawn page is checked by the host render
  test; no VM run, as the cloud VM has no Bluetooth controller.

