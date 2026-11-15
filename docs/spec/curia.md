# Curia: Settings

**Status:** accepted 2026-11-15, all four §6 decisions as recommended (Shankar: "Move it
to Arca"). Shankar chose (2026-11-14) to start Curia with a Network page, so a Wi-Fi
passphrase has somewhere to be typed. C1 built 2026-11-14 (§7), C2 built 2026-11-15 (§8).
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
| C3 | Devices page (dispensatord `CLASS_DEVICES`, `device-list`) | the device-assignment thread's Devices page check |
| C4 | further pages (Appearance, Sound, Displays, Apps' grants) | each with its own gate |

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
