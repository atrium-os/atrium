# Unsaved Sheets edits after document jail teardown

A committed cell edit reached the live autosave journal. Killing the Sheets document process triggered normal jail cleanup, which unmounted the tmpfs containing both the journal and its base. Starting Sheets again showed the launcher. Reopening the CSV showed the last saved value.

## Desktop reproduction

The private arm64 desktop VM used a Tessera root, FreeBSD kernel `9f2eed03a36c`, and the instrumented release Sheets binary from the autosave profile (SHA-256 `6f0ff04be3c5f2729673ecdb596d893c6a7f36ba1b6791bcddf58b9470bb6bf4`). The test used a dedicated Scrinium place containing a single CSV. [Autosave profile and fixture](2026-11-20-sheets-autosave-base.md)

1. Open `Proof.csv` through the launcher's picker. Its only cell contains `Original`. Record the rendered workbook frame and confirm the initial journal is empty.
2. Replace A1 with `zebra`, press Enter, and wait three seconds for the two-second journal sync interval. The frame changed, the 36-byte journal contained the literal edit, and reading the source CSV still returned `Original`. The live mount table traced the autosave path through unionfs to the document slot's tmpfs upper layer.
3. Send SIGKILL to the PID verified to belong to the document jail. Wait until its JID and mounts disappear. Both the generation pointer and journal paths disappeared, and the original launcher became visible. This was observed in two independent document processes.
4. Close the launcher and wait for its jail and window to disappear. Launch Sheets again through Portcullis. It rendered the launcher. Open the same source through the picker. The new document had an empty journal and rendered the exact original frame. The source CSV still contained `Original`.

In the completed run the crashed document was PID 3873 in jail 4. The restarted launcher was jail 5 and reopened the CSV in jail 6, PID 4055. Frame signatures for the original, edited and reopened workbook were `63d21a3ad8d4dda7`, `1915e3f75244f78d` and `63d21a3ad8d4dda7`.

## Recovery boundary

The Sheets startup scan reads the launching process's autosave root. A dedicated workbook skips that scan. Its journal lives inside its own temporary writable layer, so the launcher has no surviving generation to discover after teardown. The earlier live-copy test covers the recovery reader; this desktop test identifies where its input is lost.

The next implementation needs private recovery storage that outlives the slot and a way for the launcher to reopen one pending generation in a fresh document jail. Test that a second open workbook cannot read the first one's journal and that explicit discard clears the pending recovery.
