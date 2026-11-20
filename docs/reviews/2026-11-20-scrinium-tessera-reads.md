# Scrinium verification reads on Tessera

On a freshly written 64 MiB workbook, reads accounted for 91% of the verification pass. The 1,024 positioned reads of 64 KiB took 555 ms, and BLAKE3 took 55 ms. The uninstrumented production grant took 610 ms. On a byte-identical workbook published by Sheets Save, reads took 123 ms and hashing took 56 ms. The fresh-file case most closely matches the input used to measure Open in the [earlier workbook benchmark](2026-11-20-sheets-verification.md).

## Warm-cache timings

Each cell is the median elapsed time in milliseconds over 12 runs after one warm-up. The three rows for the timed verification loop come from the same passes. Each column’s median is calculated independently.

| Measurement over 64 MiB | Fresh write | Published by Sheets Save |
|---|---:|---:|
| Time inside 64 KiB positioned reads during verification | 555.3 | 123.4 |
| Time inside BLAKE3 updates and finalization during verification | 54.9 | 56.4 |
| Entire timed verification loop | 610.2 | 179.9 |
| Production Scrinium grant | 609.8 | 179.6 |
| BLAKE3 alone on resident bytes, 64 KiB updates | 55.2 | 56.3 |
| Positioned reads alone, 64 KiB requests | 564.0 | 123.9 |
| Sequential reads alone, 64 KiB requests | 564.7 | 122.6 |
| Positioned reads alone, 1 MiB requests | 36.1 | 43.5 |

The fresh-file read time was distributed evenly across the file. The four successive 16 MiB regions each took 138–139 ms at the median. Switching to sequential reads at 64 KiB gave similar timings. Increasing the positioned request size to 1 MiB cut the request count from 1,024 to 64 and reduced read time on both fixtures. The 1 MiB case measures reads only. The next experiment is to benchmark that size in the full verification and Save paths.

## Measurement conditions

- The host built a release example against Scrinium at `6b0d863e` and its locked BLAKE3 dependency. The test ran as `nobody` through a nullfs view of Tessera in a private FreeBSD arm64 VM, kernel `9f2eed03a36c`, under QEMU/HVF with four vCPUs and 4 GiB RAM. Both fixtures used the same dedicated 1 GiB raw virtio disk with QEMU host caching disabled.
- The previous workbook probe generated the 64 MiB CSV with seed 357104, edited A1 and saved it through Scrinium. The published-file profile ran first. For the fresh-file profile, the guest copied those exact bytes onto a newly formatted Tessera volume in one 64 MiB write and synced the file. SHA-256 matched across the copy. These are two observed file histories. The experiment did not isolate the Tessera implementation detail responsible for their timing difference.
- Each profile loaded the file into memory before timing, rotated the six cases so each occupied every position twice, and verified that both hashing cases produced the reference digest. Read-only cases checked a sample from every 64 KiB. The final full-file digest also matched. Both fixtures had BLAKE3 digest `30327fd02139d7523976c4772175e5547fcff7df701708144a85fd0152f0cedc`.
- The split loop timed each `FileExt::read_exact_at` and each BLAKE3 update separately. Read time includes kernel and filesystem work, copying into the caller’s buffer, and any scheduling delays. The grant control called the production entry point on the already-open writable descriptor. Its measured range was 605–619 ms on the fresh file and 175–385 ms on the published file. The CSV records elapsed microseconds in execution order, with round zero denoting warm-up. Only the split-loop rows have component timers. [Individual trials](data/2026-11-20-scrinium-tessera-reads.csv)

## Reproduction

Build on the host from a bootstrapped checkout.

```sh
cargo build --manifest-path scrinium/Cargo.toml --locked --release \
    --target aarch64-unknown-freebsd --example workbook_read_profile
```

Stage the example on the guest’s local disk. Run `workbook_read_profile /mnt/profile-view/Book.csv` with a writable 64 MiB workbook on the test mount. It emits the CSV on standard output and the reference digest on standard error. [Profiling example](../../scrinium/examples/workbook_read_profile.rs)
