# Opening a published workbook after remount

For a published 64 MiB CSV in the arm64 VM, the first Open after remount took a median 197 ms on UFS and 291 ms on Tessera. Compared with reopening the same file warm, the median within-pair cost was 2.2 ms on UFS and 18.5 ms on Tessera. Tessera’s first Open was slower in all 12 pairs. UFS whole-Open differences spanned zero.

## Where the time went

Times are elapsed milliseconds. Each phase column is its own median over 12 rounds. The last two columns use within-round differences (first Open minus warm reopen). Open includes all three component rows.

| Filesystem | Operation | After remount | Warm reopen | Paired difference | Paired range |
|---|---|---:|---:|---:|---:|
| UFS | Open | 196.8 | 193.1 | +2.2 | −8.1 to +24.1 |
| UFS | File open and Scrinium grant | 61.5 | 59.2 | +2.3 | +1.3 to +4.4 |
| UFS | Read workbook into Sheets | 15.6 | 14.8 | +0.8 | +0.4 to +4.4 |
| UFS | Parse CSV | 120.0 | 119.2 | −1.2 | −10.3 to +15.3 |
| Tessera | Open | 290.6 | 269.5 | +18.5 | +9.0 to +51.2 |
| Tessera | File open and Scrinium grant | 117.3 | 97.9 | +19.5 | +14.0 to +32.5 |
| Tessera | Read workbook into Sheets | 48.2 | 47.9 | +0.2 | −1.3 to +4.5 |
| Tessera | Parse CSV | 122.8 | 123.3 | −2.0 | −5.6 to +22.9 |

Scrinium reads and hashes the workbook when it issues the grant. On Tessera, this step accounted for most of the remount cost. Across every measured round the guest device recorded 205 reads totaling 65.0 MiB on Tessera’s first Open and 130 reads totaling 64.0 MiB on UFS’s. Every warm reopen recorded zero device reads.

## Conditions and reproduction

- The release probe used production Scrinium and Sheets at `403b5b3a`, including the 1 MiB verification buffer. The guest ran FreeBSD arm64 kernel `9f2eed03a36c` under QEMU/HVF with four vCPUs, 4 GiB RAM and a dedicated raw virtio disk with QEMU host caching disabled. Each filesystem had one excluded warm-up pair and 12 measured pairs. Filesystem order alternated, and each round used identical CSV bytes on both filesystems.
- For each pair, the probe published a sealed snapshot through Scrinium on a freshly formatted filesystem via nullfs. Both the view and underlying filesystem were unmounted and mounted again. A fresh unprivileged process opened the writable descriptor, obtained a grant, read the file and parsed it. A second fresh process repeated those steps without an intervening remount. Both processes regenerated the expected bytes in memory and checked the full file and re-encoded document after timing. The two existing Save probe modes also passed a smoke test.
- Device counts are differences in cumulative `iostat -Idx vtbd2` counters around each probe invocation, after mounting finished. Remounting exercises guest filesystem cache state. Host storage and device caches can still affect the elapsed times. The timers exclude fixture generation, mount time, jail launch, IPC, and first paint.
- Build the [probe](../../scripts/probes/sheets-verification.rs) using the [host build instructions](2026-11-20-sheets-verification.md#conditions-and-reproduction). On a private test volume run `sheets-verification-probe /mnt/view/Book.csv 64 357104 publish-only`. Unmount the nullfs view then the filesystem, recreate both mounts, and run the same command twice with `open-only` in separate processes. That mode outputs size, seed, grant, workbook read, CSV parse and total Open, with times in microseconds. [Individual trials and device counters](data/2026-11-20-sheets-remount-open.csv)
