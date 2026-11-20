# Initial Sheets autosave during desktop Open

Opening the 64 MiB CSV spent about 168 ms serializing and buffering the initial recovery base outside file writes. The 67 write calls added about 23–24 ms. Source filesystem had little effect on these stages. Both jails wrote the 67,886,078-byte base through unionfs to a private tmpfs upper layer. The sync measurements therefore describe memory-backed state.

## Elapsed time

Median (minimum–maximum) in milliseconds across eight measured Opens per source filesystem. Each component uses elapsed time on the UI thread. Serialization includes buffering and probe overhead outside the timed writes. Each median is calculated independently.

| Operation | UFS source | Tessera source |
|---|---:|---:|
| Initial autosave, total | 192.611 (187.688–217.863) | 191.277 (187.452–210.042) |
| Native serialization and buffering outside writes | 167.833 (164.131–187.444) | 167.959 (165.414–184.421) |
| Base file write calls | 23.695 (22.295–30.224) | 23.342 (21.840–25.404) |
| Three file syncs combined | 0.005 (0.004–0.006) | 0.006 (0.004–0.022) |
| Two directory syncs combined | 0.001 (0.001–0.003) | 0.001 (0.001–0.002) |
| Remaining setup and metadata | 0.192 (0.169–0.245) | 0.202 (0.183–0.259) |

## Method and recovery check

The isolated arm64 HVF desktop ran FreeBSD kernel `9f2eed03a36c` with the same mounted source files, compositor and picker as the preceding experiment. After one warm-up per filesystem, eight pairs alternated source order. Each Open used a fresh document jail. The picker selection, granted filename, live PID and jail ID matched on every trial, and the compositor showed the fully populated grid before recovery checks began. Both source hashes matched the fixture after all trials. [Desktop fixture](2026-11-20-sheets-desktop-open.md)

For the timing build, apply both patches with `git apply --unidiff-zero` and cross-build the release binaries as in the desktop fixture. Sheets was built from `bd543854` with these probes. The writer under the usual native serializer and buffer records time inside each file write. Subtract that sum from elapsed serialization through the final buffer drain. Time each sync separately on the base, empty journal, generation pointer, and directory before and after the pointer rename. Directory open time stays in remaining setup. The probe requires each directory sync to succeed. Join the records to the outer autosave interval by document PID. Round zero is warm-up. [Desktop markers](../../scripts/probes/sheets-desktop-open.patch) · [Autosave probe](../../scripts/probes/sheets-autosave-base.patch) · [Individual times in nanoseconds](data/2026-11-20-sheets-autosave-base.csv)

Before closing each document, copy its live generation pointer, name, native base and empty journal to a private directory and compare each copy byte for byte. Run the release example as the unprivileged fixture file owner with the copy and the selected source CSV as arguments. All 18 copies recovered through the production reader with the correct name, zero replayed edits and zero torn bytes. Re-export reproduced every byte of each 64 MiB input. Recovery was exercised while the originating jail was live. Availability of autosave state after jail teardown or reboot is a separate question. [Recovery check](../../atrium-sheets/examples/recover_base_probe.rs)
