# Work in flight

The short list of open workstreams and the exact next step for each, so any
agent (Claude or Codex) can pick one up from the repo alone. Detail lives in
each spec and in [STATUS.md](STATUS.md); this file only says where each one
stands and what to do next.

**Rule (see AGENTS.md):** before a session ends or hands work on, update its
row here (state, next step, anything uncommitted or unpushed, what it is
waiting on) and push. If a row and the commit log disagree, the log wins:
fix the row.

Last updated: 2026-11-19.

## Open

| Workstream | Spec | Where it stands | Next step | Waiting on |
|---|---|---|---|---|
| VPN (Crypta) | [vpn.md](spec/vpn.md) | V0–V4 done and pushed: V4 gate 60/60 (`e2b3f075`, includes the sign-in window end to end, the IdP password through Arca's field, the sign-in jar forgotten with its profile); `crypta-ac` fuzzers (`7df29276`); Settings sign-in frames + Curia crash fix (`6286a322`, `scripts/crypta-v4-gate.sh ui`, frescod needs the cloud VM booted with `TCG_CPU=Haswell-noTSX`). Nothing uncommitted | V5: GlobalProtect engine (`crypta-gp`) per vpn.md §9/§11; optional: frame the SSO sign-in window (navigator-ui on the VM) | script SSO (Okta, Entra) and security keys/passkeys at the IdP wait on Navigator live code I3 (Shankar's choice) |
| Navigator live code (islands) | [navigator-islands.md](spec/navigator-islands.md) | accepted 2026-11-19 (D1–D6); I0 not started | I0: cross-build Servo and Ladybird on the Mac and measure them side by side | a Mac session with disk room (about 104 GB free now, tight for both engines) |
| Mail | [mail.md](spec/mail.md) | mostly done; filters against a real Pigeonhole, VM check `scripts/mail-vm-check.sh` | VM runs for Open with…, Work compose warnings, the link scanner | the VM, and the pieces those features call into |
| Share sheet and notification actions | [share-sheet.md](spec/share-sheet.md), [tabula.md](spec/tabula.md) §5.5 | done and pushed: cross-persona sharing (`a91db3a`), notification action buttons and app raise (`55db409`, `82b3222`), the shelf in its own jail with the `notification-shelf` permission (`e94b014`, gate `scripts/vm-notify-actions-check.sh` 19/19 in the amd64 cloud VM). Nothing uncommitted | optional: let Praeco hand the app its own press so Mail can raise its reply window from an action | — |
| Officina Sheets | [officina.md](spec/officina.md) | feature batches pushed | Flash Fill number and date reformatting; rich-text comments; wrap in the cell editor | — |
| Printing (Prelum) | [printing.md](spec/printing.md) | R0–R5 done and pushed (§11.1–§11.5; R5 VM gate `scripts/prelum-r5-vm.sh` 40/40 in an own cloud VM, `a3a8fd47`). Sheets printing: merged cells, Center Across and frozen rows/columns as print titles (`d57f39d3`). Nothing uncommitted; cloud VM shut down | R7 (§9, §11 table): Arca-held printer passwords and PIN jobs; personas enforced (a Work job refused on a Personal printer by default; sharing a printer to every persona asks the person first). Also open: "Printed" notice can't be turned off; no Release in the status item; Mail prints text only; Sheets lacks print areas, charts, pictures | R6 (IPP over USB) and R8 (Wi-Fi printer over IPPS) need a real printer from Shankar (§12 D16) |
| Dev tools | [dev-tools.md](spec/dev-tools.md) | M9 open | M9: Curia grant UI; the jailed agent | — |
| Network settings gaps | [atrium-net.md](spec/atrium-net.md) | paused | continue closing the gaps | Shankar's go-ahead |
| Package catalog (Opifex) | [opifex.md](spec/opifex.md) (§14 build log) | C0–C7 built, VM gates 0 failed; root keys on hardware tokens (`portcullis-token`, rehearsed on SoftHSM: `scripts/opifex-token-check.sh`); `opifex sign` for manifests built elsewhere (§10.4); retired/revoked platform keys (§5.1.1); side-load via the sheet; base files (rtld, /lib, /usr/lib) always from this machine's base and refreshed with `opifex refresh-base` (§7.5, `8b32c02`, VM-checked via vm-bundle-check). Nothing uncommitted or unpushed | key ceremony per catalog-repo README (3 root YubiKeys + a 4th for the platform key, touch-policy cached), then create `atrium-os/catalog` + `catalog-blobs`, Pages, `CATALOG_KEY` secret, publish the CJK font packages (`scripts/opifex-cjk-fonts.sh`); whatever installs base updates must run `opifex refresh-base` | YubiKeys (Shankar is buying them); launch trust for added catalogs (platform); Curia Updates/add-a-catalog pages |
| Lock screen and security-key unlock | [lock-screen.md](spec/lock-screen.md), [sigillum.md](spec/sigillum.md) §16.6 | Key unlock passed on a real YubiKey 2026-11-13 (`scripts/sigillum-real-key-unlock.sh`). Lock screen built (frescod enforces the seat lock file, ostiarius lock + lock-only socket, forum-bar Lock, vestibulum `--lock USER` with password and "Use a security key"): FreeBSD tests pass, frescod gate `scripts/vm-lock-check.sh` 22/22 (`fdbc4b6a`). Nothing uncommitted or unpushed | full run in the Mac VM with the real vestibulum, ostiarius, sigillum-ui and the YubiKey, on a portcullisd built with `--features pam` (without it any non-empty password unlocks); check a wrong password is refused. Later: Settings "Add a security key", key removal | Shankar starting the VM with USB passthrough (sudo qemu). DSC token (S5) signing waits on him loading a certificate |
| GPU driver on silicon | [atrium-gpu-amd-design.md](spec/atrium-gpu-amd-design.md) | stage 3 done on the R9700 | GPU power P2, then stage 4 (GMC, IH, MES, first dispatch) | hardware time on the R9700 box |
| TPM and secure boot | [tpm-boot.md](spec/tpm-boot.md) | proposed 2026-11-18, nothing built | P0: swtpm in `run-vm.sh` and the cloud VM, `atrium-tpm probe`, probe the R9700 box | Shankar's answers to the spec's §10 decisions |

## Recently finished

Tessera encryption E0–E6, fonts, boot time, the share sheet.
