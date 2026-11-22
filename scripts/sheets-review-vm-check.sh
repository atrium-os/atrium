#!/bin/sh
# Run on the private 1280x720 Sheets desktop fixture with its Scrinium place.
# Stage check_recovered_copy at /tmp/check-recovered-copy for the native flow.
# For SHEETS_REVIEW_FORMAT=ods or xlsx, stage one directory from the
# sheets_review_fixture example in the Scrinium place and the FreeBSD
# build of the example at /tmp/sheets-review-fixture.
set -eu
cd /var/tmp/sheets-desktop
books=${1:-/var/tmp/sheets-select-input}
format=${SHEETS_REVIEW_FORMAT:-native}
results=$(mktemp -d /var/tmp/sheets-review-results.XXXXXX)
echo "Sheets review evidence: $results"

input() { printf '%s\n' "$@" | nc -N -U inject.sock >/dev/null; }
click() { input "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; }
key() { input "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; }
wait_for() {
    for i in $(jot 180); do "$@" && return 0; sleep .1; done
    echo "Timed out waiting for $*" >&2
    return 1
}
win_at() {
    input "MOVE $1 $2"
    awk -v x="$1" -v y="$2" '$2=="inject" && $3=="MOVE" && $4==x && $5==y {w=$NF} END{print w}' select-fresco.log
}
other_window() { window=$(win_at "$1" "$2"); test "$window" -gt 0 && test "$window" != "$3"; }
docs() { jls -n jid name | awk '/org_atrium_sheets__document_/ {sub(/^jid=/,"",$1);print $1}'; }
loaded() {
    jail=$(docs)
    test "$(printf '%s\n' "$jail" | wc -w)" -eq 1 || return 1
    directory=$(dirname "$(find "$(jls -j "$jail" path)/atrium/recovery" -name name -exec grep -lx "$1" {} +)")
    test "$directory" != . && test -s "$directory/current"
}
same_window() { test "$(win_at "$1" "$2")" = "$3"; }
has_frame() { test "$(sha256 -q selectscreen.png)" = "$1"; }
expect() {
    input 'MOVE 8 240'
    wait_for has_frame "$2"
    cp selectscreen.png "$results/$1.png"
}
pick() {
    click 520 "$1"
    sleep .25
    click 824 458
}
open_document() {
    launcher=$(win_at 48 24)
    click 48 24
    wait_for other_window 520 "$1" "$launcher"
    pick "$1"
    wait_for loaded "$2"
    document_visible() { other_window 594 24 "$launcher"; }
    wait_for document_visible
    document=$window
}
data_command() {
    click 594 24
    wait_for other_window 650 "$1" "$document"
    click 650 "$1"
}
picker_open() { other_window 520 183 "$document"; }
save_as() {
    click 205 24
    wait_for other_window 824 458 "$document"
    click 824 458
    wait_for test -s "$books/$1"
    wait_for same_window 824 458 "$document"
    /tmp/check-recovered-copy "$books/$1" "$2" plain
}
closed() {
    test -z "$(docs)" || return 1
    if test "$format" = native; then
        test -z "$(mdconfig -lv | grep sheets-recovery)"
    fi
}
close_document() {
    # A click on the grid returns focus after the picker is closed.
    click 518 240
    key 0x1a 1
    wait_for closed
}

capture() {
    input 'MOVE 8 240'
    sleep .45
    cp selectscreen.png "$results/$1.png"
}
checkpoint() { /tmp/sheets-review-fixture check "$1" "$2"; } 2>"$results/checkpoint.log"
save_in_place() {
    click 125 24
    wait_for checkpoint "$books/01-local.$format" "$1"
}
conversion_count() { grep -Fc 'officina: converted ' select-port.log || :; }
converted_after() { test "$(conversion_count)" -gt "$1"; }
picked_copy() {
    before=$(conversion_count)
    pick "$1"
    wait_for converted_after "$before"
    wait_for same_window 594 24 "$document"
}
reopened_state() {
    loaded "01-local.$format" &&
        checkpoint "$directory/base.$(cat "$directory/current").osheet" "$1"
}

foreign_review() {
    test "$(ls -A "$books")" = "$(printf '01-local.%s\n02-review.%s\n03-base.%s\n04-incoming.%s' "$format" "$format" "$format" "$format")"
    (cd "$books" && sha256 0[234]-*) >"$results/sources.sha256"
    checkpoint "$books/01-local.$format" local

    open_document 183 "01-local.$format"
    wait_for reopened_state local
    data_command 18 # Compare with…
    wait_for picker_open
    picked_copy 213
    capture compare-collapsed
    click 890 154
    sleep .25 # The list lays out the newly expanded group before the row click.
    click 890 183
    capture compare-detail
    click 890 154
    capture compare-collapsed-again
    click 1055 72 # Close Compare.

    data_command 102 # Merge Copies…
    wait_for picker_open
    base_picker=$window
    pick 243
    incoming_picker() {
        other_window 520 183 "$document" && test "$window" != "$base_picker"
    }
    wait_for incoming_picker
    picked_copy 273
    capture merge-collapsed

    # Prove the independent changes applied before either conflict is resolved.
    save_in_place independent
    click 890 154
    sleep .25 # The list lays out the newly expanded group before the row click.
    click 890 183
    capture merge-take-detail
    click 802 666 # Take Incoming for B1.
    sleep .25
    click 890 183
    capture merge-keep-detail
    click 908 666 # Keep Mine for C1.
    capture merge-resolved
    save_in_place resolved
    cp "$books/01-local.$format" "$results/saved.$format"
    close_document

    open_document 183 "01-local.$format"
    wait_for reopened_state resolved
    capture merge-reopened
    close_document
    (cd "$books" && sha256 0[234]-*) | cmp - "$results/sources.sha256"
    echo "PASS $format grouped review, independent edits, and resolved conflicts after Save and reopen"
}

case "$format" in
    ods|xlsx) foreign_review; exit ;;
    native) test "$(ls -A "$books")" = Proof.csv ;;
    *) exit 2 ;;
esac

jot 1200 | sed 's/$/,10,stable/' >"$books/01-review-local.csv"
jot 1200 | sed 's/$/,20,stable/' >"$books/02-review-incoming.csv"
printf 'merge,10,20,30,40\n' >"$books/03-merge-local.csv"
printf 'merge,1,2,3,40\n' >"$books/04-merge-base.csv"
printf 'merge,11,22,3,44\n' >"$books/05-merge-incoming.csv"
chown sheetsfiles "$books"/0*.csv
(cd "$books" && sha256 0*.csv) >"$results/sources.sha256"

open_document 183 01-review-local.csv
data_command 46 # Start Tracking Cell Changes
expect tracking-started 5cb68979f3d477b8960353131d3d383633bf59c86086b0156c32daaedb9273de
data_command 18 # Compare with…
wait_for picker_open
expect compare-picker f79d897df1a16a7482ebfde698d6febc247c0df9356522486f3779c4e21a4859
pick 213
expect compare-collapsed 850caa15d17d0c55a9364b65d3a4d46cd06601c7ebf478ec79fb534c4190a7ea
click 890 154
click 890 183
expect compare-detail ff4972a441f99b999689dfb93656d073da3546bd2449e1c3daa10a0102786650
click 802 666 # Take B1
click 890 183
click 908 666 # Keep B2
click 1040 666 # Take all remaining changes.
expect compare-applied 6fe4430ad5b0f3c044ec9caf444789641ad110ed2c76cf47a5c70fa51da3a999
# The native output must contain every taken value and the one kept value.
expected=$(jot 1200 | awk '{printf "%s%d,%d,stable", (NR == 1 ? "" : "\r\n"), $1, ($1 == 2 ? 10 : 20)}')
save_as 01-review-local.osheet "$expected"
close_document
open_document 213 01-review-local.osheet
data_command 74 # Review Tracked Changes
expect tracked-reopened 227114dcadea8ba0220abceda571299ecb3355aa396ca88c83f63bcdf5a2284d
click 890 154
click 890 183
expect tracked-detail 62dfd20dcd39fb4a3a56122b66b1f189598958bff450f454e5f622311baa3a4f
close_document

open_document 273 03-merge-local.csv
data_command 102 # Merge Copies…
wait_for picker_open
expect merge-base-picker 229d162a56bf48eba575afeb5a3f054ec0a5ac02fd7e1843909ac020d8834226
pick 303
expect merge-incoming-picker 080e1c70e2b1a33c73727142f0128ab8035eeac62d40865b66eb1c634eec9515
pick 333
expect merge-collapsed c85afaa5719f21546daf02682751471999525c96c4c936c323197f5ba502468d
click 890 154
click 890 183
expect merge-detail da6d110f40fdfff0871349f96007a2141ba5e6919d16c812438e37f331e66d92
click 802 666 # Take Incoming for B1.
click 890 183
click 908 666 # Keep Mine for C1.
expect merge-resolved f2c6faae2b9c0d85997c574c6a232a7f1c7d077f0929c8c77f7c1e5b0cbb0d54
# D1 is our independent edit; E1 is the independent incoming edit.
save_as 03-merge-local.osheet 'merge,11,20,30,44'
close_document
open_document 303 03-merge-local.osheet
expect merge-reopened 2ac5609504f437d69af82168c11af764696bb4bc6dfdfc3138e511ee03c9e395
close_document
(cd "$books" && sha256 0*.csv) | cmp - "$results/sources.sha256"
echo "Sheets desktop review passed: $results"
