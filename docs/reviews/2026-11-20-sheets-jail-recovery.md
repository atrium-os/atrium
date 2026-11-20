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
