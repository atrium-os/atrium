# Desktop Open from picker confirmation to workbook cells

The first frame showing workbook values arrived a median **562 ms** after confirming a published 64 MiB CSV on UFS and **1,001 ms** on Tessera. Both columns were visible at essentially the same median time on either filesystem. In seven of eight UFS trials the numeric column appeared a frame earlier. Every trial opened the requested workbook for writing in a fresh document jail.

| Elapsed interval | UFS median (range), ms | Tessera median (range), ms |
|---|---:|---:|
| Picker confirmation to first workbook cells | 561.5 (532.9–1,056.6) | 1,001.0 (993.0–1,157.4) |
| Picker confirmation to document process entering main | 135.3 (125.8–271.3) | 182.0 (163.8–191.2) |
| Document main to first workbook cells | 425.8 (407.0–851.8) | 818.3 (811.3–993.4) |
| Picker confirmation to both columns visible | 1,002.2 (996.4–1,631.5) | 1,001.0 (993.0–1,157.4) |

Each column gives independent medians over eight measured opens. The interval to main includes input delivery, file selection and verification, broker handoff, jail creation and process startup. The interval after main includes workbook loading and frame presentation. These headless timings include the compositor’s 200 ms minimum interval between ordinary frames and the cost of capturing earlier frames. Physical display latency requires a separate measurement.

## Frame evidence

The start marker used the compositor’s guest monotonic clock just before dispatching Enter to the picker with the correct filename selected. The end marker used the same clock immediately after composing the first frame containing actual workbook values, before pixel readback and PNG encoding. The input socket, picker, Limen, Portcullis, Sheets launcher and separate document jail all ran in the guest. A second marker in the document process identified its PID at main entry. Portcullis logged the requested filename, and the guest confirmed that PID’s live document JID before closing the window. Across the measured run and warm-ups there were 18 distinct document JIDs.

The timing build archived an image for every distinct pixel signature in the first two visible rows (x = 42–299, y = 104–143). After an initial calibration exposed an intermediate frame, the complete measurement was repeated with this capture enabled. The raw trials below are from that repeat. Independent PNG decoding reproduced the compositor’s signatures. The populated frame showed numeric 1s and distinct seeded text beginning `lxqtneioqf` and `wyykjmgvax`. Pixel comparisons of the intermediate frame matched the populated frame inside the numeric cells and the empty frame inside the text cells.

| Observed frame | Region signature |
|---|---|
| Launcher | `96c664dbb6c4aca5` |
| UFS / Tessera picker | `cdb939736efd80bd` / `f08b99702bf00f7d` |
| Empty grid while reading | `5ae766b5e2474a9e` |
| Numeric cells visible | `756a50ade1229196` |
| Numeric and text cells visible | `b442363e6d618a57` |

## Conditions and reproduction

- One private QEMU/HVF FreeBSD arm64 desktop VM ran kernel `9f2eed03a36c` with four vCPUs and 4 GiB RAM. UFS with soft updates and Tessera used separate 480 MiB partitions on the same raw virtio disk, with QEMU host caching disabled. Scrinium accessed both through nullfs. The identical 65,536-row, two-column CSVs were published with seed 357104. Both retained SHA-256 `7096d1db078e3889946c777a1f13774be594fa197b781c6c7f292d36097c176c`.
- Timing used host-built release Sheets, Scrinium and Fresco from `16c679fa` with the fixture’s existing broker services. To reproduce in a scratch checkout, apply the [timing-only patch](../../scripts/probes/sheets-desktop-open.patch) with `git apply --unidiff-zero`, and build the three binaries for `aarch64-unknown-freebsd`. Build Fresco with `--no-default-features --features cranelift`. Run the 1280×720 headless desktop with `FRESCOD_SPLIT=0` and `FRESCOD_HEADLESS_EVERY_FRAME=1`. The frame signature covers that fixture geometry.
- Use the [workbook probe](../../scripts/probes/sheets-verification.rs) in `publish-only` mode for each file. From the running launcher click Open, choose the filesystem place, select its CSV and press Enter. Keep each workbook mounted and close the clean document before the next trial. One warm-up per filesystem preceded eight measured pairs in alternating order. These are warm repeated opens with a new document jail each time. [Individual trials and monotonic timestamps](data/2026-11-20-sheets-desktop-open.csv)
