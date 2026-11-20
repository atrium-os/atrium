# Sheets workbook verification cost

At 64 MiB, verification added a median 722 ms to Open and 904 ms to Save on Tessera. The corresponding UFS costs were 55 ms and 121 ms. The UFS whole-open differences spanned zero, while the grant step alone took an extra 60–96 ms in every pair.

The tables give elapsed milliseconds in a private arm64 FreeBSD VM. Each row compares Scrinium before verification (`fa8c0a26`) and with verification (`9f9e48e5`). Each case has 12 matched pairs after one warm-up pair. **Added** is the median of the 12 within-pair differences. Before and after are separate medians, so their difference can disagree with the paired estimate.

## Time in the workbook path

Open includes opening the writable file, obtaining its grant, reading the CSV and parsing it into a recalculated Sheets document. Save includes encoding the edited CSV, sealing the snapshot and committing it through Scrinium. The last column shows the full spread of the paired differences.

| Filesystem | CSV | Action | Before (ms) | After (ms) | Added (ms) | Paired range (ms) |
|---|---:|---|---:|---:|---:|---:|
| UFS | 16 MiB | Open | 36.78 | 55.86 | +17.0 | +10.4 to +29.8 |
| UFS | 16 MiB | Save | 40.06 | 71.18 | +30.3 | +20.2 to +98.8 |
| UFS | 64 MiB | Open | 172.45 | 214.67 | +55.0 | -141.4 to +181.9 |
| UFS | 64 MiB | Save | 154.94 | 268.83 | +121.3 | -71.8 to +365.6 |
| Tessera | 16 MiB | Open | 44.18 | 97.75 | +53.7 | +38.5 to +64.0 |
| Tessera | 16 MiB | Save | 71.87 | 172.59 | +63.7 | -50.6 to +186.2 |
| Tessera | 64 MiB | Open | 180.82 | 954.23 | +721.8 | +576.1 to +1137.6 |
| Tessera | 64 MiB | Save | 341.39 | 1252.90 | +904.1 | +454.0 to +1644.5 |

## Time in Scrinium

Open here ends when the writable descriptor has a grant. Save starts with the sealed snapshot ready and ends after the commit, including publication and syncs. The new path uses 64 KiB positioned reads to hash the original workbook on both operations and hashes the replacement bytes as it copies them on Save.

| Filesystem | CSV | Action | Before (ms) | After (ms) | Added (ms) | Paired range (ms) |
|---|---:|---|---:|---:|---:|---:|
| UFS | 16 MiB | Open | 0.06 | 16.56 | +16.5 | +14.8 to +28.2 |
| UFS | 16 MiB | Save | 5.29 | 37.51 | +30.8 | +25.4 to +84.4 |
| UFS | 64 MiB | Open | 0.08 | 63.23 | +63.2 | +59.8 to +96.1 |
| UFS | 64 MiB | Save | 16.53 | 137.91 | +121.2 | +55.5 to +306.8 |
| Tessera | 16 MiB | Open | 0.06 | 44.94 | +44.9 | +41.2 to +70.5 |
| Tessera | 16 MiB | Save | 38.97 | 128.59 | +64.5 | -13.6 to +204.1 |
| Tessera | 64 MiB | Open | 0.08 | 693.18 | +693.1 | +617.1 to +1090.7 |
| Tessera | 64 MiB | Save | 205.37 | 1075.23 | +886.2 | +490.1 to +1442.5 |

## Conditions and reproduction

- Both release binaries were host-built from the same probe, Sheets sources, toolchain and dependency versions. The guest ran FreeBSD 16.0-CURRENT kernel `9f2eed03a36c` under QEMU/HVF with four vCPUs and 4 GiB RAM. Both filesystems used the same dedicated 1 GiB raw virtio disk with QEMU host caching disabled. Each arm started with a freshly formatted filesystem, including a fresh Tessera content store. UFS used soft updates. The probe ran as `nobody` through a nullfs view on both.
- Each input row occupied 1,024 bytes, with a numeric cell, a deterministic 1,020-letter text cell and a CRLF terminator. Inputs had 16,384 or 65,536 rows. Each pair used identical bytes, changed A1 from 1 to 2 and compared the entire published file with the expected bytes outside the timer. Fixture creation, fsync and a full read warmed the input before each trial. Trial seeds varied. Baseline ran first in odd rounds, verified first in even rounds. Size and filesystem order also alternated.
- These timings cover a warm local workbook. Desktop launch, picker interaction, IPC, window rendering and autosave setup fall outside the timers. Storage hardware, cold caches, other file formats and concurrent load can change elapsed times.

To rebuild either arm, use a bootstrapped checkout of its revision and the same probe source from this report’s commit. Run these commands from the checkout on the host, with `probe` set to the source path.

```sh
root=$PWD
driver=$(mktemp -d)
cargo new --bin --name sheets-verification-probe "$driver/driver"
cp "$probe" "$driver/driver/src/main.rs"
manifest="$driver/driver/Cargo.toml"
for dep in scrinium atrium-sheets limen; do
    cargo add --manifest-path "$manifest" --path "$root/$dep"
done
cargo build --manifest-path "$manifest" --release --target aarch64-unknown-freebsd
```

Stage the binary on the guest’s local disk. Run it once per fresh volume with a new destination file, for example `sheets-verification-probe /mnt/view/Book.csv 64 357104`. It prints the size, seed and four elapsed times in microseconds. Round zero in the data is the excluded warm-up. [Probe source](../../scripts/probes/sheets-verification.rs) · [Individual trials in execution order](data/2026-11-20-sheets-verification.csv)
