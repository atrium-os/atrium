# From loaded workbook to visible grid

After the 64 MiB CSV finished loading, Sheets took a median **229 ms on UFS and 232 ms on Tessera** to submit the first scene containing workbook cells. Synchronous autosave setup occupied 190–191 ms of that interval. Sending the populated batch itself took 2.8 ms. After submission, the first frame with workbook values appeared a median **392 ms later on UFS and 370 ms later on Tessera** in the headless desktop.

## Elapsed time

Each filesystem had eight measured Opens into fresh document jails. Medians and ranges below are in milliseconds. Each row is calculated from its own paired timestamps.

| Interval | UFS median (range) | Tessera median (range) |
|---|---:|---:|
| Document main to CSV parse start, including file read | 56.4 (48.8–81.5) | 92.0 (86.3–109.9) |
| Worker parse and recalculation | 127.8 (119.4–147.5) | 123.7 (119.8–150.6) |
| Worker finished to UI picking up the result | 10.4 (4.0–16.6) | 14.7 (0.2–19.5) |
| Initial autosave setup on the UI thread | 190.1 (181.4–218.7) | 191.4 (187.5–232.9) |
| Document installed in UI to first populated batch beginning | 21.8 (20.4–27.0) | 21.8 (20.7–46.9) |
| Batch beginning to client present request sent | 2.8 (2.7–9.8) | 2.8 (2.6–4.0) |
| **Worker finished to present request sent** | **228.9 (211.5–259.2)** | **231.6 (215.4–297.3)** |
| Present sent to first visible cells | 391.9 (3.8–455.2) | 370.0 (281.2–407.5) |
| Present sent to both columns visible | 453.5 (377.8–469.2) | 370.0 (281.2–407.5) |
| **Document main to first visible cells** | **851.4 (391.4–866.0)** | **827.9 (813.1–832.0)** |

The autosave interval covers creating the initial recovery base and journal before Sheets installs the loaded document. Serialization and filesystem syncs are included together. The filesystem labels identify the source workbook. In both arms autosave uses the document jail’s state directory.

The post-submission interval includes server processing, scheduling, headless frame pacing and rendering. The headless compositor enforces a 200 ms minimum interval between ordinary captures and encodes earlier frames. In three UFS trials a frame showed numeric values before the text column became visible. Both columns appeared together on the other five UFS trials and all eight Tessera trials. These measurements do not resolve physical display latency or assign the entire post-submission interval to rendering.

## Markers and checks

- The load worker sampled the guest monotonic clock on successful return from CSV loading and recalculation. The UI sampled it again when accepting that result, around autosave initialization, and on return from installing the document.
- Pergola inspected the actual node deltas routed to a window. In every trial the first batch matching the numeric cell region also carried the unique seeded B1 text prefix. Markers bracketed committing those deltas and returning from the client’s present send. The compositor independently timestamped completed frames before PNG encoding. All intervening pixel signatures were classified, including the empty grid and the frame containing only numeric cells.
- This repeats the private desktop fixture with two identical published CSVs, one warm-up per filesystem and eight alternating measured pairs. Each trial verified the picker selection, document PID and live JID. Independent PNG decoding matched the frame signatures. Both files retained their original SHA-256. The host-built release Sheets probe used sources at `3b7bc9bb`; the compositor and Scrinium binaries matched the preceding desktop profile. [Fixture and frame controls](2026-11-20-sheets-desktop-open.md)
- To reproduce, apply the measurement patch in a scratch checkout with `git apply --unidiff-zero scripts/probes/sheets-desktop-open.patch` and rebuild the documented release binaries. Join `scene-stage` and `scene-batch` records by document PID with the picker-confirm and frame records on the guest monotonic clock. Round zero is the warm-up. [Timing patch](../../scripts/probes/sheets-desktop-open.patch) · [Individual trials and timestamps](data/2026-11-20-sheets-open-stages.csv)
