# Atrium — workstream status

What is being worked on right now, where it stands, and where to read more.
This is the index; the specs and the run book hold the detail. How to run
anything named here is in [RUNBOOK.md §13](../RUNBOOK.md#13-cloud-and-remote-workflows-2026-11).

**Last refreshed: 2026-11-05 03:10 UTC** (HEAD `da65060`). Kept current by the
project's docs thread; if a row is wrong, the commit log wins — fix the row.

Commit hashes are atrium unless prefixed: `fbsd:` = atrium-os/freebsd-src
branch `atrium-os`, `gpusim:` = atrium-os/gpusim.

## At a glance

| Workstream | State | Latest | Next |
|---|---|---|---|
| [Native RDNA4 backend + gpusim](#1-native-rdna4-gpu-stack) | R9700 oracle v19: **54/54 jobs, 82 MATCH, 0 MISMATCH**; bench v5 ours ≤ RADV | spilling stage 3 `4bf3191`; VA reuse `2dac848` | bench v7 staged on the box (atrium16), waiting for an idle GPU |
| [GPU driver isolation (F1/F6)](#2-gpu-driver-isolation) | F1 done; F6 G1–G4 done (watchdog, VM-fault → EIO, read-only binds, app GPU channel, hostile-ISA fuzz); `CAP_ISA` (ABI rev 4) | `622693d`, `bb3f7ba`, `8023c92`, `a8a595d` | G5; silicon run of `test_gpu_isolation` before a production grant |
| [Fresco display pipeline](#3-fresco-display-pipeline) | compute is the default renderer; split mode on by default, renderers jailed as `_fresco-render`; F2, F3, F4a–F4e and F4g done; F4h per-client scene budget (refuse with `EV_BUDGET_REFUSED`, not a renderer kill), F4i shipped frescod refuses unsplit/dev modes (gate `fresco-production-gate.sh`, PASS in VM), F4j GPU compose never waits on a window raster + H6 cost refusal; noisy-neighbour gate on compute 0 failed (2026-11-14); F5 deferred | `3ccac5d`, `1455488`, `e7c3187`, `ad77c13`, `9fd041f`, `5ecb954`, `3487cac` | F4f (secure password input); F4j on gpusim and R9700 (`FAR_OVER` from measured time); noisy-neighbour on the GPU renderer |
| [Compressed memory](#4-compressed-memory) | zram v2 increments 0–6 done; design A (swap fast path) **kept**, B shelved | spec `3da0acb` | — (closed 2026-11-04) |
| [Navigator security](#5-navigator-threat-model-fixes) | P0/P1 rows verified on FreeBSD; jail limits verified; P2 byte budget, link activation, image decoding in the worker done | `b0927c0`, `4995101`, `7b74432`, `ceead6a` | credentials C0 (vault + per-origin fetcher) |
| [Navigator exploration coverage](#6-navigator-exploration-coverage) | covered handler elements 298 → 324 → **754 (45.2 %)** with sibling templates (recording format /4) | `78c50e9`, `3e98a25`, `493f5d6` | `el.id` at load; 6 pre-existing replay failures |
| [Navigator document lane](#9-navigator-document-lane-normalizer--renderer) | corpus layout work: grid, flex, tables, RTL, UAX #14 line breaking | grid/box-alignment batch `36a341f`…`d715d05` | keep driving corpus flags down (thread "Fix Navigator corpus render flags") |
| [Navigator backend](#10-navigator-backend) | M3 forward/back passed; M4 jailed save/restore passes in a real jail | `a168de8`, `0841678`, `b9edfe3` | — |
| Navigator UI ([navigator-ui.md](spec/navigator-ui.md)) | accepted 2026-11-13 (all D1–D13); step 1 (static shell, `navigator-ui/`, stand-in trail store, page as a placeholder slot) built 2026-11-13 and run on split-mode frescod + forum-wm in the VM (`scripts/navigator-ui-vm.sh`): the page is the security unit (one jail each), pages arranged in trails (a tab that keeps its path and branches) in a left rail, pages off screen parked by Save/Restore, a presence control for whether a site knows you, a tier indicator (Static, Replayed, Live code) in the Where field and the strip, islands framed by frescod; plus a security light, status bar and per-trail sound. Pergola mockups on headless frescod (`navigator_mockup`, project files `navigator/ui/`). Step 2 (one live page) built 2026-11-13 and driven in the VM (`scripts/navigator-live-vm.sh`, 9/9): `navigatord` daemon (fetch + one-shot converter, page tickets, link checks against the recording, origin over frescod's chrome channel), `navigator-page` doc-viewer child (Limen L6, DocumentView with links), frescod's strip naming a focused page by its host window. Not yet confined in that run: page child and converter start as ordinary processes (`limend-dev`, `--unconfined-for-testing`), pages from fixtures. Step 3 (trails on disk) built 2026-11-13, same script 19/19: `navigator_ui::disk` trail store (records only, no page content; unrecorded trails never written; 0600, atomic, debounced; bounded and validated per trail), Earlier with times, 30-day expiry and Forget; trails and Earlier survive a navigator-ui restart. Step 4 (parking, static pages) built 2026-11-13, same script 25/25: one page child per live page, hidden children swapped in (9 ms TCG), budget `NAVIGATOR_UI_LIVE` (default 6), least recently seen parked via Limen `Park`/`Parked {scroll}` (68 ms); navigatord content-addressed page store (`NAVIGATORD_STORE`; Unrecorded in memory only), parked page reopened by saved address at its scroll offset (2.7 s cache / 3.4 s store, TCG). Step 5 (presence and personas) built 2026-11-13: a jar per open (Personal, or one random jar per Unrecorded trail; Arca partition `u-<hex>.<site>`), the presence control reopens the page with `knows`, navigatord fetches it through Arca's couriers with broker tickets (navigator-cookies) and keeps it private (memory only), Forget this site / an Unrecorded trail closing / navigatord's start forget jars via new arcad `W_FORGET`; Unrecorded jars live in arcad memory only, never on disk, gone on lock; Step 6 (islands and prompts) built 2026-11-14: prompt row press rule (500 ms still and uncovered, restarts when it moves or the UI's own popovers cover it), one prompt at a time per page with a small queue, one a second, no repeat of a dismissed one; downloads told from documents by their bytes in navigatord (`files.rs`), held in memory, saved on a press 0644 under a safe new name with `atrium.origin` recorded; hand-offs (`NAVIGATORD_SCHEMES`, `atrium-app` always) asked first and passed once to `NAVIGATORD_OPENER`; frescod `slot::kind::ISLAND` with an amber frame and `live code: <origin>` tag, no pointer input while covered (dashed), Limen role `web-island`; `scripts/arca-vm-test.sh` §8 against the real Arca (98/98), live script 28/28. Step 7 (reading controls, find, history, keyboard map, accessibility) built 2026-11-14: Aa as a frescod pop-up window (text size, zoom, theme, Full/Comfortable width) remembered per persona and site in the trail store and applied by the page child without refetching; find (Ctrl-F) with counts and a rect from the child, never text; history view (Ctrl-H) by day with the page withdrawn; Ctrl-F/H/plus/minus/0 and Escape; the child hands unused shortcuts back (limen `Key`) and can give focus to the chrome (frescod `OP_WINDOW_FOCUS_PARENT`); accessibility tree in atrium-ax roles (`navigator_ui::ax`, logged with `NAVIGATOR_UI_AX=1`); live script 53/53 (2026-11-14); a press in the page gives it the keys (frescod `embed_focus`, the renderer's `FocusParent` to frescod in split mode, forum-wm focusing an embed's window via `WmEnumerateReply::embeds`), find marks cover a run's last glyph. Confined 2026-11-14 (`scripts/navigator-confined-vm.sh`, 25/25, renderers jailed too): page child as a signed bundle started by the real limend through portcullisd, one jail per page as `_navpage` (65035), its one capability the restricted `navigator-page` (navigatord's ticket socket); navigatord without the testing switch, converter and fetcher jails, pages over a hermetic network | `74fcbe2`, step 1 `1757915c`, step 2 `79d86da`, step 3 `fa4a7ae`, step 4 `66e35f3`, step 5 `0ab6189`, step 6 `5af082d`, step 7 (this commit) | no atrium-ax daemon or Pergola publisher; F6; history Fetch-the-current-version and clearing by day/site; images and in-page anchors in the child; restore times on the Mac (HVF) and R9700 to set the live budget; broker-session Save/Restore for replayed pages; idle expiry as park; presence menu (account, Sign out here), the site asking (SiteNeedsIt), named personas; frescod activation.rs MOD_SHIFT/MOD_CTRL bits look swapped against the HID modifier byte (check); jailed download viewer, app publisher and capabilities in the app prompt (Nomenclator, Opifex), a real hand-off handler, other programs' windows over the prompt row, the engine jail that produces islands |
| [x86_64 port](#7-x86_64-port) | A done, C done (amd64 VM, Tessera root, gpusim, navigator e2e); E done 2026-11-12: Atrium boots on the R9700 box bare metal (dual boot with Ubuntu, [r9700-bare-metal.md](spec/r9700-bare-metal.md)), GPU discovery table read on silicon; GPU stage 3 done 2026-11-12 (PSP sOS, 20 microcode images via the PSP, SMU features on, RLC booted) | `801a762`, `6273d0d`, `108098c`, `991db65`, `03715a5`, `0b46901` | GPU power P2 (GC must idle on its own; [gpu-power-management.md](spec/gpu-power-management.md)), then stage 4 (GMC, IH, MES, first dispatch) on silicon; box root moves to Tessera after GPU bring-up (Shankar 2026-11-12) |
| Arca secrets ([arca.md](spec/arca.md), [arca-autofill.md](spec/arca-autofill.md)) | A0 store + jailed arcad, arca-ui credential field, A1 courier, a jail per courier (§5.4) and A2 cookies with broker tickets (§8.7) and the machine store for system services (§6.4, Bluetooth bonds) pass the VM gates (`scripts/arca-vm-test.sh`) | `3d3c9b2`, `2c1c488`, `d11c217`, `992ab5e` | machine key TPM-sealed (A4); session cookies per view; A3 passwords; courier Capsicum; TLS hardening list in [TLS.md](TLS.md) |
| Device discovery and driver assignment ([device-assignment.md](spec/device-assignment.md)) | accepted 2026-11-13, all §8 decisions as recommended: keep FreeBSD newbus/devmatch/devd, add one jailed-driver policy layer in its own daemon `dispensatord` (typed devevents, `devices.d` match table, default deny, per-node grants); D1 + D2 built 2026-11-13 (§6.1): typed devctl events, device registry, fuzz target; `atrium-dispensatord` with `devices.d` rules took over portcullisd's identity-device agent (same ops on the old traces) | spec; D1/D2 see §6.1 ; S2e gate passed on dispensatord; D3 built (§6.2): `driver` rule class, Lyra's sound cards and Tactus's PS/2 nodes per device, wildcards gone from rulesets 24/26 (VM gate passed); D4 groundwork built (§6.3): `usb` rule class by interface class or vendor:product with optional kernel-driver detach, jaild `devfs_grants` for several nodes per device (`ugenB.A` + `usb/B.A.0`) ; usb gate `vm-usb-check.sh` passed 2026-11-13 (uaudio detach + nomatch CCID + restart); D5b (§6.4): `capsa-fat` FAT12/16/32 read-write (fsck.fat-clean), exFAT read, fuzz target; `atrium-capsa-fuse` speaks the FUSE protocol itself, tested under a real kernel FUSE mount; jaild mounts fusefs from outside the jail (`fuse` request, pinned `fuse_devices`) and hands the server fd 3/4; dispensatord `disk` rule class for partitions; gate `vm-fuse-check.sh` passed 2026-11-13; auto-mount (§6.5): USB-disk partitions served at /media for the active unlocked session only (0700), down on lock/logout/switch, eject socket; shipped rule + pin 31 + `_capsa`; `vm-automount-check.sh` passed 2026-11-13; sticks in Forum (§6.6): Aqueduct class `media` (list, eject, added/removed events) from dispensatord, forum-bar USB icon with Eject, capability `removable-media` (bar only), gate passed 2026-11-13; sticks with no partition table served whole (§6.7); exFAT read-write (§6.8), fsck.exfat-clean and read back by Linux; Tessera sticks read-only through the core's reader (§6.9); D6 Devices list (§6.10): every device, its owner and why one has none, Aqueduct class `devices`, capability `device-list` (Curia), `--devices`; usb gate 0 failed; apps' access to sticks (§4.8, §6.11): one-shot grants of a folder (or the whole stick for the file manager) into an app's jail, served by the stick's own server under the app's uid; the file manager's standing grant of every stick (capability `removable-files`, `ATTACH_ALL`) and one file handed over as a descriptor (`OPEN`); automount gate 0 failed | D4 rule files and pins for Vinculum controller + CCID, with B1b and S3; the file manager and picker are built (Scrinium row); Curia's Devices page; §4.7 catalog lookup once Opifex has a catalog |
| Scrinium, file manager and system file picker ([scrinium.md](spec/scrinium.md)) | built 2026-11-14: Pergola file manager in its own jail with every stick (standing grant, `removable-files`): places, crumbs, list, keys, new folder, rename, delete, copy/cut/paste without overwrite, eject; the system file picker as Limen's `picker` role, now a system role provided by Scrinium alone, shown as a sheet over the asking app (new slot kind `sheet`: frescod places it, dims the app, holds its pointer input and sends its keys to the sheet, frames it with a `system: <app>` tag; Shankar 2026-11-14): a file comes back as an open descriptor (limend `EMIT_FD`, regular files only, Capsicum-narrowed, read-only unless asked), a folder as a mount of that folder alone in the app's jail (dispensatord `PICKED`, restricted cap `file-picker`); gate `vm-scrinium-check.sh` on the cloud amd64 VM (frescod headless, limend, two jails, a FAT32 stick): 0 failed, with the sheet | `b902ee1`, sheet (this commit) | network shares accepted 2026-11-14 ([network-shares.md](spec/network-shares.md), all §9 decisions as recommended; S0 done: mount options, fusefs fuzzer `vm-fuse-fuzz.sh` (first find: a killed lookup through a symlink panics INVARIANTS kernels, [review](reviews/2026-11-14-fusefs-namei-erestart.md)), `share-server` syscall profile; S1 SFTP next: jailed user-space SFTP/WebDAV/SMB 3.1.1 servers, credentials in Arca released after the server's identity check); home folders (Scrinium's jail has none), open with, thumbnails, search, drag and drop, trash, "always allow" in Curia, frescod activation proof for picks |
| Office suite, Officina ([officina.md](spec/officina.md)) | accepted 2026-11-14 (all 17 §17 decisions as recommended). **O1 built 2026-11-14** (§16.1): `officina-core` (container, journal, undo) and `officina-grid` (column-chunked storage, formula parser, 90 functions, range dependency graph, parallel and vectorised recalc, undoable ops, native file v0), headless. Gates: 152 conformance cases, incremental = full on 1,000 random workbooks, 100 M cells in 781 MB, 2 M fuzz inputs clean | this commit | O2: `GridView` + 2D ScrollView + frescod two-axis shift, `atrium-sheets` with CSV and Save As; Fresco §10.5 gaps built (F4h–F4j, see Fresco row); depends on Scrinium Save As and bookmarks (§4.2, §4.3), an IME framework |
| Wi-Fi, Aura ([wifi.md](spec/wifi.md)) | accepted 2026-11-13, all §10 decisions as recommended; threat model §8: own Rust kmods per chip family (MediaTek MT7921AU USB + MT7925 first, Intel second), own split MAC instead of net80211 (kernel data path, management frames in jailed `aurad`), own Rust supplicant (WPA2/WPA3 + PMF), simulation-first testing via `aura_sim` + Linux hwsim/hostapd | W0 done: `aura/` (aura-proto, aura-wlan-core, aura_wlan.ko, aura_sim.ko), VM gate `scripts/aura-vm-check.sh` 24/24. W1 done 2026-11-13: jailed aura-mlme + aura-keys (`aura/`), credentials in Arca, join hostapd WPA2-PSK + PMF on hwsim, DHCP, rekey, forged deauth refused. W2 done 2026-11-13: WPA3-SAE (H2E + hunting-and-pecking), transition mode with Transition Disable, per-network MAC, roaming, compiled-in wireless-regdb; gate `scripts/aura-sim-gate.sh w1|w2` | W3 `aura_mt79` on the MT7921AU dongle, AI X1 Pro (MT7925) |
| Host network config, atrium-net ([atrium-net.md](spec/atrium-net.md)) | accepted 2026-11-14, all §9 decisions as recommended: privileged daemon that parses nothing + jailed Rust DHCP/RA reader per interface, local-unbound resolver, IPv6 SLAAC with privacy addresses, Wi-Fi profiles and auto-join, new Aqueduct class CLASS_NETCONF | N0 done 2026-11-14: `atrium-net/` (daemon in capability mode, Capsicum-sandboxed Rust DHCPv4 parser `atrium-lease`, §5.2 checks, local-unbound forwarders); gate `scripts/atrium-net-gate.sh n0` 14/14 incl. a hostile DHCP server. N1 done 2026-11-14: Wi-Fi profiles + auto-join drive the Aura gates (aura-ctl + dhclient retired there), bad-link handling (§4.1: beacon loss, 60 s address grace, fast reconnect, failing-network skip, metered hotspots), W2 tunnel scenarios | N2 IPv6, N3 multi-interface + connectivity probe |
| Identity verification, Sigillum ([sigillum.md](spec/sigillum.md)) | accepted 2026-11-13, all §17 decisions as recommended; S1 built (CTAP core, soft key, driver logic, broker state machine); **S2 done (§16.2)**: HID classification, hidraw hidden from tactusd by default, one jailed driver per key started by portcullisd's agent (via `/dev/u2f/N` on FreeBSD 15+, else hidraw), the `sigillumd` daemon, frescod's `IDENTITY` press and the `sigillum-ui` sheets; amd64 VM gate `scripts/vm-sigillum-check.sh` 0 failed (register + signed sign-in through the jailed driver and broker with QEMU's emulated key, isolation, hot-plug); **S3 built on host (§16.3)**: `sigillum-webauthn` (the WebAuthn client Navigator's broker will link: origin → RP ID, clientDataJSON, response JSON) and Arca as the platform authenticator (arcad `P_*` ops, sheet lists passkeys first), verified end to end by a relying-party check; **S4 built on host (§16.4)**: passcode checks through ostiarius (own socket, PAM via the broker, lockout), passkey UV from the passcode, Arca verify-each-use items served through `VERIFY_USER` | `7e325c3` (spec) | Navigator's broker calling `sigillum-webauthn` (Navigator not built yet); S4 on the VM with real PAM; CTAP2 on a real key (YubiKey, Mac VM); S5 smartcards (Precision InnaIT DSC token) |
| Nuntius, the system fetcher ([nuntius.md](spec/nuntius.md)) | steps 1–3 built: streaming, retries, Fd delivery, the `nuntius` crate, `nuntiusd` on an Aqueduct channel; step 4's `OpenFetcher` and the `net-fetch` capability built and checked jailed on FreeBSD (`scripts/nuntius-vm-check.sh`, 20/20); the standalone player uses it when it holds `net-fetch`; per-app `net-fetch-sites` limits | `e4bc24d`, `3823d7e`, `4c8fedf` | confirm the fetcher's start in `open()`; private-network grant (needs a permissions UI); C ABI |
| FreeBSD kernel security and Rust ([review](reviews/2026-11-07-freebsd-kernel-security-rust.md)) | scoping done 2026-11-07: 188 advisories 2020–2026 classified (59% of kernel ones memory-safety; 2026 surge from AI-assisted finds); recommends no rewrite, incremental path. Step 0 hardening ([kernel-hardening.md](spec/kernel-hardening.md)) boots and passes its check on arm64 and amd64; step 1 kmod audit ([kmod-audit](reviews/2026-11-07-kmod-audit.md)) fixed V1–V5, G1, T1, Z1, C1, J1; step 3 Rust kmod spike ([README](../kmod-rust-spike/README.md)): differential test clean on both, KASAN and 1 h syzkaller clean on arm64; step 4 phase 1 (D3): Tessera's on-disk decoders are Rust in `tessera_fs.ko` and the host core, differential-tested against the C ([rust-decoders.md](../atrium-tessera/docs/rust-decoders.md)) | data in `docs/reviews/data/`; `f187dca`, `9a19a20`, `c550dbb`; fbsd:`2a61d3635`, fbsd:`489ba6d7a` | D2 decided (Rust by default for new kernel code, 2026-11-10); D3 phase 2 (kmod pack readers, userspace tools, B-tree node edits, iterative put, superblock region check and bounded free space done 2026-11-11; encoders and journal/pack writers reviewed and left in C; cross-language LTO builds and inlines, Rust within 0.4% of C on host instructions; HVF kernel timing: Rust decoders cost nothing measurable vs C); audit items G2, G3 and V6 closed 2026-11-11 |
| Bluetooth, Vinculum ([bluetooth.md](spec/bluetooth.md)) | accepted 2026-11-07: new user-space Rust stack (not netgraph), jailed transport / host / audio, HID to Tactus, A2DP to Lyra; usable on stock FreeBSD as a port | B0 done; B1 host side done: `vinculum-host` (sans-I/O), `vinculum-hci` relay with command allowlist, `vinculumd`; `harness/e2e.sh` connects to a Bumble peer through Rootcanal and does an ATT MTU exchange; B2 part 1: `vinculum-smp` LE Secure Connections initiator wired into the host, e2e pairs with Bumble (Just Works, numeric comparison, passkey) and reconnects with the stored key; `vinculum-keys` sealed file store + Arca machine store client, bond survives a restart; pairing policy (pairable window, no downgrade, keyboards need MITM); `bluetooth-control` socket + `vinculum-ctl`; Classic SSP (SC only, AES-CCM, 16-byte keys); B2 done; B3: L2CAP channels, SDP client, Classic HID host + page scan, HID hand-off to a real tactusd in e2e (keys arrive, keyboard-initiated reconnect works); B4: `vinculum-gatt` HID over GATT (mouse motion/clicks to tactusd, battery, automatic LE reconnect); e2e 11 scenarios; 8 fuzz targets | B1b (USB via libusb, Intel/Realtek firmware, jails + services.d) needs a USB dongle (UB500 + Raytac nRF52840 ordered); VM gates for B3/B4; battery in the Forum bar; B5 audio |
| Per-jail syscall filter, sysfil ([jail-sysfil.md](spec/jail-sysfil.md)) | kernel filter (syscalls, ioctls, sockopts, families; log/deny/kill; inherit-and-narrow) in the fork, 14/14 ATF + stock jail tests pass on amd64; profiles base/renderer/app/networked/terminal close 27 of 44 app-reachable kernel advisories (30 with core dumps off); jaild and both Portcullis launch paths apply the manifest's class; shipped in log mode | fbsd:`93437440e`, fbsd:`5cb2b284b`; `69c9acf` | <1% gate on HVF/KVM; arm64 run; learn.sh on the real apps, then deny mode; upstream submission (jail(8) docs) |
| [Cloud dev loop](#8-cloud-dev-loop) | arm64 + amd64 FreeBSD VMs under TCG in cloud sessions | `scripts/cloud-vm.sh` | — |

## 1. Native RDNA4 GPU stack

Our own RDNA4 (GFX12) Vulkan/compute backend, no Mesa. Compiler layering is
frontend IR → GIR (`atrium-gir`) → RDNA4 machine layer (`atrium-rdna4-compiler`).
Command streams in `atrium-rdna4-cmd`, ISA tables in `atrium-rdna4-isa`
(MIT XML), oracle harness in `atrium-rdna4-oracle`.

- **Hardware truth:** every compiler change is checked on the R9700 (Linux,
  amdgpu as transport) and on gpusim. Reports:
  `/mnt/project-files/graphics/rdna4/reports/atrium_r9700_report_vN.txt`.
  - v14: Fresco bin + raster pixel-exact vs the Rust reference.
  - v16: first scratch use (spill stage 1).
  - v17: found that `DISPATCH_DIRECT` dims are exclusive *end* ids when
    `COMPUTE_START` is nonzero → fixed `4c9f6b2`, gpusim `60bb84e`.
  - **v19: 54 jobs, 82 MATCH, no mismatches** (includes LDS-alloc calibration).
- **Bench v5** (quiet box): ours ≤ RADV on every case; 1M-node glyph_run 374 µs vs RADV 526.
- **Spilling:** stages 1–3 done (scratch ST mode; demand-driven spill + remat;
  SGPRs into VGPR lanes, `4bf3191`). Design doc in the artifact linked from
  project memory `rdna4-spill-design`.
- **gpusim** (`~/src/gpusim`, separate repo): compute-only RDNA4 core built
  from public sources only (ISA guide + MIT headers). Draws use a SIM
  fixed-function rasterizer, so graphics-pipeline (M4) work has nothing to run on yet.
- Docs: [spec/atrium-rdna4-vulkan-backend.md](spec/atrium-rdna4-vulkan-backend.md),
  [spec/atrium-gpu-ir.md](spec/atrium-gpu-ir.md),
  [spec/rdna4-vulkan-subset.md](spec/rdna4-vulkan-subset.md),
  [GRAPHICS-STACK.md](GRAPHICS-STACK.md) (current map).
- Roadmap after this: graphics pipeline → ray queries → full RT, with Orbis as
  the driving workload.

## 2. GPU driver isolation

`atrium-gpu-amd` against GPU ABI v2.1 ([spec/atrium-gpu-abi-v2.md](spec/atrium-gpu-abi-v2.md),
header `include/atrium/gpu_abi_v2.h` is normative).

- **F1 done** (`73a94ec`): the five isolation breaks from
  [reviews/2026-11-03 §A3](reviews/2026-11-03-gpu-kernel-and-tier2-review.md)
  are closed. Map in [atrium-gpu-amd/ISOLATION.md](../atrium-gpu-amd/ISOLATION.md).
- VA reuse, reject overlapping/misaligned binds (`2dac848`).
- VRAM: a real device allocator (`vram.c`) that frees VRAM and scrubs it with a kernel-owned GPU fill (`clear.c`) before anyone else can have it; sized from the device, small-BAR aware; isolation part F (`89a0237`, gpusim `8c6f712`).
- **Client layer in Rust** (2026-11-11, [atrium-gpu-amd/rs/README.md](../atrium-gpu-amd/rs/README.md)):
  ioctls, VM/BO/syncobj fds, VM_BIND and per-open state are Rust; the hardware backend stays C
  behind `backend.rs`. G1 is now enforced by the type (binding table inside an `SxLock`).
  gpusim amd64, NODEBUG and KASAN+WITNESS: suite ALL OK, isolation 71/71, same as the all-C module; no KASAN report. KPI patches 0006–0009 (0008: amd64 Rust KASAN shadow offset). Doorbell grants (the mmap device pager, fault handler, revoke) also Rust (`rs/doorbell.rs`, KPI 0009), same results on both kernels.
  Queue management (ownership, submit, wait/hang, reset, `rs/queue.rs`) and deferred BO frees (`rs/defer.rs`) moved to Rust 2026-11-11, same results. Submit's argument checks are Rust too (the backend gets the ring's VA; the command stream is deliberately not parsed, Option A). Per-uid BO caps (`rs/quota.rs`, audit G2): EDQUOT past 75% of VRAM or 50% of RAM per uid, isolation part G. The completion lists and hang clock (`rs/pending.rs`) followed; irq.c is now only the two ISR entry points. Next: R9700 run when the GPU is quiet.
- jaild policy lists atrium-gpu-amd as **experimental (passed, not granted)**;
  a silicon run of `test_gpu_isolation` is required before production.
- Per-queue completions and a hang watchdog (`hang_ms`, default 5000) close
  review §A2.2/§A2.6; lost work fails with EIO; Bus Master is cleared on
  shutdown so warm reboots are clean (with gpusim `bf008f1`) (`622693d`).
- **F6, the app GPU path** ([spec/gpu-app-path-f6.md](spec/gpu-app-path-f6.md)):
  - Option A recorded: app machine code is untrusted and confined by its VMID (`a8587af`).
  - G1: a VM fault fails that queue's work at once; read-only binds, ABI rev 3 (`bb3f7ba`, isolation Part D `e48e89b`).
  - G2: frescod's GPU channel (`FRESCOD_GPU_SOCK`), one context and VM per connection, arch-neutral (`4cf6395`, review fixes `5f6bd11`).
  - G3: `atrium-gpu-app` checks every field and writes every PM4 packet itself (`6dcd221`).
  - G4: hostile machine code (infinite loop, wild stores) is lost with EIO while other apps' work completes (`a8a595d`). With gpusim's async doorbell (gpusim `3d31e61`) the watchdog fails a spinning kernel after ~5.2 s instead of ~20 s (`7686038`).
  - `CAP_ISA` (ABI rev 4): the ISA family comes from the driver, not the vendor string (`8023c92`).
  - jaild stanza re-attested at each driver change (latest `e572532`), still experimental.

## 3. Fresco display pipeline

Plan D1–D5 in [reviews/2026-11-04-fresco-display-pipeline.md](reviews/2026-11-04-fresco-display-pipeline.md);
compute route chosen; security conditions F1–F6 in
[spec/atrium-navigator-threat-model.md §2.5.4](spec/atrium-navigator-threat-model.md).

| Item | State | Commit |
|---|---|---|
| D1 display ABI v2 on gpusim (`include/atrium/display_abi_v2.h`, fenced commits, vblank pacing) | done | `4205742` |
| D2/D3 `fresco-compute` bin + raster kernels, Tier-2 matches reference; `FRESCOD_RENDERER=compute` | done | `ce1d804`, golden `6ca05f0` |
| GPU renderer: `FRESCOD_RENDERER=gpu` draws via RDNA4 kernels on gpusim; architecture behind `fresco_compute::gpu::Arch` (RDNA4 the one built in) | done | `7c1ec1b` |
| `compute` is frescod's default renderer (Vulkan opt-in); gpu renderer arch from `CAP_ISA` | done | `1455488`, `da65060` |
| GPU renderer scroll: a scrolled viewport's pixels move in VRAM (`shift.comp` via a scratch buffer, two passes), then only exposed tiles are rastered ([fresco-scene-groups.md §4](spec/fresco-scene-groups.md)) | done on gpusim; oracle `fresco_shift_matches_the_cpu_scroll` equals the CPU renderer's scroll bit for bit; queued in the R9700 corpus; live run 2026-11-11 (`scripts/vm-gpu-scroll.sh`): frescod on gpusim, whole-pixel and fractional scrolls both give the compute renderer's screen bit for bit; the shift frame ran 17M wave instructions vs 95M for a redraw, compose now composes only what changed since each scanout buffer last saw the window (a log of the last 8 changed rects per window; whole window past it): scroll compose 2.5 -> 2.0 s on gpusim (a scroll changes the whole viewport), small changes cost their rect; still bit-exact with the compute renderer. a scanout buffer that lacks the current window list starts as a DMA copy of the buffer composed last (no more whole-screen compose on a buffer's first frame; only when the window list changes) | `e9c903e`, gpusim `38a0524` |
| Exact group opacity (`ISOLATE`, [fresco-scene-groups.md §2.4](spec/fresco-scene-groups.md)): `KIND_PUSH`/`KIND_POP` ops, a per-pixel layer in the raster kernel, no extra buffer | done, four levels exact since 2026-11-12 (GIR `dce::carried` drops loop-carried temporaries: raster 243 → 107 VGPRs at one level, 137 at four, no spill); reference, Tier-2, GIR, fuzzer, frescod pack test, gpusim `fresco_isolate_layers_match_the_reference`; Vulkan renderer ignores it | `95f5f8a`, gpusim `b035966` |
| F4e one GPU VM per window (gpu renderer); compose maps surfaces read-only | done | `e7c3187` |
| F4g compute renderer: each client's jailed renderer rasters its windows, frescod composes pixels only | done | `9a01b97` |
| Damage tracking (only dirty tiles re-rastered; 15 s → 0.4 s/frame on gpusim) | done | `57035bc` |
| F2 bounds hardening + differential fuzzer | done | `9323062` |
| F4a per-window surfaces + compose kernel | done | `a20492f`, `797a3e5` |
| F4b one jailed renderer per client (portcullisd jail, `_fresco-render`; frescod as `_frescod`) | done, on by default with compute/gpu; frescod starts renderers via portcullisd and fails closed; fonts + 512 MiB cap in the bundle; 32 connections per uid | `a46ec90`, `0a864bd`, `a43f23f`, `3754439`, `3ccac5d` |
| F4c input routed by the composed layer list; window 0 = WM | done | `1c87e2d` |
| F4c′ click-through on alpha < 16 (anti-clickjacking) | done | `2cf9231` |
| F4d trusted chrome: 24 px origin strip only frescod draws, focus ring | done | `a9bf150` |
| forum-wm lays out below the strip (`DISPLAY_INFO.reserved_top`) | done | `11984d2` |
| F3 broker fuzzing (`atrium-split-fuzz`, `scripts/fresco-split-fuzz.sh`; broker never blocks on a renderer) | done | `9d8429a` |
| App GPU channel in frescod (F6 G2–G4) | done | `4cf6395`, `a8a595d` |
| Tactus input layer T1–T2 (atrium-input.md rev. 2): jailed tactusd (`_tactus`, devfs 26) reads hidraw and serves frescod; frescod's own readers and its hidraw nodes removed | done (`a12f6dd`); key repeats to apps and T3 touchpads (host tests) done; `scripts/vm-tactus-check.sh` 0 failed; T6 Bluetooth HID done (Vinculum e2e with real tactusd passes, Classic and LE/HOGP; Battery messages from vinculumd not yet used); T4 PS/2 part 1 done (atkbd0 + psm, tactus-console hands the keyboard over, needs fbsd:34a110a9b, VM check 0 failed); gestures routed (pinch to apps as EV_INPUT_POINTER_MAGNIFY, 3/4-finger swipes to Forum as EV_INPUT_SWIPE: workspace switch); T5 gamepads done (pads follow keyboard focus, EV_GAMEPAD_* 0x0706–0x0709, split records; `GAMEPAD=1 scripts/vm-tactus-check.sh` 0 failed: stand-in Bluetooth pad, jailed tactusd, split-mode frescod); `pergola-gamepad` standard names (A/B/X/Y, sticks, triggers, D-pad; Sony and generic mappings, dead zones) checked in the same run; touchpad native packets (T4 part 2, needs a laptop); exclusive grants built with per-app capabilities (fresco-capabilities.md C1–C4), and the user's override (C5: Forum bar controller menu / `forum-ctl take-back`, Guide held 1 s; `TAKEBACK=1` and `GUIDE=1` checks 0 failed) | `a12f6dd` and the commit after |
| Fresco pop-up windows (fresco-popups.md): menus, drop-downs, tooltips as frescod-placed windows that reach past their parent (flip, slide, shrink to the monitor; grabbing pop-ups take the keys; an outside press dismisses the chain and is swallowed) | steps 1–2 done: protocol (OP_POPUP_CREATE/PLACE, EV_POPUP_PLACED/DISMISSED), compositor, split records, fresco-client `popup_create`/`popup_place`; `scripts/vm-popup-check.sh` 0 failed direct and split. Bar security (fe61c19: app pop-ups below the strip, Chrome above app pop-ups, one open chain, Chrome role only for the bar/dock launched as session components, marked `session` in the launch registry by portcullisd; FRESCOD_CHROME_UIDS for tests). Step 3 done: Pergola Menu/Dropdown/attached MenuButton (c77291e, 91912a5); the Forum bar's controller menu is an attached drop-down, run live with a fake pad: `BAR=1 TAKEBACK=1 CLAIM=1 GAMEPAD=1 scripts/vm-tactus-check.sh` presses the icon and "Take back" (0 failed) | the pop-up commit |
| Forum status items (forum-status-items.md): windowless bar apps (VPN, clipboard, mixer) put an icon in the bar; the app describes a menu or a panel (text, groups, rows/columns, toggles, sliders, choices, buttons, progress) and the bar draws it under the app's name | menu (4a23ab6) and panels (30d8238) done: Portcullis `status-item` capability, forum-wm status socket (registry-checked), bar drawing; checked live in the amd64 VM with forum-ctl/examples/status-item.rs. Bar window now 38 px (bar drop-downs had been refused live). The bar's own volume drop-down (mute, level, output over choragusd audio-control) checked live: `scripts/vm-bar-volume-check.sh` 0 failed. Text fields done (checked live: typed text and Enter reach the app). Vector icons from the app's bundle done (live: the sample's icon switches with its state). App-drawn panels (§4) done: the bar's one-use grant (OP_POPUP_GRANT), the app's claim (OP_POPUP_CLAIM, Pergola `WindowDesc::hosted`), lit icon via EV_POPUP_HOSTED, frescod's label naming the app (a pill above the panel, anti-aliased Plex Mono baked at build time); live in split mode `scripts/vm-status-drawn-check.sh` 0 failed | 30d8238 |
| F4f secure password input to frescod | waits on F5 + Navigator C0 | — |
| F5 | deferred (Shankar) | — |
| Display lease: only frescod's jail and uid may open `/dev/atrium-display0` (jaild writes the lease; devfs ruleset 23 pinned to frescod) | done; host root and other jails get EPERM in the VM | `1441356` and the jaild commit after it |
| Flip on the in-fence signal (the display watches its in-fence syncobj; a failed render drops the commit and fails its out-fence) | done; fenced async flips land 8–11 ms after the signal in the VM, before they waited for vblank (18 ms) | `c613251` |
| Recovery from a device reset: `NOTE_RESET` kqueue event + `STATUS_CONTENT_LOST` (GPU ABI v2.1 §10.3); frescod re-presents at once, gpu renderer falls back to Tier-2 for 5 s | done; screen back within 2 s on both renderers in the VM (black without frescod) | `7cc2939`, `6e2b92d` |
| D4 real DCN 4.0.1 scanout/flip/vblank registers in gpusim + display kmod (modeset still `regDISP_*`) | done; display tests, `atrium_gpu_test`, `test_gpu_isolation`, jailed frescod pass on gpusim | gpusim `3f37036`, `e3b0658` |
| Modes, EDID and hotplug (display 2.1): shared EDID/CTA-861 parser + per-driver hooks (`atrium-display/`), any listed mode via OTG timing, `NOTE_HOTPLUG`; frescod switches mode on a WM request or hotplug, forum-wm re-lays out, `forum-ctl mode WxH[@Hz]` | done on gpusim; `display_modes` 7/7 in the VM | gpusim `a140c6a`, `192a47a`; atrium `aeca87e` and after |
| Variable refresh (display 2.2): range from the EDID, `COMMIT_VRR`, OTG `V_TOTAL_MIN/MAX`; frescod turns it on when the monitor's range covers the mode | done on gpusim; `display_vrr` in the VM | gpusim `b972fb2`; atrium `display 2.2` commits |
| Several monitors (display 3.0): OUTPUTS, monitor identity, atomic COMMIT_MULTI with bandwidth check and rollback, per-output events; gpusim 4 pipes + shared bandwidth budget; frescod one desktop with settle/grace and remembered arrangements; per-client VRAM reuse | done on gpusim; `display_multi` 6/6, frescod hotplug cycles 8/8 in the VM. Forum: per-monitor layout, window rescue/restore by monitor, `forum-ctl outputs/arrange/move-monitor/scale`; per-monitor scale from pixel density with `EV_WINDOW_DPI_CHANGED`; arrangements and scales kept per user across restarts. Chaos test (`scripts/vm-multimonitor-chaos.sh`, 60 random plug/unplug/dropout/burst/arrange steps) 0 failures. Mirroring (`forum-ctl mirror`): same-size or scaled, pixel-exact against the source in the VM | gpusim `3334c3c`..`678361c`; atrium `00ed75f`..`3542dc6` |
| Game spanning (display 3.1, [spec/fresco-span.md](spec/fresco-span.md)): span groups with timing lock + flip lock (`COMMIT_SYNC_GROUP`), one VRR range per group, a zoom over a Forum span group, bezel gaps, `EV_WINDOW_SPAN` tiles, 30 s dropout grace and flap hold-out | done on gpusim; `display_span` 7/7 (100 of 100 group flips in one vblank), `scripts/vm-span-e2e.sh` 0 failures (zoom over three monitors, direct scanout, dropout, flapping + notice, gone, un-zoom, 11 mm bezel). `forum-ctl span/spans/span-app/zoom`, bezels in px or mm. Direct scanout of a pixel-mode game (strip 3 s after focus changes), flapping notice via Praeco, `atrium-span-probe` + `span-probe-check.py` (all members continuous across the bezels), `vm-span-chaos.sh` 60 steps, 0 failures. Not yet: a GPU-rendered game's surface | gpusim `aac2b96`, `52fe4c7`; atrium `d8a9096`, `64bf1d7`, `8e99b77`, `6867070` |
| HDR (display 2.3): HDR caps from the EDID, HDR ioctl (PQ / HLG, BT.2020, static metadata, from the next commit), XRGB2101010 scanout, infoframes via VPG; frescod HDR10 desktop with `FRESCOD_HDR=1` | done on gpusim; `display_hdr` 6/6 in the VM | gpusim `ec7a71d`; atrium `display 2.3` commits |
| Video layers ([spec/fresco-video.md](spec/fresco-video.md)) V1–V4: shared-memory NV12/P010/RGBA8 layers composed under the window, converted in the client's renderer jail; present times, `PRESENTED`, VRR content rate; colour (BT.601/709/2020, range, BT.2390 tone map); protected flag. Embed-only connections for Limen children (S1) | done on gpusim, both modes; `atrium-video-probe` 28/28 headless, `--play` on the flip path with the reference frame on scanout. HDR video tone-maps on HDR monitors for now; passthrough needs a wider compose path (later, Shankar 2026-11-06) | `947dc9b`, `951a636`, `893900e`, `f8a9f22`, `c29d71d` |
| D5 silicon display (inherit GOP mode first) | not started (needs native boot on the R9700; it has no monitor) | — |

Slice detail: [spec/fresco-f4-split.md](spec/fresco-f4-split.md). The whole display stack as it stands: [GRAPHICS-STACK.md](GRAPHICS-STACK.md). Screenshots
in `/mnt/project-files/graphics/frescod-*.png`.

## 4. Compressed memory

Decision (2026-11-03): in-kernel compressed memory; GPU-VRAM overflow rejected.
Spec [spec/atrium-zram-v2.md](spec/atrium-zram-v2.md).

- Increments 0–6 done: swap-free notification (fbsd:`da0b30649`), packed pool,
  per-CPU codecs, lazy table, backing store + writeback, LZ4 option (`c964309`).
- Native §6 bench on the R9700 (KVM): swap+GEOM overhead 46.5 % zstd / 68.6 %
  LZ4, over the 25 % trigger.
- **Design A** (swap fast path, fbsd:`482ce93dd`, spec §6.1): zstd 41.0 → 18.3 %
  (passes), LZ4 70.8 → 29.8 % (31.0 % with backing). 0 corrupt pages.
- **Shankar: keep A; B (full compressor pager) shelved.** Results in
  `/mnt/project-files/zram-bench/`.

## 5. Navigator threat model fixes

[spec/atrium-navigator-threat-model.md](spec/atrium-navigator-threat-model.md)
(all recommendations accepted 2026-11-04).

| Row | Fix | Verified on FreeBSD |
|---|---|---|
| P0 converter (runs page JS) unjailed | one-shot jail per conversion `68adf2e`; navigatord refuses unconfined host | yes, `da360ac` — 50 pages byte-identical jailed vs unconfined, no leaks |
| P1 no LAN/loopback filter in fetch | filter after resolution + pin `4a99a56` | yes, `9880f2b` — 23-row M1 gate unjailed and jailed |
| P1 `require_confinement()` unused | `PRERENDER_REQUIRE_CONFINEMENT=1` | yes |
| Jail limits (rctl memory/pcpu, memfed pools) | `f26f8ad`; SetRctl replaces instead of stacks `c23c266` | yes, §6.5.2e |
| P1 production image decoding home | images decode in the document's worker; broker validates address, budget and reply (§2.4) `ceead6a` | — |
| P1 shared-store poisoning (per-user dedup, reproduce-to-share) | conversions reproduce 123/123 offline, host locale pinned (§2.6) `c92d16e` | — |
| P2 total fetched-bytes budget per conversion | enforced in the trusted fetch relay `b0927c0` | — |
| P2 link activation | broker decides every activation, scheme allowlist, only hrefs the recording carries (§2.10) `4995101`, `7b74432` | — |
| Portcullis snags from the FreeBSD run | deploy keeps `[resource_control]`, etc. (§6.5.2e′) `13ed962` | yes, `fec41a1` |
| Credentials design (§6: vault + per-origin fetcher, C0) | designed; no logins before C0 | — |

Also settled: islands + DRM approach (legacy-web §8.1); honest UA, never solve
bot challenges (§5.4.1b).

Open (2026-11-07): how to host the CDM without the linuxulator — scoping study
`docs/reviews/2026-11-07-linux-compat-scoping.md` recommends an own foreign-library host
(ELF loader + glibc shim, no kernel Linux ABI) for the CDM and OpenH264, and a micro-VM lane
for any future Linux apps. Accepted by Shankar 2026-11-07 (build first, then approach
Google for a licence and a native CDM build); specs amended.
Architecture proposed 2026-11-11: `docs/spec/hospes.md` (Hospes; build order H0–H5, seven
decisions in §14 accepted 2026-11-11). H0 measured 2026-11-11
(`docs/reviews/2026-11-11-hospes-h0.md`): Widevine makes raw syscalls (K1 hook required),
official arm64 CDM exists, OpenH264 only decodes Baseline/no-B-frame content correctly.
H1 done 2026-11-11 (`docs/reviews/2026-11-11-hospes-h1.md`): `hospes-elf` parser, `no_std`
`hospes-rt` (raw syscalls, allocator, glibc-shaped TLS, threads, loader); 11/11 fixtures pass
on FreeBSD amd64 and arm64 incl. the capability-mode isolation probe; real CDM and OpenH264
map and relocate. H2 done 2026-11-11 (`docs/reviews/2026-11-11-hospes-h2.md`): `hospes-glibc`
shim for OpenH264's 71 imports, `hospes-codec` (capability mode), player `h264` feature; 12/12
conformance streams bit-identical to glibc on Linux and FreeBSD amd64/arm64, player plays
H.264 in the FreeBSD arm64 VM. H3 done 2026-11-12 (`docs/reviews/2026-11-12-hospes-h3.md`):
`hospes-cdm` (CDM interface 10, capability mode, per-origin store), `hospes-proto`, a C++ Clear
Key test CDM with a glibc reference host (transcripts identical on Linux and FreeBSD
amd64/arm64), CENC/CBCS sample info in the MP4 and WebM demuxers, and the player decrypting
through `hospes-cdm` (cenc MP4, cbcs MP4 and encrypted WebM play with frames equal to the clear
sources on Linux and both FreeBSD VMs). K1 done 2026-11-12
(`docs/reviews/2026-11-12-hospes-k1.md`): system call trap for the library's own raw Linux
calls, a core hook in the fork's `syscallenter()` plus `hospes_trap.ko` (C glue, Rust range
table); `SIGSYS` handler in `hospes-rt`, Linux-call emulation and the first virtual files in
`hospes-glibc`; H0's raw call set trapped on FreeBSD arm64/amd64, probe and H3 still green.
`hospes-cdm` on FreeBSD now needs `hospes_trap.ko` loaded. H4 (Widevine) done 2026-11-12
(`docs/reviews/2026-11-12-hospes-h4.md`): Shaka's angel-one Widevine stream plays to the end
in the FreeBSD amd64 and arm64 VMs under Hospes (H.264 through the CDM's decrypt-and-decode, Opus
decrypt-only, licence POSTed through Nuntius, CDM installed by `hospes-install` from Google's
signed package). H5 (hardening) done 2026-11-12 (`docs/reviews/2026-11-12-hospes-h5.md`):
five fuzz targets (ELF, both channel decoders, VFS paths, install parsers) clean for 1 h each
after one fix (own `.deb` reader); version-watch CI `.github/workflows/hospes-version-watch.yml`
checks today's Chrome CDM and OpenH264 daily on both machines (CDM never run in CI). bhyve
wrapper not built. Build order H0-H5 complete; open items in the H4/H5 write-ups. The CDM's own jail and user done 2026-11-12
(`docs/reviews/2026-11-12-hospes-cdm-jail.md`): portcullisd `OpenCdm` + manifest capability
`protected-media`; hospes-cdm runs as `_hospes-cdm` (65027) in a one-shot jail with a
per-app, per-origin store, library and store rights-limited; arm64 VM check passes. Widevine WebM (VP9) plays through the manifest's PSSH
(`docs/reviews/2026-11-12-hospes-webm-pssh.md`, `ATRIUM_PLAYER_CDM_MPD`/`_PSSH`), arm64 VM. Real Widevine plays through the jailed system path (player as an app, CDM via `OpenCdm`, licence via `OpenFetcher`; `docs/reviews/2026-11-12-hospes-jailed-widevine.md`, `JAILED=1 scripts/hospes-h4-vm.sh`), arm64 VM. That run needed the fork rebased onto upstream main (`atrium-os/freebsd-src` branch `atrium-os` now holds it, Laminar on upstream's new scheduler interface; the old branch is kept as `atrium-os-pre-rebase-2026-11-12`). Run: `scripts/hospes-build.sh`, `scripts/hospes-h3-vm.sh`,
`scripts/hospes-kmod.sh`, `scripts/hospes-k1-vm.sh`, `scripts/hospes-h4-vm.sh`,
`scripts/hospes-version-watch.sh`, `scripts/hospes-cdm-jail-vm.sh`.

## 6. Navigator exploration coverage

Measurement in legacy-web §5.4.5.6a (`3dd3fb7`); raw tables in
`~/src/navigator-corpus/coverage-2026-11-05/` (Mac, never committed).

- Baseline: exploration covers 14.6 % of 2,045 handlers; server-fed data 1.5 %;
  large state spaces 28.6 % (mostly the 64-probe cap); other 47 % (probe fidelity).
- Done: probe fidelity (real gestures, delegated targets) `c57631b`;
  per-handler budget `37c1de6`, raised to 64 per handler in `c4bbf2e` (8 lost coverage); spinning probe ends
  itself `4e653a6`.
- Re-measure (legacy-web §5.4.5.6b): covered handler elements 298 → 324 at
  64/handler, 256/document; 7,422 probes, 29 s over both corpora.
- Sibling templates (legacy-web §5.4.5.6c, recording format `/4`): covered handler
  elements 324 → 754 (19.4% → 45.2%); case 3 775 → 55; server-fed 39 = 18 data,
  13 remote execution, 8 analytics. No normalize/flag change; raw tables in
  `coverage-2026-11-05-r3/`.
- Next: answer `el.id` at load (24 documents read it; own corpus diff); the 6
  pre-existing replay failures (gov.uk, chinadaily, yahoo.jp, ynet); case 1 on arXiv.

## 7. x86_64 port

[spec/x86_64-port.md](spec/x86_64-port.md) (status §3.5).

- **A done** (`2280670`): Tier-2 on x86_64 via Cranelift.
- **Bespoke x86_64 Tier-2 backend** ([spec/tier2-bespoke-x86.md](spec/tier2-bespoke-x86.md)): phase 1 skeleton (`4b0675c`, iced-x86, x86-64-v3) and phases 2–3, VCode + linear-scan regalloc, integer ops, any control flow (`dbd2d34`); phase 4 memory + vertex stage (`01d5c6c`); phase 6 compute (`1a8f884`); phase 7 images and derivatives via staged helper calls; phase 8 span entry (call-per-lane thunk, ELF symbol and blob `fs_span`). All 110 compute differential cases and the texture/fetch tests now run bespoke on x86 (none falls back since PackHalf2x16 landed). Next: phase 9 benchmark. `atrium-spv-compile` tries it before Cranelift on x86_64; Cranelift takes whatever it returns Unsupported. Debug knobs `ATRIUM_SPV_X86_DUMP=1`, `ATRIUM_SPV_FORCE_SPILL=1`.
- **Tier-2 robust access** ([spec/tier2-robust-access.md](spec/tier2-robust-access.md)): step 1 done, ABI v5 bounds-checks every SSBO access (x86 bespoke and Cranelift; out-of-range accesses go to a sink) and clamps texel pointers. The arm64 bespoke backend declines SSBO shaders until it has the check, so they use Cranelift there. Step 2 done: `atrium-spv-compile --stdio` sandboxes itself (Capsicum on FreeBSD, Landlock on Linux) before reading SPIR-V and the loader writes the cache. Step 3 done: one executor jail per app (`--transport broker` hands each connection to an `org.atrium.aqueduct.gpu` jail from portcullisd; `scripts/aqueduct-gpu-bundle.sh`), verified in the amd64 VM.
- **C done**: amd64 FreeBSD VM boots the fork kernel (`801a762`) and a Tessera
  root; fork userland cross-built and installed (`108098c`); Navigator jail e2e
  passes on amd64 (`6273d0d`); gpusim on amd64 (`991db65`, `atrium_gpu_test` 27/27).
- **E done (2026-11-12)**: the box was reprovisioned for dual boot; Atrium boots
  natively from its own partition (`scripts/r9700/`, one-shot `efibootmgr --bootnext`,
  20 min fallback to Ubuntu). The silicon GPU bring-up follows the stages in
  [spec/r9700-bare-metal.md](spec/r9700-bare-metal.md) (stage 2 discovery done).
- **Next:** GPU stage 3 (PSP + firmware); B (finish Ubuntu userspace). D (VFIO) is
  superseded by native boot.

## 8. Cloud dev loop

Cloud sessions run FreeBSD 16-CURRENT under QEMU TCG (no KVM, ~50–100× slower
than HVF): fine for jail/functional tests, not for perf numbers. See
[RUNBOOK §13.1](../RUNBOOK.md#131-freebsd-vm-in-a-cloud-session-scriptscloud-vmsh).

## 9. Navigator document lane (normalizer + renderer)

The document lane converts a page into the Document Profile
([spec/atrium-document-profile-v1.md](spec/atrium-document-profile-v1.md)) and
lays it out natively. Measured against two frozen corpora on the Mac (batch 1:
85 recordings; batch 2: 47), flagged by `nsg-flag` (TEXTLOSS, OVERLAP,
OFFCANVAS, SLICED, NARROW, REFUSED).

- 2026-11-03/04 work, roughly in order: inherited custom properties shared,
  GitHub render 177 s → 16 s; grid placement and minified media queries;
  tables (rowspan, baselines, nested tables, RTL tables); UAX #14 line breaking
  for CJK/Thai and spaced scripts; full RTL (`f03f991`…`ef5ef3e`); flexbox
  §9.7 freezing and auto margins, webkit aliases (…`228bc7f`); reversed flex
  done by the normalizer so visual order = reading order (`ac9e827`, profile G5);
  grid track sizing and §8.5 auto-placement; grid shorthands and Box Alignment
  (`36a341f`…`d715d05`).
- Rule: after any renderer change re-run `navigator-normalize` before
  `nsg-flag`, or the flags are stale. Flag runs are numbered
  (`baseline/flag-N.txt`, `batch2/flag-N.txt`) in `~/src/navigator-corpus`.
- Policy: honest UA, bot-check pages are REFUSED-BY-ORIGIN, never solved
  (legacy-web §5.4.1b).

## 10. Navigator backend

[spec/atrium-navigator-backend.md](spec/atrium-navigator-backend.md).

- M3 forward/back through every layer passed (`a168de8`).
- Session save/restore by replay through a content-addressed store (`6b6449d`),
  then through the jailed host (`0841678`).
- M4: jailed save/restore passes in a real FreeBSD jail (`b9edfe3`;
  "1 sessions, 1 navigations, 1 rewinds, 1 restores, 0 failures", TCG timings only).
- Accepts recording format v3 (`08661a8`).

## Finished threads (for the record)

| Thread | Outcome |
|---|---|
| Run FreeBSD VM on cloud machine / Boot FreeBSD and run jail tests | `scripts/cloud-vm.sh` (`e163289`); jail e2e passes in the cloud VM in ~1m48s |
| Test jailed save and restore | Navigator M4 passes in a real jail (`b9edfe3`) |
| Build compressed main memory | zram v2 increments 0–6, design A kept, B shelved |
| Add RDNA4 shader core to gpusim | gpusim compute core from public sources; calibrated against the R9700 (VGPR step 8/4, LDS unit 512 B, TIDIG); scratch modelled (gpusim `ed8c928`) |
| Port Atrium to x86_64 | phases A and C done; amdgpu firmware licence checked: binary redistribution OK, no reverse engineering |
| Set up R9700 GPU access here | access via Remote Control on the Mac; handed over to "Run R9700 jobs from the Mac" |
| Verify Navigator security fixes on FreeBSD | §2.1/§2.2 verified (`9880f2b`, `da360ac`); jail limits verified and rctl stacking bug fixed (`c23c266`); open nits in portcullis.md §6.5.2e′ |

## Where things live

| What | Where |
|---|---|
| Repo (Mac clone) | `~/src/bsd` |
| gpusim | `~/src/gpusim` (Mac), `~/gpusim` in cloud; repo atrium-os/gpusim |
| FreeBSD fork | atrium-os/freebsd-src branch `atrium-os` |
| QEMU fork (gpusim device) | atrium-os/qemu |
| Navigator corpus (third-party content, never commit) | `~/src/navigator-corpus` on the Mac |
| Shared outputs (reports, screenshots, kits) | project files: `graphics/`, `navigator/`, `x86_64/`, `zram-bench/`, `catchup/` |
| R9700 box | reachable only from the Mac (`ssh -l ubuntu orbis-r9700`); see RUNBOOK §13.5 |
