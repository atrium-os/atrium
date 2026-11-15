# Curia: Settings

**Status:** accepted 2026-11-15, all four §6 decisions as recommended (Shankar: "Move it
to Arca"). Shankar chose (2026-11-14) to start Curia with a Network page, so a Wi-Fi
passphrase has somewhere to be typed. C1 built 2026-11-14 (§7), C2 and C3 built 2026-11-15 (§8, §9), C4a the Display page built 2026-11-15 (§11).
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
| C4 | further pages: C4a Display (light/dark, text size, monitors), then Sound, Apps' grants | each with its own gate. **C4a built 2026-11-15** (§11) |

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
  refresh), and for the others Place (left of, right of, above, below Display 1) and
  mirroring. A new resolution goes back after 15 s unless "Keep" is pressed, since a
  mode the monitor shows black can't be clicked away from. While the page shows, forum-wm
  is asked again every 3 s (a monitor plugged in, the bar's light/dark switch). Without
  forum-wm: "Display settings are not available / Forum is not running; trying again".
- Live check in the cloud arm64 VM, 2026-11-15 (`curia-vm.sh display`): Dark and 130%
  from the page change the whole session (the bar too); forum-wm killed, the page says
  so, and after its restart comes back with 130% kept. `forum-ctl` pointed at the settings
  socket gets `appearance`, `outputs` and `modes` answered and `surfaces`, `zoom` and
  `take-back` refused. That VM's frescod is headless and lists no monitor ("No monitor is
  connected"), so the monitor rows are checked only by the host tests so far; a run on
  a machine with monitors (the Mac VM) is still to do.
