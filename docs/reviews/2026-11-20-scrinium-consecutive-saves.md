# Consecutive saves of a published Tessera workbook

With the current 1 MiB verification buffer, the median first Save took 364 ms and the median second Save took 379 ms on the same grant. Relative to the previous 64 KiB buffer, the median paired reduction was 88 ms for the first Save and 100 ms for the second. All 12 second saves improved. Each save published the expected 64 MiB of workbook bytes.

| Operation | 64 KiB median (ms) | 1 MiB median (ms) | Paired change (ms) | Paired range (ms) | Pairs faster with 1 MiB |
|---|---:|---:|---:|---:|---:|
| Grant | 179.6 | 101.0 | -78.5 | -88.1 to -53.4 | 12/12 |
| Open | 359.4 | 279.5 | -78.1 | -107.4 to -53.4 | 12/12 |
| First commit | 334.1 | 236.7 | -89.9 | -331.3 to +102.3 | 11/12 |
| First Save | 458.1 | 363.6 | -88.1 | -394.1 to +126.8 | 11/12 |
| Second commit | 333.6 | 244.7 | -89.6 | -335.3 to -56.7 | 12/12 |
| Second Save | 473.3 | 379.3 | -100.1 | -456.3 to -46.1 | 12/12 |

For the current buffer, the median *within-run* change from first to second Save was +5.6 ms, with a range of −162 to +90 ms. The timings do not establish a consistent penalty for reusing the grant.

## Method

- The probe used Scrinium’s Save As publication path to create the CSV from a sealed snapshot on a freshly formatted Tessera volume. It then opened the published file, obtained one grant, and parsed the CSV into one Sheets document. It changed A1 from 1 to 2 and saved, then changed A1 to 3 and saved again with the same token. It compared the entire on-disk file with the expected bytes after publication and after each Save, outside the timers.
- Each buffer ran one warm-up and 12 measured rounds. Both arms used the same deterministic 64 MiB input for each round, alternating arm order across rounds. Each arm ran as an unprivileged user through nullfs on the same private raw virtio disk. The arm64 FreeBSD VM ran kernel `9f2eed03a36c` under QEMU/HVF with four vCPUs and 4 GiB RAM. The two host-built release binaries used the same Sheets sources and dependency versions, with Scrinium from `eee204a9` for 64 KiB and `6d7f925c` for 1 MiB. The deployed binaries matched their host checksums.
- Open includes obtaining the grant and parsing the CSV. Save includes encoding, sealing, and Scrinium commit through directory sync. The grant and commit rows are components of those totals. The timings cover warm local files and exclude desktop launch, rendering, IPC and the initial Save As. Earlier measurements used a freshly written input with a different Tessera write history. [Previous first-Save benchmark](2026-11-20-scrinium-verification-buffer.md)
- Build the same probe against each Scrinium revision and append `published-pair` to its usual invocation, for example `sheets-verification-probe /mnt/view/Book.csv 64 357104 published-pair`. Output gives size and seed, followed by grant, Open, first commit, first Save, second commit and second Save in microseconds. Round zero is the excluded warm-up. [Build instructions](2026-11-20-sheets-verification.md#conditions-and-reproduction) · [Individual trials](data/2026-11-20-scrinium-consecutive-saves.csv)
