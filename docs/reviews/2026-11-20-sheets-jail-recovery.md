# Unsaved Sheets edits after document jail teardown

## Original failure

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

## Recovery fix and verification

The launcher now offers **Recover…**. Portcullis mounts one private recovery volume in a fresh document jail. The workbook replays its own base and journal, and Save As requests a new destination grant. The original CSV is untouched.

The private Tessera-root desktop ran the checked-in gate three consecutive times using two 128 MiB quota-limited Tessera recovery volumes. In each run the journal contained the committed `zebra` edit before SIGKILL. A second workbook, using its own UID inside its own jail, could read its own autosave and could not open the first workbook's journal. Recover restored the edited cell in a new jail. Don’t Save closed the window, cleared the pending marker and left `Original` in the source. The check also passed using two bounded UFS images. A pending workbook from each backing format was recovered after a clean guest shutdown and restart. A separate test interrupted the first generation before its pointer was published and verified explicit discard released the slot. [Desktop gate](../../scripts/sheets-jail-recovery-vm-check.sh)

The FreeBSD standard test profile passed all 96 Sheets library tests. The optimized profile passed 94. Two timing assertions in `a_slow_edit_hands_the_window_back_and_greys_what_is_not_done` and `a_slow_file_is_read_while_the_caller_carries_on` expected more recalculation slices and polling iterations. The host passed all 26 overlay, IPC and peer-registry library tests.

## Two pending workbooks

The launcher now lists the launch-time names when more than one workbook is pending. Selecting an entry rechecks its identity before opening it. Canceling the password prompt closes only that recovery window. The encrypted generation remains pending.

The two-workbook desktop gate edited A1 in a CSV to `zebra` and A1 in a password-protected new workbook to `fox`, then killed both document processes. It canceled the protected recovery, checked both pending markers, and recovered the pair in each order. For each workbook, Save As published a native copy. An ordinary Save added B1 to that copy. An independent reader opened all four results per backing format and checked the cell values after both writes. The sealed copy required the fixture password and rejected a wrong one. The source CSV still contained `Original`.

Both orders passed on native Tessera recovery volumes and bounded UFS fallback images in a Tessera-root VM. A stale chooser entry displayed a refusal and left both recoveries available. The native run also upgraded an older empty marker and recovered its protected workbook. All 96 Sheets library tests and 18 broker IPC and peer-registry tests passed. [Desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) · [Save As verifier](../../atrium-sheets/examples/check_recovered_copy.rs)

## Save As names after a crash

Two document jails opened the same `Proof.csv`. Canceling Save As preserved each workbook's name and recovery identity. Saving both to the same folder produced `Proof.osheet` and `Proof (2).osheet`. After both processes were killed, the chooser displayed those two published names. Each selection restored the corresponding edits and suggested its own published name on the next Save As.

The desktop check passed on native Tessera volumes and bounded UFS images in the isolated Tessera-root VM. An independent reader found A1/B1 values of `fox`/`2` in `Proof (3).osheet` and `zebra`/`2` in `Proof (2) (2).osheet`. The original CSV retained its SHA-256 digest. A FreeBSD broker test also verified that a delayed rename from a previous occupant of a document slot leaves the current workbook's marker intact. The remaining case is an interruption after Scrinium publishes the file and before Portcullis acknowledges the new name. [Desktop gate, run with `--save-as-labels`](../../scripts/sheets-recovery-choices-vm-check.sh)
