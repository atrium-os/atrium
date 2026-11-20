# Workbook Open and Save with a 1 MiB verification buffer

On a freshly written 64 MiB Tessera workbook, increasing Scrinium’s verification read size from 64 KiB to 1 MiB saved a median 539 ms on Open and 532 ms on Save. All eight Tessera pairs improved at both workbook sizes. On UFS, whole-workbook timings varied in both directions, with median changes within 1.1 ms. Scrinium now uses the larger buffer when verifying the pinned workbook. It hashes the entire file and continues to stage replacement bytes in 64 KiB chunks.

## Elapsed time

Times are milliseconds. The two buffer columns give separate medians. The change and range columns use the eight within-pair differences, calculated as 1 MiB minus 64 KiB. Negative values indicate a faster operation. Grant is part of Open, and Commit is part of Save.

| Filesystem | CSV (MiB) | Operation | 64 KiB | 1 MiB | Paired change | Paired range |
|---|---:|---|---:|---:|---:|---:|
| UFS | 16 | grant | 15.5 | 15.1 | -0.1 | -2.6 to 0.0 |
| UFS | 16 | open | 48.5 | 47.8 | -1.1 | -11.5 to +1.8 |
| UFS | 16 | commit | 33.9 | 33.7 | -0.2 | -19.3 to +0.9 |
| UFS | 16 | save | 64.5 | 63.3 | -0.6 | -29.7 to +0.5 |
| UFS | 64 | grant | 60.7 | 59.7 | -0.4 | -3.2 to +0.0 |
| UFS | 64 | open | 198.5 | 199.9 | +0.0 | -6.0 to +6.1 |
| UFS | 64 | commit | 130.5 | 129.9 | -0.4 | -1.4 to +13.4 |
| UFS | 64 | save | 253.9 | 252.9 | -0.3 | -3.3 to +18.6 |
| Tessera | 16 | grant | 42.4 | 24.9 | -17.1 | -19.6 to -15.4 |
| Tessera | 16 | open | 84.2 | 66.4 | -17.8 | -21.3 to -12.4 |
| Tessera | 16 | commit | 97.1 | 79.5 | -19.0 | -21.3 to -14.4 |
| Tessera | 16 | save | 127.3 | 109.5 | -18.3 | -21.4 to -14.5 |
| Tessera | 64 | grant | 630.5 | 93.9 | -537.3 | -550.3 to -519.6 |
| Tessera | 64 | open | 799.3 | 264.2 | -538.9 | -547.9 to -521.2 |
| Tessera | 64 | commit | 820.6 | 289.3 | -536.2 | -546.0 to -451.7 |
| Tessera | 64 | save | 944.2 | 413.5 | -531.5 | -551.7 to -446.7 |

## Conditions and checks

- Both release probes were built from the scratch worktree at `5fbf6af4` with the same locked dependencies. The candidate sized each read buffer to the smaller of the file length and the caller’s chunk limit, with verification capped at 1 MiB. The hashes of the deployed binaries matched their host builds.
- The private FreeBSD arm64 VM ran kernel `9f2eed03a36c` under QEMU/HVF with four vCPUs and 4 GiB RAM. Each arm started with a freshly formatted filesystem on the same dedicated raw virtio disk. The probe ran as `nobody` through a nullfs mount. Inputs were synced and read to warm the cache before timing. Each pair used identical CSV bytes and changed A1 from 1 to 2. The complete published file matched the expected edit in every run.
- For each filesystem and size, round zero warmed up both builds. Eight measured pairs followed, alternating which build ran first. Open covers descriptor open, grant creation, CSV read and parse. Save covers CSV encoding, snapshot sealing, commit and sync. Desktop launch, rendering and IPC are outside these timers. The fixture measures the first Save after Open. [Probe and build instructions](2026-11-20-sheets-verification.md#conditions-and-reproduction) · [Raw trials in execution order](data/2026-11-20-scrinium-verification-buffer.csv)
- All 51 Scrinium library tests passed on each filesystem. After four consecutive saves, the external-edit test overwrote a byte in the short final chunk of a 1 MiB + 4 byte file, preserved its inode and length, and restored its modification time. Scrinium refused the next Save and preserved the external bytes. The Tessera retained-generation test recovered the complete old and new revisions.
