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

## Missing Save As acknowledgement

The desktop test disconnected the Portcullis worker socket while Scrinium published `Proof (2).osheet`. Sheets recorded the new name on its private volume. Before the fix, the broker marker still said `Proof.csv` after document teardown and broker restart, and the chooser displayed that stale label. Selecting the row still recovered the correct workbook.

Portcullis now reconciles the marker with the durable autosave name after the jail releases its mounts and before the volume detaches. The same interruption on bounded UFS and native Tessera recovery volumes produced the chooser rows `Proof (2).osheet` and `Proof.osheet` after restart. An independent reader found `zebra,2` and `fox,2` in the recovered copies. The source CSV retained its SHA-256 digest, and both markers cleared after recovery. The native Tessera desktop also passed both recovery orders for a plain and a password-protected workbook, including password cancellation, Save As and subsequent Save. The broker tests checked a stale identity, a symlink, an oversized name, and an ambiguous autosave directory. [Desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh)

The broker that supervised the jails completed teardown before the test restarted it.

## Broker restart with an open workbook

Two document jails opened `Proof.csv`. The second publication received the name `Proof (2).osheet` while its Portcullis worker connection was interrupted. The fixture restarted the broker while that workbook was still open. The workbook kept its jail and recovery mount, accepted a B1 edit, and was then killed.

Before the fix the kernel released the jail, but its root, upper layer, socket mounts and recovery device remained attached. The chooser displayed `Proof.csv`. With the fix, Recover released those mounts and the device before selection. The chooser displayed `Proof (2).osheet` and `Proof.osheet`, and the recovery identity stayed stable. An independent reader verified A1/B1 as `zebra,2` and `fox,2` in the two recovered copies. The original CSV retained its SHA-256 digest.

The desktop gate passed on native Tessera and bounded UFS recovery volumes in the isolated Tessera-root VM. The UFS desktop also passed password cancellation, Save As and subsequent Save with plain and protected workbooks recovered in both orders. Cleanup runs when a subsequent workbook action examines the slot. [Desktop gate, run with `--live-broker-restart` and the private fixture broker executable](../../scripts/sheets-recovery-choices-vm-check.sh)

## Clean close and slot reuse with a sibling open

The original broker left an empty persistent jail when a workbook closed cleanly after the broker had restarted. The window and process had gone, its autosave directory was empty, and the kernel still reported `persist` on the registered document root.

With the updated broker, a clean close released the jail while its sibling stayed open. New reused the same slot and root with a different JID and recovery identity. The new workbook started with blank A1. An independent reader checked all three saved copies: the closed workbook contained `fox`, the new workbook contained only B1=`2`, and the sibling retained A1=`zebra` and accepted B1=`2`. The source CSV retained its SHA-256 digest. After the last close and Recover, there were no document jails, pending markers or recovery devices.

The clean-close desktop gate passed on native Tessera and bounded UFS recovery volumes. On Tessera it also passed when the legacy broker launched both workbooks and the updated broker took over before close. On UFS the live-restart crash regression passed as well. Both focused FreeBSD recovery-marker tests passed. [Desktop gate, run with `--clean-close-reuse` and the private fixture broker executable](../../scripts/sheets-recovery-choices-vm-check.sh)

## Reboot before a cleanly closed slot is reclaimed

Two jails opened `Proof.csv`. Slot 0 held an unsaved A1 edit of `zebra`. Slot 1 saved `fox` as `Proof.osheet` and closed cleanly after Portcullis restarted. The test then killed the sibling and powered down the guest before another workbook action. Both markers persisted. The old broker opened a blank workbook in slot 2 after boot, leaving the empty slot 1 reserved.

The corrected broker inspected the existing images after boot. New reused slot 1 with a fresh recovery ID and published a copy with blank A1 and B1 set to `2`. Recover opened slot 0 directly with its original ID. An independent reader verified `zebra` in its published copy and `fox` in the previously saved workbook. The original CSV retained its SHA-256 digest. Both pending markers cleared and all recovery devices detached.

The two-stage check passed on native Tessera and bounded UFS volumes in a Tessera-root VM. The native check also exercised markers from the old broker. Both plain and password-protected recovery orders passed on UFS. Three focused FreeBSD broker tests passed, including keeping a legacy marker and leaving the backing files absent when its image is missing. [Two-stage desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh)

## Reboot after a missed Save As name update

Two jails opened `Proof.csv` and published `Proof.osheet` and `Proof (2).osheet`. The second publication completed while the Portcullis worker socket was unavailable. Sheets recorded its collision-suffixed name locally. The fixture restarted the broker before that document exited, verified its marker still said `Proof.csv`, and powered down before another workbook action could reconcile it.

After boot the old chooser displayed `Proof.csv` alongside `Proof.osheet` and marked both slots nonempty. The updated broker rechecked those same Tessera images, including the slot the old broker had already inspected. The rendered chooser displayed `Proof (2).osheet` and `Proof.osheet`. The two-stage test also passed with bounded UFS recovery images.

Each selection kept its original ID. An independent reader found `zebra,2` in `Proof (2) (2).osheet` and `fox,2` in `Proof (3).osheet` after recovery and Save As. The source CSV retained its SHA-256 digest. Four focused FreeBSD broker tests passed. [Two-stage desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh)

## Retry after repairing unreadable name metadata

The desktop gate starts with two pending copies. The damaged workbook published `Proof (2).osheet` while its broker name update was interrupted. Its marker still says `Proof.csv`. The fixture saves the original name file and either removes it from its usual path or puts invalid UTF-8 there. The workbook base and journal stay intact.

The chooser displayed `Proof.csv` beside the healthy `Proof.osheet`. Selecting the fallback row restored the damaged workbook and accepted a new B1 edit of `3`. After a second crash and Recover, the broker retried the name read and kept the marker. With both workbooks closed, the fixture restored only the original name file and detached the image. The next Recover displayed `Proof (2).osheet`. Both recovery IDs matched their original values.

An independent reader verified `zebra,3` in the first recovered copy and `fox,2` in its sibling. The source CSV retained its SHA-256 digest. Both faults passed on native Tessera and bounded UFS recovery images in the isolated Tessera-root VM. All recovery devices detached after the copies closed. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--retry-recovery-label`, the fixture broker executable, and either `missing` or `invalid-utf8`.

## A pending workbook’s image goes offline

With a plain and a password-protected workbook pending, the original broker listed both and cached their names. The fixture moved the detached plain image aside. Selecting its row closed the chooser. The launch thread logged the missing image, but the launcher showed no error. Both markers survived.

Portcullis now checks that the backing image exists even when it can reuse a cached label. The launcher displayed “The recovery image for ‘Proof.csv’ is unavailable.” Selecting the protected sibling restored A1=`fox`. Save As and a subsequent Save preserved its protection and added B1=`2`. With only `Proof.csv` pending, Recover again identified the unavailable workbook. Its marker stayed byte-identical and no replacement image appeared.

The fixture restored the original image and checked its SHA-256 digest. Recover reopened the workbook with the same ID and A1=`zebra`. A reader verified the saved copy after adding B1=`2`. The source CSV retained its digest and all pending markers and recovery devices cleared. The [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) accepts `--retry-missing-image before-list` or `--retry-missing-image after-list` to exercise the two timings. Both passed on native Tessera and bounded UFS images in the isolated Tessera-root VM. Five focused FreeBSD broker tests passed.

## A listed recovery image fails to mount

After the chooser cached both names, the fixture replaced the detached image for `Proof.csv` with an unformatted file at the same path. With the previous broker, Tessera rejected the mount and the chooser closed without reporting the failure. With the updated broker, the launcher displayed “The recovery image for ‘Proof.csv’ is unavailable.” It displayed the same error when this was the only pending workbook. The failed selections preserved its marker byte for byte.

Both before-list and after-list substitutions passed with native Tessera and bounded UFS images in the isolated Tessera-root VM. Launcher screenshots confirmed the named error in both selection paths. An independent reader checked the protected sibling’s `fox` edit after Save As and `fox,2` after Save. Restoring the original image with its SHA-256 digest intact allowed Recover to reopen the original ID and save `zebra`, then `zebra,2`. Both workbooks closed with no pending marker or attached recovery device. The UFS missing-image case also passed after listing. Five focused broker tests passed on FreeBSD.

Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--retry-unmountable-image after-list` or `--retry-unmountable-image before-list`.

## Jail setup fails after the recovery image mounts

The desktop gate opened and edited two workbooks, crashed both document processes and listed their pending recoveries. The fixture placed a regular file where Portcullis needed to create the selected workbook’s writable jail layer. Selecting that workbook reached jail setup and the broker logged the expected file-exists error. Both markers remained byte-identical. The recovery device detached, and the failed launch left no document jail or mounts at the jail root or upper layer.

While that path was still blocked, the protected sibling recovered and saved. After the fixture removed the file, the same broker reopened the selected workbook with its original recovery ID and jail path. An independent reader verified A1 after Save As and A1/B1 after a subsequent Save for each workbook (`fox,2` and `zebra,2`). Both copies closed with no pending markers or attached recovery devices.

Both native Tessera and bounded UFS recovery volumes passed in the isolated Tessera-root VM. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--retry-jail-setup`. The setup failure is currently reported in the broker log.

## The launcher identifies the workbook after jail preparation fails

The broker now waits for the document worker to prepare the jail root and mountpoints before acknowledging recovery. With the same blocked upper layer, the launcher displayed “Could not prepare a recovery window for ‘Proof.csv’. Try Recover again.” The desktop gate checks the rendered frame both when the user selects that workbook from two choices and when Recover selects the only remaining workbook.

The gate passed on native Tessera and bounded UFS recovery volumes. The protected sibling saved while the fault was present. After the fixture cleared the fault, retry recovered the original ID in the same jail slot, and the saved copy contained the original edit and a subsequent edit. The source CSV retained its original contents. Both recovery devices detached and both markers cleared. Five focused broker tests passed on FreeBSD.

## Sheets cannot execute after the recovery jail is prepared

The fixture cached both pending workbook names, then temporarily removed execute permission from its Sheets executable. FreeBSD logged a denied exec of `/bin/atrium-sheets` inside the selected document jail. The launcher displayed “Could not start Sheets for ‘Proof.csv’. Try Recover again.” The desktop gate checks the rendered message both for an explicit selection and for the only remaining recovery.

With execute permission restored, the protected sibling recovered and saved. After the second failure, retry reopened the original workbook in its prior jail slot under the same broker, with its recovery ID intact. An independent reader verified the recovered A1 values and subsequent B1 edits in both saved copies. Both markers and devices cleared on close. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--retry-runtime-start`.

Both native Tessera and bounded UFS recovery volumes passed in the isolated Tessera-root VM. The earlier jail preparation failure case and all five focused FreeBSD broker tests also passed. The launcher cleared the error after successful recovery.

## A recovery takes longer than the startup acknowledgement

The private fixture held the selected document’s runtime jail command after the broker had prepared the mountpoints. With the jail still held and no document process running, the launcher displayed “Sheets is still starting recovery for ‘Proof.csv’.” A second Recover press reached the broker and changed the launcher text to “No closed workbook is waiting for recovery”. The original request remained active.

Releasing the command started exactly one document jail. Its path and recovery ID matched the original workbook. An independent reader verified A1 was `zebra` after Save As and A1/B1 were `zebra,2` after Save. The protected sibling had already recovered and saved `fox,2`. Both documents closed with no pending markers or recovery devices. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--delayed-runtime-start` and the private fixture broker path.

The gate passed with native Tessera and bounded UFS recovery volumes in the isolated Tessera-root VM. Both produced the same initial and repeated-press messages.

## A second Recover press retains the named startup notice

The broker now remembers a delayed recovery in its reserved slot and checks for the Sheets process when Recover is pressed again. The desktop gate waits for the second press to reach the broker and compares the resulting launcher screenshot byte for byte with the first. Both frames displayed “Sheets is still starting recovery for ‘Proof.csv’.”

Native Tessera and bounded UFS recovery volumes each recorded one runtime jail start across the two presses. Releasing the hold opened the original recovery ID in its previous jail slot. An independent reader verified `zebra` after Save As and `zebra,2` after Save. The execution-failure desktop regression and five focused FreeBSD broker tests also passed.

## Recovering a sibling during a delayed launch

With two edited workbooks pending, the fixture held the runtime launch for `Proof.csv`. Its startup notice stayed visible with a selectable `Untitled` row beneath it. The protected sibling opened directly from that row and also after a fresh Recover press in a separate run. An independent reader verified `fox` after Save As and `fox,2` after Save. The sibling closed with the first launch still held.

Releasing the hold opened `Proof.csv` in its original slot with the same recovery ID. The reader verified `zebra` and then `zebra,2`. The fixture counted one start for the delayed jail. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--delayed-start-sibling`, the private fixture broker path, and `direct` or `refresh`.

Direct and refreshed selection passed on native Tessera. Refreshed selection passed with bounded UFS volumes in the same isolated Tessera-root VM. The single-pending-workbook regression, all seven IPC tests, and five focused recovery-marker tests also passed.

## A listed sibling goes offline during a delayed launch

The launcher showed the startup notice for `Proof.csv` with `Untitled` still selectable. The fixture confirmed that only the first recovery image was attached, moved the sibling’s image aside, and selected its existing row. The launcher displayed “The recovery image for ‘Untitled’ is unavailable.” Both recoveries remained pending, the sibling marker stayed byte-identical, and no document jail had opened while the first launch was held.

With the sibling image still offline, releasing the first launch reopened `Proof.csv` once, in its original slot and with the same recovery ID. An independent reader verified `zebra` after Save As and `zebra,2` after Save. Restoring the sibling image with its original SHA-256 digest allowed the protected workbook to recover and save `fox,2`. Both markers and recovery devices cleared after close. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--delayed-start-sibling`, the private fixture broker path, and `unavailable`.

The gate passed on native Tessera and bounded UFS recovery volumes in the isolated Tessera-root VM. Both runs matched the same named-error screenshot.

## A listed sibling’s recovery ID changes during a delayed launch

After the chooser showed `Untitled` beneath the startup notice for `Proof.csv`, the fixture atomically changed only the sibling marker’s ID. Its label, slot, and autosave contents stayed the same. Selecting the cached row displayed “That recovery is no longer available. Choose Recover again.” Both markers remained pending, the replacement marker stayed byte-identical, and the runtime launch log contained only the held start for `Proof.csv`.

Releasing the hold opened the first workbook once in its original jail slot with its original ID. An independent reader checked `zebra` after Save As and `zebra,2` after Save. Once the first workbook closed, a fresh Recover started the protected sibling. Its running Sheets process received the replacement ID, `41`. The saved sibling copy contained `fox,2`. Both recovery devices detached and both markers cleared. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--delayed-start-sibling`, the private fixture broker path, and `stale-id`.

The stale-selection gate passed with native Tessera and bounded UFS recovery volumes in the isolated Tessera-root VM.

## Refreshing the chooser after a stale sibling selection

The fixture kept the first runtime launch on hold after Sheets rejected the stale sibling row. Pressing Recover again restored the startup notice for `Proof.csv` and the selectable `Untitled` row. Refreshing left the launch count unchanged. Clicking the row opened the protected sibling in a separate jail, and its process arguments confirmed that it received the replacement recovery ID, `41`.

Before releasing the first launch, the gate unlocked the sibling, saved it under a new name, added a cell edit, saved again, and closed it. The first workbook then opened once with its original ID. An independent reader verified `fox` followed by `fox,2` in the sibling copy and `zebra` followed by `zebra,2` in the first copy. Both pending markers cleared and both recovery devices detached. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--delayed-start-sibling`, the private fixture broker path, and `stale-refresh`.

The refreshed-row gate passed with native Tessera and bounded UFS recovery volumes in the isolated Tessera-root VM.

## Interleaved saves from two live recovered workbooks

The replacement sibling opened with recovery ID `41`, unlocked, and used Save As to publish `Untitled.osheet`. With that document still open, the fixture released the held launch. `Proof.csv` opened in a second jail under its original recovery ID and published `Proof.osheet`. Its Save As left the sibling file byte-identical.

The fixture tiled the documents and gave keyboard focus to the target window before each B1 edit. It then alternated ordinary Save four times. An independent reader checked both published files after every turn.

| Document saved | Plain copy (A1, B1) | Protected copy (A1, B1) |
| --- | --- | --- |
| Protected | zebra, empty | fox, 3 |
| Plain | zebra, 4 | fox, 3 |
| Protected | zebra, 4 | fox, 5 |
| Plain | zebra, 6 | fox, 5 |

Each turn checked both live jails, the caller UID in Scrinium’s trace, a changed digest for the intended destination, and an unchanged digest for its sibling. The reader checked the protected file with the fixture password and confirmed that a wrong password failed. The source CSV still contained `Original`. Run the [desktop gate](../../scripts/sheets-recovery-choices-vm-check.sh) with `--delayed-start-sibling`, the fixture broker path, and `stale-refresh interleave`. Cross-build the [window tiling helper](../../atrium-sheets/examples/tile_recovery_windows.rs) on the host and stage it as `/tmp/tile-recovery-windows` in the private guest.

The interleaved-save gate passed with native Tessera and bounded UFS recovery volumes in the isolated Tessera-root VM. Both runs cleared the pending markers and detached their recovery devices.

## Restarting Limen with both recovered workbooks open

Append `restart-limen` to the interleaved-save gate above. After both Save As operations complete, the test records both Sheets PIDs in their original jails and the Scrinium PID. It verifies that all three are connected to the fixture Limen, stops it, waits for the process and listener to disappear, then starts a new Limen. The four ordinary saves begin only after the new broker has accepted connections from all three original processes.

The same per-turn checks apply across the restart. Each Scrinium request carries the saving jail’s UID. An independent reader checks the contents of both files, including password protection, and the idle sibling’s digest stays unchanged. The final copies contain `zebra,6` in Proof.osheet and `fox,5` in Untitled.osheet.

Both runs passed in the isolated Tessera-root VM, one with native Tessera recovery volumes and one with bounded UFS volumes. Both document jails closed cleanly and released their recovery devices.

## Restarting Limen during a pending Save

Run the interleaved-save gate with `stale-refresh interleave pending-save`. After both Save As operations, the fixture enters B1=4 in the plain workbook, pauses Limen, and checks that its receive queue for that document is empty. It presses Save and waits for bytes on that connection before terminating Limen. This puts the interruption after Sheets submitted the request and before Scrinium could publish it.

After the same two Sheets processes and Scrinium reconnect, the gate checks both file digests against their pre-restart values. It saves B1=3 in the protected sibling first. The independent reader must see `fox,3` there and only `zebra` in the plain copy. Pressing ordinary Save again in the plain window, without re-entering the edit, must produce `zebra,4` and preserve the sibling byte-for-byte. Two more alternating saves exercise both grants again.

The pending Save test passed with native Tessera and bounded UFS recovery volumes in the isolated Tessera-root VM. Both runs finished with the plain copy at `zebra,6`, the protected copy at `fox,5`, and the source CSV intact. Both jails closed and released their recovery devices.

## Restarting Limen after publication and before confirmation

Run the interleaved-save gate with `stale-refresh interleave published-save`. Both recovered workbooks first use Save As. The plain workbook then submits B1=2. The fixture uses guest LLDB to stop Scrinium at the atomic rename, pauses Limen, and releases Scrinium to publish. An independent reader verifies `zebra,2` on disk while Limen is still paused. The fixture enters B1=4 in the same window and terminates Limen before it can deliver confirmation.

After the two original Sheets processes and Scrinium reconnect, the protected sibling saves `fox,3` while the plain file still contains `zebra,2`. Pressing Save in the plain window without re-entering B1 must publish `zebra,4` and leave the protected file byte-identical. Two further alternating saves exercise both grants. Each turn checks the caller UID, reads both copies and verifies the idle sibling’s digest.

Both the native Tessera and bounded UFS recovery-volume runs passed in the private Tessera-root VM. They finished with `zebra,6` and `fox,5` in the respective files. Both jails closed, both markers cleared, and both recovery devices detached.

## External edit after reconnect

Run the interleaved-save gate with `stale-refresh interleave external-edit`. Scrinium publishes B1=2 in the plain workbook while Limen is paused. Sheets retains the later B1=4 edit. Once both original Sheets processes and Scrinium reconnect, a fixture process restores the earlier plain workbook bytes in place. This clears B1 on disk while keeping A1=`zebra`.

The test retries ordinary Save and matches the desktop frame showing B1=4 and the warning “The workbook changed since it was opened. Reopen it or use Save As.” The protected sibling then saves B1=3. Without retyping the pending edit, the plain workbook uses Save As in its tiled window. The independent reader checks `zebra,4` in `Proof (2).osheet` and `fox,3` in the protected sibling. The test also compares the external file byte-for-byte with the outside writer’s copy and checks that Save As leaves the sibling’s digest unchanged.

The gate passed on both native Tessera and bounded UFS recovery volumes in the isolated Tessera-root VM. After both jails closed, all three archived workbooks from each run passed the independent reader checks. Both markers cleared and both recovery devices detached.

## Ordinary Save on the conflict copy and desktop reopen

The `external-edit` gate continues in the same plain window after Save As. It enters C1=7 and presses ordinary Save. Scrinium must see the plain jail’s UID and only `Proof (2).osheet` may change. The gate checks the new copy contains `zebra,4,7`, compares the external file with the outside writer’s bytes, and checks the protected sibling’s digest.

After closing both recovered windows and detaching their devices, the gate uses the launcher's Open picker for each file. It unlocks the protected sibling with its fixture password. For each open it checks the document jail, the loaded filename and the active autosave snapshot created by that jail from the newly loaded workbook.

The first desktop run reproduced a picker refusal for the sealed `Untitled.osheet`. The shared file-type table now recognizes the version-one sealed envelope when the filename has the workbook suffix. The envelope is common to Officina documents, so suffix-free sealed bytes alone do not identify a spreadsheet. All nine host file-type tests passed, including checks for a truncated header and an unsupported envelope version.

The full desktop test passed with both native Tessera and bounded UFS recovery volumes in the private Tessera-root VM. Fresh document jails reopened `Proof.osheet` with A1=`zebra` and blank B1, `Untitled.osheet` with `fox,3`, and `Proof (2).osheet` with `zebra,4,7`. Each close cleared its recovery marker and detached its volume.

## Wrong password, retry and cancellation from Open

The `external-edit` desktop gate selects `Untitled.osheet` from the Sheets Open picker twice. Each time it submits a wrong password and checks the rendered retry prompt names the file. On the first open it submits the correct password in the same jail and reads the loaded protected snapshot to verify A1=`fox` and B1=`3`. On the second it clicks Cancel at the retry prompt, checks the status says the file stayed closed, and reads the empty Untitled window’s snapshot before closing it.

After each path the gate checks all three published files against their pre-prompt bytes or digests. It then reopens both plain workbooks through the picker and reads their loaded cells.

Both password paths passed on native Tessera and bounded UFS recovery volumes. The reopened external revision held A1=`zebra` with B1 blank, and the Save As copy held `zebra,4,7`. Each window closed cleanly and released its recovery volume. The fixture window clips the last words of the wrong-password retry guidance.

## Password prompt width

In the 1100 px document window, the previous 300 px label clipped the end of “Try again”. Sheets now measures the prompt with Pergola using the same semibold style it draws. The retry frame shows the complete message `Wrong password for “Untitled.osheet”. Try again`. The input starts at x=360, with a gap after the final letter. Open and Cancel retain their positions. The desktop gate matches the initial and retry frames before entering the correct password or cancelling.

The visual checks, correct-password retry, cancellation and plain-file integrity checks passed with both Tessera and UFS recovery volumes in the isolated VM.

## Long workbook name in a tiled password prompt

The original single-line retry prompt ran behind the password field and buttons in a 640 px tile. The regression opens a protected workbook named `Year End Consolidated Financial Statements and Regional Revenue Reconciliation - Final Audit Committee Review.osheet` and tiles it beside the plain workbook. When the text would crowd the field, Sheets uses Pergola’s measured line breaks across the tile and places the controls below the last line. Short prompts still use the single-row layout.

The retry frame shows the name through `Review.osheet` and ends with “Try again” on the second line. The test compares both the initial and retry frames, then types the correct password without clicking the field after tiling. The loaded protected workbook must contain `fox,3`. The password field has a stable focus ID across changes to the number of prompt lines.

The final desktop gate passed on native Tessera and bounded UFS recovery volumes, including the existing 1100 px prompt checks and the two new 640 px frames.

## Choosing the long-named protected workbook after a crash

The desktop gate enters C1=8 in the unlocked audit workbook and D1=9 in its plain sibling, waits for the edits to reach the journals, and kills both Sheets processes without saving. Two pending workbooks force the launcher to show the chooser. The original 440 px button centered the long label partly outside the window, hiding its beginning.

The chooser now measures the button label, expands the row when space permits, and wraps the label within a taller button when necessary. The gate captures the 1100 px and 640 px chooser layouts and selects the protected workbook from the second line of its tiled row. After unlocking in a fresh jail it uses Save As, then reads the resulting protected workbook independently. It also recovers the plain sibling and compares the original published files with their pre-crash revisions.

Both runs passed on native Tessera and bounded UFS recovery volumes. The protected copy contained `fox,3,8` and the plain copy contained `zebra,4,7,9`. The original revisions matched their pre-crash contents.

## Canceling and retrying the protected recovery

The gate cancels the Restore password prompt before entering a password. It checks that the document jail exits, the recovery volume detaches, both pending workbook names and IDs survive, and the published files stay intact. It then chooses the same filename from the 640 px launcher and checks that a fresh jail mounts the original journal byte for byte. After unlocking, the protected copy must contain `fox,3,8`. The sibling must still recover `zebra,4,7,9`.

The final desktop gate passed with both native Tessera and bounded UFS recovery volumes.

## Wrong password during protected recovery

After reopening the long-named recovery from the chooser, the gate submits a wrong password. It checks the rendered workbook name and retry guidance, the same open jail, both pending recoveries, and the journal digest. It then types the correct password without refocusing the field. Save As must produce a protected copy with `fox,3,8`. The plain sibling must still recover `zebra,4,7,9`.

The final gate passed end to end on UFS. The native Tessera trial captured the same retry frame, verified the original journal digest, and published the protected copy. Its first attempt to save the sibling reached Save As before the workbook window appeared. After the window appeared, the continuation saved and independently read the plain copy. The gate now waits for the workbook windows before interacting with them.

## Canceling from the wrong-password recovery prompt

The gate cancels the protected recovery twice, first from Restore and then from the retry prompt after a wrong password. After each cancellation it checks that the jail exits and the recovery volume detaches. It compares both pending workbook names and IDs after cancellation and again after reopening from the chooser. The reopened jail must see the original journal digest. The final unlock and Save As must produce a protected copy with `C1=8`, and the plain sibling must still recover `D1=9`.

The desktop gate passed end to end on native Tessera and bounded UFS recovery volumes. The original published workbooks matched their pre-crash contents.

## Saving the plain sibling while protected recovery waits

The gate tiles the launcher beside the long-named protected workbook while it waits at its Restore password prompt. It recovers the plain sibling and saves a copy with `D1=9` while both document jails are open. The protected jail keeps the same process and journal through the plain save. After the plain window closes, the gate checks that Restore still names the protected workbook, unlocks it, and saves a protected copy with `C1=8`. A final digest check verifies the plain copy still holds the revision published before unlock.

The complete desktop gate passed on bounded UFS recovery volumes. On native Tessera, the same sequence passed in stages while the fixture window placement and focus controls were adjusted. Independent reads verified both published copies on both filesystems.

## Restarting Limen while protected recovery waits

The desktop gate publishes the plain sibling, then leaves a new edit in its live window with the protected workbook still waiting at Restore. It restarts Limen and checks that the original Sheets and Scrinium processes connect to the replacement listener. Ordinary Save in the plain window must update its existing copy to `zebra,4,7,9,6`. The protected recovery journal must retain its digest through the restart and the sibling save. The same protected window must still show Restore, accept the password, and publish a protected copy with `fox,3,8`. That save must leave the plain copy byte-for-byte intact.

The complete gate passed on native Tessera and bounded UFS recovery volumes. Both original workbooks matched their pre-crash contents.

## Interrupting the plain Save while protected recovery waits

The gate commits `E1=6` in the plain sibling and pauses Limen. It presses Save once and checks that the receive queue for that Sheets process grows from zero before terminating Limen. That shows the request reached the broker socket while delivery to Scrinium was blocked. Both Sheets processes and Scrinium must connect to the replacement listener. The original plain copy must still match its pre-request digest, and the locked workbook's recovery journal must still match its pre-crash digest.

The retry presses only Save. An independent read must find `zebra,4,7,9,6` in the same plain file. The protected prompt must still identify the long-named workbook. Unlocking and publishing its protected copy must recover `fox,3,8` and leave the retried plain revision intact.

The complete desktop gate passed on native Tessera and bounded UFS recovery volumes. Both original workbooks matched their pre-crash contents.

## Protected recovery closeout

The desktop gate runs the protected recovery cases together. It cancels both password prompts, reopens the same recovery ID and journal, then saves the plain sibling while the protected workbook waits at Restore. It checks three points in the plain save flow while that same protected window remains open.

- Before delivery, the gate verifies Save has reached Limen's receive queue, restarts Limen, and retries without retyping the edit. The existing copy must contain `E1=6`.
- After publication, the gate pauses Scrinium at the rename, freezes Limen before releasing it, and reads `F1=7` from disk before terminating Limen. A later `G1=8` edit must save to the same destination after reconnect.
- With an external revision, the gate repeats the unconfirmed publication with `H1=9`, writes a valid earlier revision to the same file, and checks the conflict message. Ordinary Save must preserve that external revision. Save As must preserve the local `H1=9` and `I1=1` in a separate copy.

The protected journal must match through the Limen restarts and sibling saves. The protected copy must contain `C1=8` after unlocking, and its Save As must preserve both plain outputs byte for byte.

The revised gate passed end to end on bounded UFS recovery volumes. On native Tessera, the run reached the expected conflict and captured the reference frame. A continuation verified Save As, protected unlock, and the final file contents in the same live jails.
