#!/bin/sh
# Private 1280x720 Sheets headless fixture. The single Scrinium place holds
# Proof.csv, with A1=Original. Stage the check_recovered_copy example locally.
# Pass --save-as-labels for two copies of Proof.csv and collision-suffixed names.
# --interrupted-labels /absolute/path/to/fixture-broker also disconnects the
# workers socket during Save As and restarts that broker after document teardown.
# --live-broker-restart and --clean-close-reuse take the same path and restart
# with a document open, then exercise crash recovery or clean close and New.
# For a clean-close upgrade test, pass the legacy executable then its replacement.
# --prepare-reboot-reuse takes the broker path. Power-cycle the private VM and
# restart the fixture, then pass the printed directory to --verify-reboot-reuse.
# --prepare-reboot-labels and --verify-reboot-labels exercise a missed rename.
# --retry-recovery-label takes the fixture broker path. Its optional third argument
# selects a missing name (missing, the default) or unreadable text (invalid-utf8).
# --retry-missing-image [after-list|before-list] takes one image offline until its sibling is saved.
# --delayed-start-sibling keeps the protected workbook selectable; pass direct, refresh, unavailable, stale-id, or stale-refresh after the broker path.
# With stale-refresh, append interleave to keep both documents open. Stage the tile_recovery_windows example at /tmp/tile-recovery-windows.
# Append restart-limen after interleave to restart the private Limen between Save As and Save.
# Use pending-save instead of restart-limen to interrupt an ordinary Save before delivery.
# Use published-save to interrupt confirmation after publication (requires guest LLDB).
# Use external-edit to exercise the conflict, reopen all three files, and cancel and retry protected crash recovery.
# --delayed-runtime-start takes the private broker path and delays one document jail start.
# --retry-runtime-start temporarily removes execute permission from the fixture Sheets binary.
# --retry-jail-setup blocks creation of the selected jail’s upper layer after mount.
# --retry-unmountable-image leaves an unformatted fixture file at the image path.
set -eu
cd /var/tmp/sheets-desktop
books=/var/tmp/sheets-select-input
if test "${1:-}" = --verify-reboot-reuse || test "${1:-}" = --verify-reboot-labels; then
    results=${2:?pass the result directory printed before shutdown}
else
    results=$(mktemp -d /var/tmp/sheets-select-results.XXXXXX)
fi
test "$(cat "$books/Proof.csv")" = Original
input() { printf '%s\n' "$@" | nc -N -U inject.sock >/dev/null; }
click() { input "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; }
key() { input "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; }
type_keys() { for k in "$@"; do key "$k"; done; }
edit() { click "$1" 112; shift; type_keys "$@" 0x28; }
wait_for() {
    for i in $(jot 120); do "$@" && return 0; sleep .1; done
    echo "Timed out waiting for $*" >&2; return 1
}
docs() { jls -n jid name | awk '/org_atrium_sheets__document_/ {sub(/^jid=/,"",$1);print $1}'; }
count() { test "$(docs | wc -w)" -eq "$1"; }
root() { jls -j "$1" path; }
win_at() {
    input "MOVE $1 $2"
    awk -v x="$1" -v y="$2" '$2=="inject" && $3=="MOVE" && $4==x && $5==y {w=$NF} END{print w}' select-fresco.log
}
different_window() { win=$(win_at "$1" "$2"); test "$win" -gt 0 && test "$win" != "$3"; }
pending() { test "$(find /home/alice/.atrium/apps -maxdepth 2 -name '*.pending' | wc -l)" -eq "$1"; }
kill_doc() {
    pid=$(ps -axo jid,pid,comm | awk -v j="$1" '$1==j && $3~/atrium-sheets$/ {print $2}')
    test "$(ps -p "$pid" -o jid= | tr -d ' ')" = "$1"
    kill -KILL "$pid"
}
opened() { test -n "$(find "$(root "$1")/atrium/recovery" -name name -exec grep -lx "$2" {} +)"; }
make_pair() {
    wait_for count 0
    test ! -e "$books/Proof.osheet" && test ! -e "$books/Untitled.osheet"
    launcher=$(win_at 48 24)
    click 48 24
    wait_for different_window 488 183 "$launcher"
    picker_window=$win
    click 488 183; sleep .5; key 0x28
    wait_for count 1; plain=$(docs)
    plain_root=$(root "$plain")
    wait_for opened "$plain" Proof.csv
    plain_window_visible() { different_window 515 24 "$launcher" && test "$win" != "$picker_window"; }
    wait_for plain_window_visible
    plain_window=$win
    edit 62 0x1d 0x08 0x05 0x15 0x04 # zebra
    click 515 24; wait_for count 2
    protected=$(docs | grep -vx "$plain")
    wait_for opened "$protected" Untitled
    protected_window_visible() { different_window 677 24 "$plain_window" && test "$win" != "$launcher"; }
    wait_for protected_window_visible
    click 677 24; sleep .4
    type_keys 0x17 0x08 0x16 0x17 0x28 # fixture password: test
    private="$(root "$protected")/atrium/recovery"
    sealed() { g=$(find "$private" -name current -type f); test -n "$g" && test "$(head -c 15 "$(dirname "$g")/base.$(cat "$g").osheet")" = OFFICINA-SEALED; }
    wait_for sealed
    edit 62 0x09 0x12 0x1b # fox
    sleep 2.2
    generation=$(cat "$g")
    test -s "$(dirname "$g")/journal.$generation"
    kill_doc "$plain"; kill_doc "$protected"
    wait_for count 0; wait_for pending 2
}
choose() {
    click 213 24; sleep .6
    if test "$1" = plain; then click 120 96; else click 120 136; fi
    wait_for count 1
    sleep .6
}
save_copy() {
    if test "$1" = protected; then
        type_keys 0x17 0x08 0x16 0x17 0x28
        stem=Untitled; cell=fox
    else
        stem=Proof; cell=zebra
    fi
    sleep .6
    docwin=$(win_at 205 24)
    click 205 24
    wait_for different_window 824 457 "$docwin"
    click 824 457
    out="$books/$stem.osheet"
    wait_for test -s "$out"
    /tmp/check-recovered-copy "$out" "$cell" "$1"
    # A later ordinary Save must use the grant for this new file.
    previous=$(sha256 -q "$out")
    sleep .5; edit 129 0x1f; click 125 24
    changed() { test "$(sha256 -q "$out")" != "$previous"; }
    wait_for changed
    /tmp/check-recovered-copy "$out" "$cell,2" "$1"
    key 0x1a 1
    wait_for count 0
    mv "$out" "$results/$first-$stem.osheet"
    test "$(cat "$books/Proof.csv")" = Original
}
# Open the same file twice. The first published copy takes the suggested
# name and the second gets Scrinium's collision suffix.
open_proof() {
    parent=$(win_at 48 24)
    click 48 24
    wait_for different_window 488 183 "$parent"
    click 488 183; sleep .5; key 0x28
    wait_for count "$1"
}
publish() {
    parent=$(win_at 205 24)
    click 205 24
    wait_for different_window 824 457 "$parent"
    click 824 457
    wait_for test -s "$books/$1"
    /tmp/check-recovered-copy "$books/$1" "$2" "${3:-plain}"
    back_in_document() { test "$(win_at 824 457)" = "$parent"; }
    wait_for back_in_document
}
check_name() {
    test "$(cat "$1/name")" = "$3" && grep -Fq "\"name\":\"$3\"" "$2"
}
fixture_broker() {
    broker=${1:?pass the running private fixture broker executable}
    workers=/atrium/sockets/workers/workers.sock
    control=/atrium/sockets/portcullis/portcullis.sock
    test -x "$broker" && test -S "$workers"
    broker_pid=$(pgrep -f "^$broker --socket $control$")
    test "$(procstat -b "$broker_pid" | awk 'NR==2 {print $NF}')" = "$broker"
}
restart_broker() {
    kill -TERM "$broker_pid"
    stopped() { ! kill -0 "$broker_pid" 2>/dev/null; }
    wait_for stopped
    env PORTCULLISD_FRESCO_ACTIVATION="$PWD/activation.sock" daemon -f -o "$PWD/select-port.log" "$broker" --socket "$control"
    ready() { sockstat -ulw | grep -F "$workers" >/dev/null; }
    wait_for ready
}
recover_named_pair() {
    # Slot zero is the copy with the suffix, although it was saved second.
    click 120 96; wait_for count 1; sleep .6
    publish 'Proof (2) (2).osheet' "${1:-zebra,2}"
    key 0x1a 1; wait_for count 0; wait_for pending 1
    click 213 24; wait_for count 1; sleep .6
    publish 'Proof (3).osheet' 'fox,2'
    key 0x1a 1; wait_for count 0; wait_for pending 0
    test "$(sha256 -q "$books/Proof.csv")" = "$original"
    for copy in "$books"/*.osheet; do mv "$copy" "$results/"; done
    echo "Both recovered names, identities, edits and suggested Save As names verified. Frames and copies: $results"
}
# Repair this fixture's private name with no workbook or broker mount open.
repair_fixture_name() (
    count 0; detached
    store=$(dirname "$marker")
    if test -f "$store/0.ufs"; then
        image="$store/0.ufs"; fs=ufs; options=nosuid,noexec
    else
        image="$store/0.img"; fs=tessera; options=tessera.quota_bytes=134217728
    fi
    repair="$results/repair"
    mkdir "$repair"
    unit=$(mdconfig -a -t vnode -f "$image")
    trap 'mdconfig -d -u "$unit"' EXIT
    mount -t "$fs" -o "$options" "/dev/$unit" "$repair"
    trap 'umount "$repair" && mdconfig -d -u "$unit"' EXIT
    saved=$(find "$repair/autosave" -name name.saved -type f)
    test "$(cat "$saved")" = 'Proof (2).osheet'
    if test "$fault" = missing; then
        test ! -e "${saved%/*}/name"
    else
        cmp "$results/unreadable-name" "${saved%/*}/name"
    fi
    mv "$saved" "${saved%/*}/name"
    umount "$repair"; mdconfig -d -u "$unit"; trap - EXIT
    rmdir "$repair"
)
if test "${1:-}" = --verify-reboot-labels; then
    test "$(sysctl -n kern.boottime)" != "$(cat "$results/boot")"
    original=$(cat "$results/source.sha256")
    wait_for count 0; wait_for pending 2
    marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name 0.pending)
    sibling="$(dirname "$marker")/1.pending"
    grep -Fq '"name":"Proof.csv"' "$marker"
    grep -Fq '"name":"Proof.osheet"' "$sibling"
    test -z "$(mdconfig -lv | grep sheets-recovery)"
    click 213 24
    if ! wait_for grep -Fq '"name":"Proof (2).osheet"' "$marker"; then
        cp selectscreen.png "$results/stale-boot-chooser.png"
        exit 1
    fi
    for slot in 0 1; do
        sed 's/,"name":.*//' "$(dirname "$marker")/$slot.pending" > "$results/after.id"
        sed 's/,"name":.*//' "$results/$slot.before" | cmp -s - "$results/after.id"
    done
    sleep .6; cp selectscreen.png "$results/recovery-names.png"
    recover_named_pair
    exit 0
fi
if test "${1:-}" = --verify-reboot-reuse; then
    test "$(sysctl -n kern.boottime)" != "$(cat "$results/boot")"
    wait_for count 0; wait_for pending 2
    target_marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name 1.pending)
    sibling_marker="$(dirname "$target_marker")/0.pending"
    cmp "$target_marker" "$results/closed.marker"
    cmp "$sibling_marker" "$results/sibling.marker"
    test -z "$(mdconfig -lv | grep sheets-recovery)"
    click 119 24; wait_for count 1
    replacement=$(docs); wait_for opened "$replacement" Untitled
    grep -Fq '"name":"Untitled"' "$target_marker" || { echo "New skipped the closed slot" >&2; exit 1; }
    sed 's/,"name":.*//' "$results/closed.marker" > "$results/closed.id"
    sed 's/,"name":.*//' "$target_marker" > "$results/replacement.id"
    if cmp -s "$results/closed.id" "$results/replacement.id"; then exit 1; fi
    wait_for pending 2
    grep -Fq '"checked_detached":true' "$sibling_marker"
    sleep .6; edit 129 0x1f
    publish Untitled.osheet ',2'
    key 0x1a 1; wait_for count 0; wait_for pending 1
    click 213 24; wait_for count 1
    restored=$(docs); wait_for opened "$restored" Proof.csv
    grep -Fq '"checked_detached":false' "$sibling_marker"
    sed 's/,"name":.*//' "$sibling_marker" > "$results/restored.id"
    sed 's/,"name":.*//' "$results/sibling.marker" | cmp -s - "$results/restored.id"
    sleep .6; publish 'Proof (2).osheet' zebra
    key 0x1a 1; wait_for count 0; wait_for pending 0
    /tmp/check-recovered-copy "$books/Proof.osheet" fox plain
    test "$(sha256 -q "$books/Proof.csv")" = "$(cat "$results/source.sha256")"
    test -z "$(mdconfig -lv | grep sheets-recovery)"
    for copy in "$books"/*.osheet; do mv "$copy" "$results/"; done
    echo "Reused the closed slot and recovered the sibling after reboot. Copies: $results"
    exit 0
fi
if test "${1:-}" = --clean-close-reuse || test "${1:-}" = --prepare-reboot-reuse; then
    fixture_broker "${2:-}"
    wait_for count 0; wait_for pending 0
    test "$(ls -A "$books")" = Proof.csv
    original=$(sha256 -q "$books/Proof.csv")
    open_proof 1; sibling=$(docs)
    wait_for opened "$sibling" Proof.csv
    sleep .6; edit 62 0x1d 0x08 0x05 0x15 0x04 # zebra
    open_proof 2; closing=$(docs | grep -vx "$sibling")
    wait_for opened "$closing" Proof.csv
    sleep .6; edit 62 0x09 0x12 0x1b # fox
    publish Proof.osheet fox
    old_root=$(root "$closing")
    target_marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name 1.pending)
    sibling_marker="$(dirname "$target_marker")/0.pending"
    wait_for grep -Fq '"name":"Proof.osheet"' "$target_marker"
    cp "$target_marker" "$results/closed.marker"
    cp "$sibling_marker" "$results/sibling.marker"
    if test -n "${3:-}"; then broker=$3; test -x "$broker"; fi
    restart_broker
    test "$(root "$closing")" = "$old_root"
    # The desktop process can exit before a legacy persistent jail is released.
    doc_process() { ps -axo jid,comm | awk -v j="$1" '$1==j && $2~/atrium-sheets$/ {found=1} END {exit !found}'; }
    doc_process "$closing"
    key 0x1a 1
    closed() { ! doc_process "$closing"; }
    wait_for closed
    if test -n "${3:-}"; then
        test "$(jls -j "$closing" persist)" = true
    else
        wait_for count 1
    fi
    doc_process "$sibling"
    contents=$(ls -A "$(dirname "$target_marker")/1/autosave")
    test -z "$contents"
    if test "$1" = --prepare-reboot-reuse; then
        kill_doc "$sibling"; wait_for count 0; wait_for pending 2
        cmp "$target_marker" "$results/closed.marker"
        cmp "$sibling_marker" "$results/sibling.marker"
        sysctl -n kern.boottime > "$results/boot"
        echo "$original" > "$results/source.sha256"
        sync
        echo "Reboot and run --verify-reboot-reuse $results"
        exit 0
    fi
    click 515 24
    new_jail() { replacement=$(docs | grep -vx "$closing" | grep -vx "$sibling"); test -n "$replacement"; }
    wait_for new_jail; wait_for count 2
    test "$replacement" != "$closing"
    test "$(root "$replacement")" = "$old_root"
    wait_for opened "$replacement" Untitled
    wait_for grep -Fq '"name":"Untitled"' "$target_marker"
    sed 's/,"name":.*//' "$results/closed.marker" > "$results/closed.id"
    sed 's/,"name":.*//' "$target_marker" > "$results/replacement.id"
    if cmp -s "$results/closed.id" "$results/replacement.id"; then
        echo "Replacement workbook inherited the previous recovery identity" >&2
        exit 1
    fi
    wait_for pending 2
    cmp "$results/sibling.marker" "$sibling_marker"
    sleep .6; edit 129 0x1f # blank A1, B1=2
    publish Untitled.osheet ',2'
    key 0x1a 1; wait_for count 1; wait_for pending 1
    test "$(docs)" = "$sibling"
    cmp "$results/sibling.marker" "$sibling_marker"
    edit 129 0x1f
    publish 'Proof (2).osheet' 'zebra,2'
    key 0x1a 1
    sibling_closed() { ! doc_process "$sibling"; }
    wait_for sibling_closed
    click 213 24; wait_for pending 0; wait_for count 0
    test -z "$(mdconfig -lv | grep sheets-recovery)"
    /tmp/check-recovered-copy "$books/Proof.osheet" fox plain
    test "$(sha256 -q "$books/Proof.csv")" = "$original"
    for copy in "$books"/*.osheet; do mv "$copy" "$results/"; done
    echo "Clean close reused the slot with a fresh identity and preserved the live sibling. Copies: $results"
    exit 0
fi
if test "${1:-}" = --interrupted-labels || test "${1:-}" = --live-broker-restart || test "${1:-}" = --save-as-labels || test "${1:-}" = --prepare-reboot-labels || test "${1:-}" = --retry-recovery-label; then
    interrupted=false
    if test "$1" = --retry-recovery-label; then
        fault=${3:-missing}
        case "$fault" in
            missing) ;;
            invalid-utf8) printf '\377' > "$results/unreadable-name" ;;
            *) echo "Expected missing or invalid-utf8" >&2; exit 1 ;;
        esac
    fi
    if test "$1" != --save-as-labels; then
        interrupted=true
        fixture_broker "${2:-}"
    fi
    wait_for count 0
    wait_for pending 0
    test "$(ls -A "$books")" = Proof.csv
    original=$(sha256 -q "$books/Proof.csv")
    open_proof 1; first=$(docs)
    wait_for opened "$first" Proof.csv
    sleep .6
    edit 62 0x1d 0x08 0x05 0x15 0x04 # zebra
    open_proof 2; second=$(docs | grep -vx "$first")
    wait_for opened "$second" Proof.csv
    sleep .6
    edit 62 0x09 0x12 0x1b # fox
    for slot in 0 1; do
        marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name "$slot.pending")
        grep -Fq '"name":"Proof.csv"' "$marker"
        cp "$marker" "$results/$slot.before"
    done
    for slot in 1 0; do
        if test "$slot" = 1; then
            jid=$second; name=Proof.osheet; a1=fox
        else
            jid=$first; name='Proof (2).osheet'; a1=zebra
        fi
        dir=$(dirname "$(find "$(root "$jid")/atrium/recovery" -name current)")
        marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name "$slot.pending")
        # Dismissing Save As must preserve both old names and the identity.
        parent=$(win_at 205 24); click 205 24
        wait_for different_window 824 457 "$parent"
        key 0x29; sleep .5
        check_name "$dir" "$marker" Proof.csv
        cmp "$marker" "$results/$slot.before"
        if "$interrupted" && test "$slot" = 0; then
            # Leave Scrinium and Limen available, and interrupt only the
            # document's connection to Portcullis during publication.
            test ! -e "$workers.held"
            mv "$workers" "$workers.held"
            trap 'mv "$workers.held" "$workers"' EXIT
            publish "$name" "$a1"
            wait_for opened "$jid" "$name"
            cmp "$marker" "$results/$slot.before"
            mv "$workers.held" "$workers"
            trap - EXIT
        else
            publish "$name" "$a1"
            wait_for check_name "$dir" "$marker" "$name"
        fi
        sed 's/,"name":.*//' "$marker" > "$results/id"
        sed 's/,"name":.*//' "$results/$slot.before" | cmp -s - "$results/id"
        if test "$slot" = 0 && { test "$1" = --live-broker-restart || test "$1" = --prepare-reboot-labels; }; then
            jail_root=$(root "$jid")
            restart_broker
            test "$(root "$jid")" = "$jail_root"
            mount -p | awk -v path="$jail_root/atrium/recovery" '$2==path && $3=="nullfs" {found=1} END {exit !found}'
            wait_for opened "$jid" "$name"
        fi
        edit 129 0x1f
        sleep 2.2
        if test "$1" = --retry-recovery-label && test "$slot" = 0; then
            test "$(cat "$dir/name")" = "$name"
            mv "$dir/name" "$dir/name.saved"
            if test "$fault" = invalid-utf8; then cp "$results/unreadable-name" "$dir/name"; fi
        fi
        kill_doc "$jid"; wait_for count "$slot"
    done
    wait_for pending 2
    if test "$1" = --prepare-reboot-labels; then
        cmp "$marker" "$results/0.before"
        mdconfig -lv | grep -F sheets-recovery >/dev/null
        sysctl -n kern.boottime > "$results/boot"
        echo "$original" > "$results/source.sha256"
        sync
        echo "Reboot and run --verify-reboot-labels $results"
        exit 0
    fi
    if "$interrupted"; then
        detached() { test -z "$(mdconfig -lv | grep sheets-recovery)"; }
        if test "$1" = --live-broker-restart; then
            mount -p > "$results/after-crash.mounts"
            mdconfig -lv > "$results/after-crash.devices"
        else
            wait_for detached
            restart_broker
        fi
    fi
    click 213 24
    if test "$1" = --retry-recovery-label; then
        sibling="$(dirname "$marker")/1.pending"
        wait_for grep -Fq '"checked_detached":true' "$sibling"
        wait_for detached
        cmp "$results/0.before" "$marker"
        cp "$sibling" "$results/healthy.marker"
        sleep .6; cp selectscreen.png "$results/fallback-chooser.png"
        click 120 96; wait_for count 1
        fallback=$(docs)
        wait_for test -f "$(root "$fallback")/atrium/recovery/$(basename "$dir")/name.saved"
        sleep .6; edit 129 0x20 # commit B1=3 via the fallback row
        sleep 2.2; kill_doc "$fallback"; wait_for count 0; wait_for detached
        warnings() { awk '/keeping previous workbook recovery label/ {n++} END {print n+0}' select-port.log; }
        before=$(warnings)
        click 213 24
        retried() { test "$(warnings)" -gt "$before"; }
        wait_for retried; wait_for detached
        cmp "$results/0.before" "$marker"
        repair_fixture_name
        cmp "$results/0.before" "$marker"
        click 213 24
    fi
    if "$interrupted"; then
        marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name '0.pending')
        wait_for grep -Fq '"name":"Proof (2).osheet"' "$marker" || {
            cp selectscreen.png "$results/recovery-names.png"
            echo "Stale chooser marker after broker restart. Frame: $results/recovery-names.png" >&2
            exit 1
        }
    fi
    if test "$1" = --live-broker-restart; then
        wait_for detached
        sed 's/,"name":.*//' "$marker" | cmp -s - "$results/id"
        mount -p | awk -v root="$jail_root" '$2 == root || $2 == root "-upper" || index($2, root "/")==1 {bad=1} END {exit bad}'
    fi
    # The chooser redraw follows the broker reply, after the volume detaches.
    sleep .6
    cp selectscreen.png "$results/recovery-names.png"
    if test "$1" = --retry-recovery-label; then
        grep -Fq '"checked_detached":true' "$marker"
        cmp "$results/healthy.marker" "$sibling"
        for slot in 0 1; do
            sed 's/,"name":.*//' "$(dirname "$marker")/$slot.pending" > "$results/after.id"
            sed 's/,"name":.*//' "$results/$slot.before" | cmp -s - "$results/after.id"
        done
        recover_named_pair 'zebra,3'
        wait_for detached
    else
        recover_named_pair
    fi
    exit 0
fi
if test "${1:-}" = --delayed-runtime-start || test "${1:-}" = --delayed-start-sibling; then
    first=delayed-start
    interleave=false
    if test "${4:-}" = interleave; then
        test "$1" = --delayed-start-sibling && test "$3" = stale-refresh
        test -x /tmp/tile-recovery-windows
        interleave=true
    fi
    fixture_broker "${2:?pass the private fixture broker executable}"
    test "$(ls -A "$books")" = Proof.csv
    make_pair
    if test "$1" = --delayed-runtime-start; then
        choose protected; save_copy protected; wait_for pending 1
    fi
    marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name 0.pending)
    sed 's/,"name":.*//' "$marker" > "$results/original.id"

    # Only the private broker gets this PATH. Hold its runtime jail -c for the
    # selected UID after the recovery mount and setup handshake have completed.
    bin="$results/bin"; mkdir "$bin"
    target="org_atrium_sheets__document_${plain_root##*-}"
    printf '%s\n' "$target" > "$bin/target"
    cat > "$bin/jail" <<'SH'
#!/bin/sh
set -eu
dir=${0%/*}
if test "$#" = 5 && test "$1" = -q && test "$2" = -c; then
    echo "$5" >> "$dir/starts"
fi
if test "$#" = 5 && test "$1" = -q && test "$2" = -c && test "$5" = "$(cat "$dir/target")"; then
    echo "$$" >> "$dir/attempts"
    for i in $(jot 450); do
        test ! -e "$dir/hold" && break
        sleep .1
    done
    test ! -e "$dir/hold" || exit 1
fi
exec /usr/sbin/jail "$@"
SH
    chmod 755 "$bin/jail"
    touch "$bin/hold"
    restore_sibling() {
        if test -f "$results/offline.image"; then
            test ! -e "$sibling_image" && mv "$results/offline.image" "$sibling_image"
        fi
    }
    restore_broker() {
        restore_sibling
        if test -e "$bin/hold"; then unlink "$bin/hold"; fi
        fixture_broker "$broker"
        restart_broker
    }
    detached() { test -z "$(mdconfig -lv | grep sheets-recovery)"; }
    wait_for detached
    trap restore_broker EXIT
    PATH="$bin:$PATH" restart_broker
    click 213 24
    if test "$1" = --delayed-start-sibling; then
        wait_for grep -Fq '"checked_detached":true' "$(dirname "$marker")/1.pending"
        sleep .6; click 120 96
    fi
    wait_for test -s "$bin/attempts"
    input "MOVE 120 96"
    # Compare the named notice and all selectable rows in the headless frame.
    frame=9eab724e5b8afc69816b69e78b42f336ff7998adda5cb52f01c365def052fb38
    if test "$1" = --delayed-start-sibling; then
        frame=0e41a5a8ee7dfee56af903f203271fc4884ba1981924cac5f1a800d895e2f900
        pending 2
    else
        pending 1
    fi
    starting() { test "$(sha256 -q selectscreen.png)" = "$frame"; }
    wait_for starting
    cp selectscreen.png "$results/starting.png"
    count 0
    case "${3:-refresh}" in
        refresh)
            offset=$(wc -l < select-port.log)
            click 213 24
            repeat_received() { tail -n "+$((offset+1))" select-port.log | grep -Fq 'is app org.atrium.sheets'; }
            wait_for repeat_received
            input "MOVE 120 96"; sleep 2.5
            cp selectscreen.png "$results/repeated.png"
            cmp "$results/starting.png" "$results/repeated.png"
            ;;
        direct|unavailable|stale-id|stale-refresh) test "$1" = --delayed-start-sibling ;;
        *) exit 2 ;;
    esac
    count 0; test "$(wc -l < "$bin/attempts")" -eq 1
    if test "${3:-}" = stale-id || test "${3:-}" = stale-refresh; then
        sibling_marker="$(dirname "$marker")/1.pending"
        cp "$sibling_marker" "$results/listed.marker"
        # Change only the opaque ID; the old row still has the same name and slot.
        replacement='"id":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,41]'
        sed "s/\"id\":\[[^]]*\]/$replacement/" "$results/listed.marker" > "$results/replacement.marker"
        grep -Fq "$replacement" "$results/replacement.marker"
        if grep -Fq "$replacement" "$marker" "$results/listed.marker"; then exit 1; fi
        cp -p "$sibling_marker" "$sibling_marker.tmp"
        cat "$results/replacement.marker" > "$sibling_marker.tmp"
        mv "$sibling_marker.tmp" "$sibling_marker"
        click 120 96
        stale_notice() { test "$(sha256 -q selectscreen.png)" = 45ce0e5ef6dc104f5e1845eeebdf96c7b7003d7c7b6a2f80b5733038d03c7a62; }
        wait_for stale_notice
        cp selectscreen.png "$results/stale-selection.png"
        cmp "$sibling_marker" "$results/replacement.marker"
        test -f "$bin/hold"; count 0; pending 2
        cmp "$bin/starts" "$bin/target"
        if test "$3" = stale-refresh; then
            click 213 24; input "MOVE 120 96"
            # The pointer can reach the row before or after the chooser redraw.
            refreshed() {
                case "$(sha256 -q selectscreen.png)" in
                    "$frame"|a5d0fe78e1729016f0d59f33c8b84dc325de128ed5934f8dcb9d37c7c3c27420) return 0 ;;
                    *) return 1 ;;
                esac
            }
            wait_for refreshed
            cp selectscreen.png "$results/refreshed.png"
            count 0; pending 2; cmp "$bin/starts" "$bin/target"
        fi
    elif test "${3:-}" = unavailable; then
        store=$(dirname "$marker")
        sibling_image="$store/1.img"
        if test -f "$store/1.ufs"; then sibling_image="$store/1.ufs"; fi
        mdconfig -lv > "$results/held.devices"
        grep -F "$store/0." "$results/held.devices" >/dev/null
        if grep -F "$sibling_image" "$results/held.devices"; then exit 1; fi
        digest=$(sha256 -q "$sibling_image")
        cp "$store/1.pending" "$results/sibling.marker"
        mv "$sibling_image" "$results/offline.image"
        offset=$(wc -l < select-port.log)
        click 120 96
        refused() { tail -n "+$((offset+1))" select-port.log | grep -Fq 'checking detached workbook slot 1:'; }
        wait_for refused
        # The headless launcher reads “The recovery image for ‘Untitled’ is unavailable.”
        sibling_error() { test "$(sha256 -q selectscreen.png)" = 4b53bd18aab2d35e4d5ff6918b5b25b1847ff63bd11224e0d2bd734c0fd19f71; }
        wait_for sibling_error
        cp selectscreen.png "$results/unavailable.png"
        test -f "$bin/hold"; count 0; pending 2
        cmp "$store/1.pending" "$results/sibling.marker"
    fi
    case "$1/${3:-refresh}" in
    --delayed-start-sibling/direct|--delayed-start-sibling/refresh|--delayed-start-sibling/stale-refresh)
        click 120 96; wait_for count 1
        sibling=$(docs)
        test "$(root "$sibling")" != "$plain_root"
        wait_for opened "$sibling" Untitled
        if test "${3:-}" = stale-refresh; then
            wait_for pgrep -f -j "$sibling" -- 'atrium-sheets --recover-document 41$'
        fi
        sleep .6
        if "$interleave"; then
            type_keys 0x17 0x08 0x16 0x17 0x28
            sleep .6; publish Untitled.osheet fox protected
            sibling_win=$parent
            sibling_root=$(root "$sibling")
            sibling_hash=$(sha256 -q "$books/Untitled.osheet")
            count 1; pending 2
        else
            save_copy protected; wait_for pending 1; count 0
        fi
        test -f "$bin/hold"
        test "$(wc -l < "$bin/starts")" -eq 2
        ;;
    esac
    unlink "$bin/hold"
    if "$interleave"; then
        wait_for count 2
        jid=$(docs | grep -vx "$sibling")
    else
        wait_for count 1; jid=$(docs)
    fi
    wait_for pgrep -x -j "$jid" atrium-sheets
    test "$(root "$jid")" = "$plain_root"
    sed 's/,"name":.*//' "$marker" | cmp -s - "$results/original.id"
    if "$interleave"; then
        wait_for different_window 205 24 "$sibling_win"
        sleep .6; publish Proof.osheet zebra
        plain_win=$parent
        test "$(sha256 -q "$books/Untitled.osheet")" = "$sibling_hash"
        tiled() { test "$(win_at 125 24)" = "$plain_win" && test "$(win_at 765 24)" = "$sibling_win"; }
        case "${5:-}" in
        restart-limen|pending-save|published-save|external-edit)
            plain_pid=$(pgrep -x -j "$jid" atrium-sheets)
            sibling_pid=$(pgrep -x -j "$sibling" atrium-sheets)
            files_pid=$(pgrep -x -U sheetsfiles bench-scrinium)
            limen_socket=/atrium/sockets/limen/limend.sock
            listener() { sockstat -ulw | awk -v path="$limen_socket" '$6==path {print $3}'; }
            # Check the server end of each connection, pinned to the original
            # document and file-provider processes on both sides of the restart.
            connected() {
                sockstat -ucw | awk -v server="$1" -v peer="$2" \
                    '$3==server && index($0, "-> [" peer " ") {found=1} END {exit !found}'
            }
            old_limen=$(listener)
            test "$(procstat -b "$old_limen" | awk 'NR==2 {print $NF}')" = "$PWD/limend"
            for peer in "$plain_pid" "$sibling_pid" "$files_pid"; do connected "$old_limen" "$peer"; done
            stopped_limen() { ps -p "$old_limen" -o state= | grep -q '^T'; }
            if test "$5" = published-save || test "$5" = external-edit; then
                if test "$5" = external-edit; then cp "$books/Proof.osheet" "$results/external.osheet"; fi
                /tmp/tile-recovery-windows "$plain_win" "$sibling_win" "$plain_win"
                wait_for tiled
                edit 129 0x1f # First snapshot contains B1=2.
                (
                    # Stop the provider at the atomic rename. Freeze the broker
                    # before releasing the provider so confirmation cannot pass.
                    release="$results/release-rename"
                    trap 'touch "$release"; kill -CONT "$old_limen" 2>/dev/null || :' EXIT
                    timeout 30 lldb -b -p "$files_pid" \
                        -o 'breakpoint set -n renameat' \
                        -o "platform shell touch $results/debugger-ready" -o continue \
                        -o "platform shell touch $results/at-rename" \
                        -o "platform shell timeout 15 sh -c 'until test -f $release; do sleep .1; done'" \
                        -o 'process detach' </dev/null > "$results/rename.log" 2>&1 &
                    debugger=$!
                    wait_for test -f "$results/debugger-ready"
                    click 125 24
                    wait_for test -f "$results/at-rename"
                    grep -q 'stop reason = breakpoint 1' "$results/rename.log"
                    kill -STOP "$old_limen"
                    wait_for stopped_limen
                    /tmp/check-recovered-copy "$books/Proof.osheet" zebra plain
                    touch "$release"; wait "$debugger"
                    published() { /tmp/check-recovered-copy "$books/Proof.osheet" zebra,2 plain; }
                    wait_for published
                    test "$(sha256 -q "$books/Untitled.osheet")" = "$sibling_hash"
                    edit 129 0x21 # B1=4 stays in Sheets until the retry.
                    kill -KILL "$old_limen"
                    trap - EXIT
                )
                plain_hash=$(sha256 -q "$books/Proof.osheet")
            elif test "$5" = pending-save; then
                /tmp/tile-recovery-windows "$plain_win" "$sibling_win" "$plain_win"
                wait_for tiled
                edit 129 0x21 # Commit B1=4 once. The retry presses only Save.
                plain_hash=$(sha256 -q "$books/Proof.osheet")
                # The server-side receive queue proves this document sent its
                # request before the broker died, while Scrinium could not see it.
                pcb=$(sockstat -Aucw | awk -v server="$old_limen" -v peer="$plain_pid" \
                    '$3==server && index($0, "-> [" peer " ") {sub(/^0x/,"",$NF); print $NF; exit}')
                test -n "$pcb"
                recv_bytes() { netstat -an -f unix | awk -v pcb="$pcb" '$1==pcb {print $3}'; }
                (
                    trap 'kill -CONT "$old_limen" 2>/dev/null || :' EXIT
                    kill -STOP "$old_limen"
                    wait_for stopped_limen
                    test "$(recv_bytes)" = 0
                    click 125 24
                    queued() { test "$(recv_bytes)" -gt 0; }
                    wait_for queued
                    kill -KILL "$old_limen"
                    trap - EXIT
                )
            else
                kill -TERM "$old_limen"
            fi
            offline() { ! kill -0 "$old_limen" 2>/dev/null && test -z "$(listener)"; }
            wait_for offline
            env LIMEN_SOCKET="$limen_socket" LIMEN_FRESCOD_ACTIVATION="$PWD/activation.sock" \
                daemon -f -u _limen -o "$PWD/bench-limen.log" "$PWD/limend"
            listening() { new_limen=$(listener); test -n "$new_limen" && test "$new_limen" != "$old_limen"; }
            wait_for listening
            for peer in "$plain_pid" "$sibling_pid" "$files_pid"; do wait_for connected "$new_limen" "$peer"; done
            ;;
        esac
        # Each ordinary Save must update its own Save As destination in place.
        # Keep the other file's digest and independently read both copies.
        save_turn() {
            mode=$1; usage=$2; plain_cells=$3; protected_cells=$4
            if test "$mode" = protected; then
                x=640; focus=$sibling_win; uid=${sibling_root##*-}; target_file=Untitled; other_file=Proof
            else
                x=0; focus=$plain_win; uid=${plain_root##*-}; target_file=Proof; other_file=Untitled
            fi
            /tmp/tile-recovery-windows "$plain_win" "$sibling_win" "$focus"
            count 2; pending 2; wait_for tiled
            before=$(sha256 -q "$books/$target_file.osheet")
            other=$(sha256 -q "$books/$other_file.osheet")
            trace_offset=$(wc -l < select-files.log)
            if test "$usage" != retry; then edit "$((x + 129))" "$usage"; fi
            click "$((x + 125))" 24
            saved() { test "$(sha256 -q "$books/$target_file.osheet")" != "$before"; }
            wait_for saved
            tail -n "+$((trace_offset+1))" select-files.log | grep -Fq "for org.atrium.sheets (uid $uid)"
            test "$(sha256 -q "$books/$other_file.osheet")" = "$other"
            /tmp/check-recovered-copy "$books/Proof.osheet" "$plain_cells" plain
            /tmp/check-recovered-copy "$books/Untitled.osheet" "$protected_cells" protected
        }
        if test "${5:-}" = external-edit; then
            # An outside writer clears B1 in place by restoring the valid
            # revision saved before B1=2. The live plain window still has B1=4.
            test "$(sha256 -q "$books/Proof.osheet")" = "$plain_hash"
            test "$(sha256 -q "$results/external.osheet")" != "$plain_hash"
            cat "$results/external.osheet" > "$books/Proof.osheet"
            /tmp/check-recovered-copy "$books/Proof.osheet" zebra plain
            /tmp/tile-recovery-windows "$plain_win" "$sibling_win" "$plain_win"
            wait_for tiled
            click 125 24
            input 'MOVE 120 96'
            # Both windows tiled, B1=4 still visible and the status reads
            # “The workbook changed since it was opened. Reopen it or use Save As.”
            conflict_visible() { test "$(sha256 -q selectscreen.png)" = 3946787817a18a200dbf2f8f35f1ca710093ef012c658a92252f29a1dac4c428; }
            wait_for conflict_visible
            cp selectscreen.png "$results/conflict.png"
            cmp "$books/Proof.osheet" "$results/external.osheet"
            test "$(sha256 -q "$books/Untitled.osheet")" = "$sibling_hash"
            save_turn protected 0x20 zebra fox,3
            sibling_saved=$(sha256 -q "$books/Untitled.osheet")
            /tmp/tile-recovery-windows "$plain_win" "$sibling_win" "$plain_win"
            wait_for tiled
            copy='Proof (2).osheet'
            test ! -e "$books/$copy"
            click 205 24
            wait_for different_window 611 457 "$plain_win"
            click 611 457
            wait_for test -s "$books/$copy"
            /tmp/check-recovered-copy "$books/$copy" zebra,4 plain
            wait_for opened "$jid" "$copy"
            picker_dismissed() { test "$(win_at 611 457)" = "$plain_win"; }
            wait_for picker_dismissed
            cmp "$books/Proof.osheet" "$results/external.osheet"
            test "$(sha256 -q "$books/Untitled.osheet")" = "$sibling_saved"
            /tmp/tile-recovery-windows "$plain_win" "$sibling_win" "$plain_win"
            wait_for tiled
            copy_hash=$(sha256 -q "$books/$copy")
            trace_offset=$(wc -l < select-files.log)
            edit 191 0x24 # C1=7, retaining B1=4 from Save As.
            click 125 24
            copy_saved() { test "$(sha256 -q "$books/$copy")" != "$copy_hash"; }
            wait_for copy_saved
            tail -n "+$((trace_offset+1))" select-files.log | grep -Fq "for org.atrium.sheets (uid ${plain_root##*-})"
            /tmp/check-recovered-copy "$books/$copy" zebra,4,7 plain
            cmp "$books/Proof.osheet" "$results/external.osheet"
            test "$(sha256 -q "$books/Untitled.osheet")" = "$sibling_saved"
        else
            plain_input=0x21
            plain_published=zebra
            if test "${5:-}" = published-save; then plain_published=zebra,2; fi
            if test "${5:-}" = pending-save || test "${5:-}" = published-save; then
                test "$(sha256 -q "$books/Proof.osheet")" = "$plain_hash"
                test "$(sha256 -q "$books/Untitled.osheet")" = "$sibling_hash"
                plain_input=retry
            fi
            save_turn protected 0x20 "$plain_published" fox,3
            save_turn plain "$plain_input" zebra,4 fox,3
            save_turn protected 0x22 zebra,4 fox,5
            save_turn plain 0x23 zebra,6 fox,5
        fi
        count 2; key 0x1a 1; wait_for count 1
        test "$(docs)" = "$sibling"
        /tmp/tile-recovery-windows --clear-focus
        key 0x1a 1; wait_for count 0
        /tmp/tile-recovery-windows --clear-focus
        if test "${5:-}" != external-edit; then mv "$books/Proof.osheet" "$books/Untitled.osheet" "$results/"; fi
        test "$(cat "$books/Proof.csv")" = Original
    else
        save_copy plain
    fi
    if test "${3:-}" = unavailable; then
        wait_for pending 1
        test ! -e "$sibling_image"
        cmp "$store/1.pending" "$results/sibling.marker"
        restore_sibling
        test "$(sha256 -q "$sibling_image")" = "$digest"
        click 213 24; wait_for count 1
        wait_for opened "$(docs)" Untitled
        sleep .6; save_copy protected
    fi
    if test "${3:-}" = stale-id; then
        wait_for pending 1
        cmp "$bin/starts" "$bin/target"
        cmp "$sibling_marker" "$results/replacement.marker"
        click 213 24; wait_for count 1
        sibling=$(docs)
        wait_for opened "$sibling" Untitled
        wait_for pgrep -f -j "$sibling" -- 'atrium-sheets --recover-document 41$'
        sleep .6; save_copy protected
        test "$(wc -l < "$bin/starts")" -eq 2
    fi
    wait_for pending 0
    test "$(wc -l < "$bin/attempts")" -eq 1
    test -z "$(mdconfig -lv | grep sheets-recovery)"
    restore_broker; trap - EXIT
    if test "${5:-}" = external-edit; then
        # Open from the launcher after the recovered jails and their mounts are
        # gone. Read the autosave snapshot created from each newly loaded model.
        open_row() {
            row=$1
            count 0; pending 0
            launcher=$(win_at 48 24)
            click 48 24; wait_for different_window 488 "$row" "$launcher"
            picker=$win
            click 488 "$row"; sleep .5; key 0x28
            wait_for count 1
            reopened=$(docs)
            test "$reopened" != "$jid" && test "$reopened" != "$sibling"
            document_visible() {
                different_window 48 24 "$launcher" && test "$win" != "$picker"
            }
            wait_for document_visible
        }
        loaded_cells() {
            name=$1; cells=$2; mode=$3
            wait_for opened "$reopened" "$name"
            directory=$(dirname "$(find "$(root "$reopened")/atrium/recovery" -name name -exec grep -lx "$name" {} +)")
            wait_for test -s "$directory/current"
            generation=$(cat "$directory/current")
            /tmp/check-recovered-copy "$directory/base.$generation.osheet" "$cells" "$mode"
        }
        close_reopened() { key 0x1a 1; wait_for count 0; wait_for pending 0; wait_for detached; }
        frame_is() { test "$(sha256 -q selectscreen.png)" = "$1"; }
        reject_password() {
            open_row 273
            input 'MOVE 48 24'
            wait_for frame_is a7ba8f30ff86bc518db45915b88dd91ab0e7e3ce2629d23775d3c633dc8664d7
            type_keys 0x05 0x04 0x07 0x28 # bad
            # At 1100 px the filename and the full “Try again” guidance
            # fit beside the password field.
            wait_for frame_is 7e8b4da6aafa57384385354e48afe7aa772e6436072ff9e83b30cf5730f5c89c
            test "$(docs)" = "$reopened"
            if opened "$reopened" Untitled.osheet; then echo "Wrong password opened the workbook" >&2; return 1; fi
        }
        files_intact() {
            cmp "$books/Proof.osheet" "$results/external.osheet" &&
                test "$(sha256 -q "$books/$copy")" = "$copy_saved_hash" &&
                test "$(sha256 -q "$books/Untitled.osheet")" = "$sibling_saved"
        }
        copy_saved_hash=$(sha256 -q "$books/$copy")
        reject_password
        type_keys 0x17 0x08 0x16 0x17 0x28 # test
        loaded_cells Untitled.osheet fox,3 protected
        close_reopened; files_intact

        reject_password
        click 1067 70; input 'MOVE 48 24'
        # Cancel leaves the empty window showing “Untitled.osheet” stayed closed.
        wait_for frame_is dc795b9a753bdc6692a51a0f5be989dfecdddb087ebf83a1fb4a7ded9255986f
        loaded_cells Untitled '' plain
        close_reopened; files_intact

        open_row 243; loaded_cells Proof.osheet zebra plain; close_reopened
        open_row 183; loaded_cells "$copy" zebra,4,7 plain

        # Open a long-named protected copy from the plain document, then put
        # both windows into 640 px tiles on the 1280 px fixture desktop.
        long_name='Year End Consolidated Financial Statements and Regional Revenue Reconciliation - Final Audit Committee Review.osheet'
        test ! -e "$books/$long_name"
        install -o sheetsfiles -m 600 "$books/Untitled.osheet" "$books/$long_name"
        left_jail=$reopened; left_window=$(win_at 48 24)
        click 48 24; wait_for different_window 488 303 "$left_window"
        picker_window=$win
        click 488 303; sleep .5; key 0x28
        wait_for count 2; reopened=$(docs | grep -vx "$left_jail")
        protected_visible() { different_window 48 24 "$left_window" && test "$win" != "$picker_window"; }
        wait_for protected_visible
        right_window=$win
        /tmp/tile-recovery-windows "$left_window" "$right_window" "$right_window"
        input 'MOVE 688 24'
        wait_for frame_is f8a508c84809a8200efd35035e5559b40b10bb1eb0c1823ac36e625c39e7a725
        cp selectscreen.png "$results/tiled-password.png"
        type_keys 0x05 0x04 0x07 0x28 # bad
        wait_for frame_is 2fe24233ccb855c86ec86a34af959fffe97822bc8baccb43054f5738ce501407
        cp selectscreen.png "$results/tiled-retry.png"
        type_keys 0x17 0x08 0x16 0x17 0x28 # test
        loaded_cells "$long_name" fox,3 protected
        # Leave two journaled workbooks so Recover offers an actual choice.
        edit 831 0x25 # protected C1=8
        protected_jail=$reopened
        /tmp/tile-recovery-windows "$left_window" "$right_window" "$left_window"
        edit 255 0x26 # plain D1=9
        sleep 2.2
        test -s "$directory/journal.$generation"
        journal_rel="${directory##*/}/journal.$generation"
        journal_hash=$(sha256 -q "$directory/journal.$generation")
        cmp "$books/Untitled.osheet" "$books/$long_name"
        kill_doc "$protected_jail"; kill_doc "$left_jail"
        wait_for count 0; wait_for pending 2
        /tmp/tile-recovery-windows --clear-focus
        click 213 24; input 'MOVE 8 240'
        wait_for frame_is 2c2c469f30037c925bd2cfcced2e60d4991adf2208434f4e7a8a54f067dd2749
        cp selectscreen.png "$results/long-chooser.png"
        launcher=$(win_at 48 24)
        /tmp/tile-recovery-windows --window-width "$launcher" 640
        input 'MOVE 8 240'
        wait_for frame_is fcc1c405c3f135937337aa6950a42493c2f0d63151419391e50ff9d4337b37e7
        cp selectscreen.png "$results/tiled-long-chooser.png"
        open_protected_recovery() {
            click 120 155; wait_for count 1
            /tmp/tile-recovery-windows --clear-focus
            reopened=$(docs)
            wait_for frame_is a9adaf60ea16d330ad49e809c57faf7e8fffc0aaf38e2cc46375e1a97270793f
        }
        store=$(dirname "$marker")
        # Opening the slot invalidates its detached-volume check. Compare the
        # workbook names and identities without that cached flag.
        pending_identity() { sed 's/,"checked_detached":[a-z]*//' "$store/$1.pending"; }
        for slot in 0 1; do
            pending_identity "$slot" > "$results/before-cancel-$slot.pending"
        done
        recovery_unchanged() {
            for slot in 0 1; do
                pending_identity "$slot" > "$results/$1-$slot.pending"
                cmp "$results/before-cancel-$slot.pending" "$results/$1-$slot.pending" || return 1
            done
        }
        journal_unchanged() {
            test "$(sha256 -q "$(root "$reopened")/atrium/recovery/$journal_rel")" = "$journal_hash"
        }
        wrong_recovery_password() {
            type_keys 0x05 0x04 0x07 0x28 # bad
            # The workbook stays locked and the full name and retry guidance are visible.
            wait_for frame_is "${2:-300f65803a34296db04f3f55300f244489aa4479d1f8360314dc9cc94f949143}"
            test "$(docs)" = "$reopened"
            pending "${1:-2}"
            journal_unchanged
        }
        open_protected_recovery
        test "$reopened" != "$protected_jail"
        for prompt in initial wrong-password; do
            if test "$prompt" = wrong-password; then wrong_recovery_password; fi
            canceled_jail=$reopened
            cp selectscreen.png "$results/before-cancel-$prompt.png"
            click 1058 94
            wait_for count 0; wait_for detached; wait_for pending 2
            recovery_unchanged "after-cancel-$prompt"
            cmp "$books/Untitled.osheet" "$books/$long_name"
            files_intact

            /tmp/tile-recovery-windows --clear-focus
            click 213 24; input 'MOVE 8 240'
            wait_for frame_is fcc1c405c3f135937337aa6950a42493c2f0d63151419391e50ff9d4337b37e7
            cp selectscreen.png "$results/retry-chooser-$prompt.png"
            open_protected_recovery
            test "$reopened" != "$canceled_jail"
            recovery_unchanged "after-reopen-$prompt"
            journal_unchanged
        done
        locked_jail=$reopened
        locked_window=$(win_at 48 24)
        test "$locked_window" -gt 0 && test "$locked_window" != "$launcher"
        locked_pid=$(pgrep -x -j "$locked_jail" atrium-sheets)
        # Put the launcher beside the protected recovery so both can receive input.
        /tmp/tile-recovery-windows "$launcher" "$locked_window" "$launcher"
        launcher_visible() { test "$(win_at 213 24)" = "$launcher"; }
        wait_for launcher_visible
        click 213 24; wait_for count 2
        plain_recovery=$(docs | grep -vx "$locked_jail")
        wait_for opened "$plain_recovery" 'Proof (2).osheet'
        plain_recovery_visible() {
            different_window 205 24 "$launcher" && test "$win" != "$locked_window"
        }
        wait_for plain_recovery_visible
        plain_window=$win
        plain_restored='Proof (2) (2).osheet'
        publish "$plain_restored" zebra,4,7,9
        plain_hash=$(sha256 -q "$books/$plain_restored")
        count 2
        test "$(pgrep -x -j "$locked_jail" atrium-sheets)" = "$locked_pid"
        journal_unchanged
        cmp "$books/Untitled.osheet" "$books/$long_name"
        files_intact
        /tmp/tile-recovery-windows --window-width "$plain_window" 1100
        key 0x1a 1; wait_for count 1; wait_for pending 1

        /tmp/tile-recovery-windows --window-width "$locked_window" 1100
        wait_for frame_is f8f99323b7bc731b57445368c1f84102207abd7fe7db07e5acd372eae37daef5
        cp selectscreen.png "$results/protected-after-plain-save.png"
        test "$(docs)" = "$locked_jail"
        wrong_recovery_password 1 fb70f11373a913e201d724b082cc62e84195646dfc24161fc8625455f548cc9b
        cp selectscreen.png "$results/protected-recovery-retry.png"
        type_keys 0x17 0x08 0x16 0x17 0x28
        sleep .6
        restored="${long_name%.osheet} (2).osheet"
        publish "$restored" fox,3,8 protected
        /tmp/tile-recovery-windows --window-width "$locked_window" 1100
        close_reopened
        /tmp/tile-recovery-windows --clear-focus
        wait_for launcher_visible
        /tmp/tile-recovery-windows --window-width "$launcher" 1100
        /tmp/tile-recovery-windows --clear-focus
        wait_for launcher_visible
        test "$(sha256 -q "$books/$plain_restored")" = "$plain_hash"
        mv "$books/$restored" "$books/$plain_restored" "$results/"
        cmp "$books/Untitled.osheet" "$books/$long_name"
        mv "$books/$long_name" "$results/"
        cmp "$books/Proof.osheet" "$results/external.osheet"
        test "$(sha256 -q "$books/Untitled.osheet")" = "$sibling_saved"
        mv "$books/$copy" "$books/Proof.osheet" "$books/Untitled.osheet" "$results/"
    fi
    echo "Recovered one delayed workbook. Frame and copies: $results"
    exit 0
fi
if test "${1:-}" = --retry-jail-setup || test "${1:-}" = --retry-runtime-start; then
    first=setup-retry
    broker_pid=$(sockstat -ulw | awk '$6=="/atrium/sockets/portcullis/portcullis.sock" {print $3}')
    test -n "$broker_pid"
    test "$(ls -A "$books")" = Proof.csv
    make_pair
    marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name 0.pending)
    store=$(dirname "$marker")
    click 213 24
    for slot in 0 1; do wait_for grep -Fq '"checked_detached":true' "$store/$slot.pending"; done
    detached() { test -z "$(mdconfig -lv | grep sheets-recovery)"; }
    wait_for detached
    cp "$marker" "$results/selected.marker"
    cp "$store/1.pending" "$results/sibling.marker"
    upper="$plain_root-upper"
    if test "$1" = --retry-runtime-start; then
        entry=/var/lib/atrium/apps/org.atrium.sheets/bin/atrium-sheets
        test -x "$entry"; mode=$(stat -f %Lp "$entry")
        block() { chmod a-x "$entry"; }
        unblock() { chmod "$mode" "$entry"; }
        diagnostic='exec /bin/atrium-sheets: Permission denied'
        frame=93af748613319b84914805330182932743335f53ed60aca971da1b815c5633db
        # Entering jail setup invalidates the detached-volume check.
        selected_intact() {
            sed 's/"checked_detached":true/"checked_detached":false/' "$results/selected.marker" | cmp -s - "$marker"
        }
    else
        # Only remove the exact fixture inode used to block the upper mount.
        touch "$results/blocked-upper"
        block() { test ! -e "$upper" && ln "$results/blocked-upper" "$upper"; }
        unblock() { if test "$upper" -ef "$results/blocked-upper"; then unlink "$upper"; fi; }
        diagnostic='Sheets workbook launch: Failed("document", "File exists'
        frame=b484989308608aba88ef7e66cc5c2d8ee83dabdd93f4f578cabf77416b0e2249
        selected_intact() { cmp "$marker" "$results/selected.marker"; }
    fi
    trap unblock EXIT
    block
    start=$(wc -l < select-port.log)
    click 120 96
    failed() { tail -n "+$((start+1))" select-port.log | grep -Fq "$diagnostic"; }
    wait_for failed
    named_error() { test "$(sha256 -q selectscreen.png)" = "$frame"; }
    wait_for named_error
    cp selectscreen.png "$results/setup-failed.png"
    wait_for detached; count 0; pending 2
    mount -p | awk -v root="$plain_root" -v upper="$upper" '$2==root || index($2,root "/")==1 || $2==upper {found=1} END {exit found}'
    selected_intact
    cmp "$store/1.pending" "$results/sibling.marker"
    if test "$1" = --retry-runtime-start; then unblock; fi
    choose protected; save_copy protected; wait_for pending 1
    # Listing the sibling can refresh the detached marker.
    if test "$1" = --retry-runtime-start; then block; else selected_intact; fi
    start=$(wc -l < select-port.log)
    click 213 24; wait_for failed
    input "MOVE 120 96" # Clear the Recover button hover before comparing frames.
    wait_for named_error
    wait_for detached; count 0; pending 1
    selected_intact
    unblock; trap - EXIT
    click 213 24; wait_for count 1; wait_for opened "$(docs)" Proof.csv
    test "$(root "$(docs)")" = "$plain_root"
    kill -0 "$broker_pid"
    mdconfig -lv | grep -F "$store/0." >/dev/null
    sed 's/,"name":.*//' "$marker" > "$results/retried.id"
    sed 's/,"name":.*//' "$results/selected.marker" | cmp -s - "$results/retried.id"
    save_copy plain; wait_for pending 0; wait_for detached
    echo "Retried the same workbook after $1. Copies: $results"
    exit 0
fi
if test "${1:-}" = --retry-missing-image || test "${1:-}" = --retry-unmountable-image; then
    first=missing-image
    timing=${2:-after-list}
    case "$timing" in before-list|after-list) ;; *) exit 2 ;; esac
    original=$(sha256 -q "$books/Proof.csv")
    test "$(ls -A "$books")" = Proof.csv
    make_pair
    marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name 0.pending)
    store=$(dirname "$marker")
    detached_images() { test -z "$(mdconfig -lv | grep sheets-recovery)"; }
    wait_for detached_images
    if test "$timing" = after-list; then
        click 213 24
        for slot in 0 1; do wait_for grep -Fq '"checked_detached":true' "$store/$slot.pending"; done
        wait_for detached_images
    else
        grep -Fq '"checked_detached":false' "$marker"
    fi
    cp "$marker" "$results/unavailable.marker"
    image="$store/0.img"
    if test -f "$store/0.ufs"; then image="$store/0.ufs"; fi
    digest=$(sha256 -q "$image")
    restore_image() {
        if test "$image" -ef "$results/unformatted.image"; then unlink "$image"; fi
        if test -f "$results/offline.image"; then
            test ! -e "$image" && mv "$results/offline.image" "$image" || return 1
        fi
        if test -e "$results/unformatted.image"; then unlink "$results/unformatted.image"; fi
    }
    trap restore_image EXIT
    mv "$image" "$results/offline.image"
    if test "$1" = --retry-unmountable-image; then
        # Use a hard link to a separate blank file so teardown can verify its identity.
        truncate -s 8m "$results/unformatted.image"
        ln "$results/unformatted.image" "$image"
    fi
    if test "$timing" = before-list; then
        click 213 24
        wait_for grep -Fq '"checked_detached":true' "$store/1.pending"
        wait_for detached_images
    fi
    cp "$store/1.pending" "$results/sibling.marker"
    errors() { awk '/checking detached workbook slot 0:|could not mount selected workbook slot 0:/ {n++} END {print n+0}' select-port.log; }
    refused() { test "$(errors)" -gt "$before"; }
    before=$(errors)
    sleep .6; click 120 96
    if ! wait_for refused; then cp selectscreen.png "$results/selection-failed.png"; exit 1; fi
    sleep .6; cp selectscreen.png "$results/unavailable.png"
    count 0; pending 2
    cmp "$marker" "$results/unavailable.marker"
    cmp "$store/1.pending" "$results/sibling.marker"
    choose protected; save_copy protected
    wait_for pending 1; cmp "$marker" "$results/unavailable.marker"
    before=$(errors); click 213 24; wait_for refused
    sleep .6; cp selectscreen.png "$results/only-unavailable.png"
    count 0; pending 1
    cmp "$marker" "$results/unavailable.marker"
    wait_for detached_images
    if test "$1" = --retry-unmountable-image; then
        test "$image" -ef "$results/unformatted.image"
    else
        test ! -e "$store/0.img"; test ! -e "$store/0.ufs"
    fi
    restore_image; trap - EXIT
    test "$(sha256 -q "$image")" = "$digest"
    click 213 24; wait_for count 1; wait_for opened "$(docs)" Proof.csv
    sed 's/,"name":.*//' "$marker" > "$results/restored.id"
    sed 's/,"name":.*//' "$results/unavailable.marker" | cmp -s - "$results/restored.id"
    save_copy plain; wait_for pending 0
    test "$(sha256 -q "$books/Proof.csv")" = "$original"
    test -z "$(mdconfig -lv | grep sheets-recovery)"
    echo "Recovered the healthy workbook and retried the restored image. Frames and copies: $results"
    exit 0
fi
for first in protected plain; do
    make_pair
    if test "$first" = plain; then
        # Replace a synthetic identity after the list is drawn to model slot reuse.
        # Older brokers recorded an empty marker. Its first listing supplies
        # an identity without needing to unlock the workbook to read its name.
        legacy=$(find /home/alice/.atrium/apps -maxdepth 2 -name '1.pending')
        : > "$legacy"
        click 213 24; sleep .6
        grep -Fq 'Recovered workbook 2' "$legacy"
        marker=$(find /home/alice/.atrium/apps -maxdepth 2 -name '0.pending')
        cp "$marker" "$results/old-marker"
        sed 's/"id":\[[^]]*\]/"id":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]/' "$marker" > "$marker.tmp"
        ! cmp -s "$marker.tmp" "$marker" || exit 1
        mv "$marker.tmp" "$marker"
        click 120 96; sleep .6
        count 0; pending 2
        cp selectscreen.png "$results/stale-selection.png"
        cmp -s "$marker" "$results/old-marker" && exit 1
    fi
    choose protected
    click 1067 70 # cancel the unlock prompt
    wait_for count 0; wait_for pending 2
    choose "$first"; save_copy "$first"
    wait_for pending 1
    # With only one pending entry Recover opens it directly.
    click 213 24; wait_for count 1; sleep .6
    if test "$first" = plain; then save_copy protected; else save_copy plain; fi
    wait_for pending 0
    echo "$first first: password cancellation preserved both; both copies verified after Save As and Save."
done
