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
| Printing (Prelum) | [printing.md](spec/printing.md) | R7 open; R6 and R8 parked | R7: confirm before sharing a printer, printer PINs | R6 and R8 need a real printer |
| Dev tools | [dev-tools.md](spec/dev-tools.md) | M9 open | M9: Curia grant UI; the jailed agent | — |
| Network settings gaps | [atrium-net.md](spec/atrium-net.md) | paused | continue closing the gaps | Shankar's go-ahead |
| Package catalog | [atrium-pkg-registry.md](spec/atrium-pkg-registry.md) | key ceremony deferred | signing-key ceremony | YubiKeys (Shankar is buying them) |
| Security-key unlock | [sigillum.md](spec/sigillum.md) | blocked | real-key runs (`scripts/sigillum-real-key-*.sh`) | hardware |
| GPU driver on silicon | [atrium-gpu-amd-design.md](spec/atrium-gpu-amd-design.md) | stage 3 done on the R9700 | GPU power P2, then stage 4 (GMC, IH, MES, first dispatch) | hardware time on the R9700 box |
| TPM and secure boot | [tpm-boot.md](spec/tpm-boot.md) | proposed 2026-11-18, nothing built | P0: swtpm in `run-vm.sh` and the cloud VM, `atrium-tpm probe`, probe the R9700 box | Shankar's answers to the spec's §10 decisions |

## Recently finished

Tessera encryption E0–E6, fonts, boot time, the share sheet.
