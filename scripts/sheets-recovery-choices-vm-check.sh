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
set -eu
cd /var/tmp/sheets-desktop
books=/var/tmp/sheets-select-input
if test "${1:-}" = --verify-reboot-reuse; then
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
    click 488 183; sleep .5; key 0x28
    wait_for count 1; plain=$(docs)
    wait_for opened "$plain" Proof.csv
    sleep .6
    edit 62 0x1d 0x08 0x05 0x15 0x04 # zebra
    click 515 24; wait_for count 2
    protected=$(docs | grep -vx "$plain")
    wait_for opened "$protected" Untitled
    sleep .6
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
    /tmp/check-recovered-copy "$books/$1" "$2" plain
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
    grep -Fq '"confirmed_nonempty":true' "$sibling_marker"
    sleep .6; edit 129 0x1f
    publish Untitled.osheet ',2'
    key 0x1a 1; wait_for count 0; wait_for pending 1
    click 213 24; wait_for count 1
    restored=$(docs); wait_for opened "$restored" Proof.csv
    grep -Fq '"confirmed_nonempty":false' "$sibling_marker"
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
if test "${1:-}" = --interrupted-labels || test "${1:-}" = --live-broker-restart || test "${1:-}" = --save-as-labels; then
    interrupted=false
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
        if test "$1" = --live-broker-restart && test "$slot" = 0; then
            jail_root=$(root "$jid")
            restart_broker
            test "$(root "$jid")" = "$jail_root"
            mount -p | awk -v path="$jail_root/atrium/recovery" '$2==path && $3=="nullfs" {found=1} END {exit !found}'
            wait_for opened "$jid" "$name"
        fi
        edit 129 0x1f
        sleep 2.2
        kill_doc "$jid"; wait_for count "$slot"
    done
    wait_for pending 2
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
    # Slot zero is the copy with the suffix, although it was saved second.
    click 120 96; wait_for count 1; sleep .6
    publish 'Proof (2) (2).osheet' 'zebra,2'
    key 0x1a 1; wait_for count 0; wait_for pending 1
    click 213 24; wait_for count 1; sleep .6
    publish 'Proof (3).osheet' 'fox,2'
    key 0x1a 1; wait_for count 0; wait_for pending 0
    test "$(sha256 -q "$books/Proof.csv")" = "$original"
    for copy in "$books"/*.osheet; do mv "$copy" "$results/"; done
    echo "Both recovered names, identities, edits and suggested Save As names verified. Frames and copies: $results"
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
