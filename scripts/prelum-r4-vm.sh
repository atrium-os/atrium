#!/bin/sh
# Prelum R4 in the FreeBSD VM (docs/spec/printing.md §5, §11): apps print
# through Limen's `print` role, prelum-sheet draws the sheet over the app's
# window with previews from a render jail, and only a real press on Print
# prints. Runs on the HOST after `sh scripts/cloud-vm.sh up`.
#
#   sh scripts/prelum-r4-vm.sh [out-dir]       # screenshots land in out-dir
#
# The printer is CUPS's ippeveprinter on this host (a test oracle, never
# shipped), which the guest reaches at 10.0.2.2:8661; alice adds it.
#
# Apps, each its own uid in the launch registry, all alice's:
#   50621 org.test.printdemo  prelum-print-demo (prints a page it makes)
#   50622 org.atrium.write    Write, Ctrl+P on a document
#   50623 org.atrium.pdf      Atrium PDF, Ctrl+P on a PDF
#   50624 org.atrium.sheets   Sheets, Ctrl+P on a sheet
#   50625 org.atrium.slides   Slides, Ctrl+P on a deck
#
# Checks:
#   - the press rule, the way Curia's Apps page is checked: with frescod not
#     vouching for injected input, a click on Print is not counted and
#     nothing prints; a process that isn't the sheet can't act for an app
#     (prelumctl --for); with presses vouched for, a click on Print queues
#     the job, which the printer prints;
#   - previews: the sheet draws the app's page from a render jail;
#   - a press beside the sheet reaches neither the sheet nor the app;
#   - Escape cancels; the app hears "cancelled", and nothing prints;
#   - Save as PDF: the sheet hands the app the rewritten PDF (the picker
#     that would take it is Scrinium's own gate);
#   - a window opening sheets as fast as it can is slowed by limend;
#   - Write, Atrium PDF, Sheets and Slides print, and the printer gets
#     their jobs.
#
#   ONLY="sheets slides" sh scripts/prelum-r4-vm.sh   # just those stages
#   (press rule, print, cancel, save, rate, write, pdf, sheets, slides)
# Ends "PASS: Prelum R4 in the VM" or "FAILED: N".
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/scratch/prelum-r4}"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
TARGET=aarch64-unknown-freebsd
HOSTIP=10.0.2.2
# Cross builds share one target folder (the per-crate ones fill the disk).
XT="${XT:-$HOME/ct-x}"
work=$(mktemp -d)
FAILS=0
STAGE=startup
pids=""
mkdir -p "$OUT"
port_up() { python3 -I -c "import socket,sys; socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1)" "$1" 2>/dev/null; }
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED [%s]: %s\n' "$STAGE" "$*" >&2; exit 1; }
want() { [ -z "${ONLY:-}" ] || case " $ONLY " in *" $1 "*) true ;; *) false ;; esac; }
check() { # check <label> <ok 0/1> <detail>
    if [ "$2" = 0 ]; then printf 'PASS %-62s %s\n' "$1" "$3"; else printf 'FAIL %-62s %s\n' "$1" "$3"; FAILS=$((FAILS+1)); fi
}
g() { "$ROOT/scripts/vssh" "$@"; }
put() { # SRC DEST (scp under TCG now and then cuts a file short: again)
    for _try in 1 2 3; do
        $SCP "$1" "root@localhost:$2" >/dev/null && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$(g "sha256 -q $2")" ] && return 0
    done
    die "$1 arrived damaged"
}
get() { $SCP "root@localhost:$1" "$2" >/dev/null 2>&1; }
P=/tmp/pr
cleanup() {
    g "pkill -f prelum-print-demo; pkill -x atrium-write; pkill -x atrium-pdf; pkill -x atrium-sheets; pkill -x atrium-slides; pkill -f atrium-prelum-sheet; pkill -x limend; pkill -x frescod; pkill -f 'prelumd --socket $P/prelum.sock'" >/dev/null 2>&1
    for f in frescod limend sheet prelumd demo-forged demo demo-escape demo-save demo-rate write pdf sheets slides; do get $P/$f.log "$OUT/$f.log"; done
    for p in $pids; do kill "$p" 2>/dev/null; done
    [ -n "${KEEP:-}" ] && echo "work dir: $work" || rm -rf "$work"
}
trap cleanup EXIT INT TERM

STAGE=build
say "$STAGE (cross, on this host)"
CROSS_RUSTFLAGS=$(cd "$ROOT" && sh -c '. ./scripts/cloud-vm.sh >/dev/null 2>&1; printf %s "$CROSS_RUSTFLAGS"' sh env)
[ -n "$CROSS_RUSTFLAGS" ] || die "cloud-vm.sh env"
export CC_aarch64_unknown_freebsd=clang AR_aarch64_unknown_freebsd=llvm-ar
export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$VMDIR/sysroot"
export CARGO_PROFILE_RELEASE_DEBUG=0
xb() { # DIR ARGS...
    d=$1; shift
    (cd "$ROOT/$d" && CARGO_TARGET_DIR="$XT" RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TARGET "$@") || die "cross-build of $d"
}
[ -n "${NO_BUILD:-}" ] || {
    xb prelum -p prelumd -p prelum-ipp -p prelum-render --bins
    xb frescod --bin frescod
    xb limen --bin limend
    xb prelum-sheet --bins
    xb atrium-pdf --bin atrium-pdf
    xb atrium-write --bin atrium-write
    xb atrium-sheets --bin atrium-sheets
    xb atrium-slides --bin atrium-slides
    (cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TARGET -p jaild) || die "jaild cross-build"
}
B="$XT/$TARGET/release"

STAGE=deploy
say "$STAGE"
g true || die "no ssh to the VM (sh scripts/cloud-vm.sh up)"
CROSS_RUSTFLAGS="$CROSS_RUSTFLAGS" RUSTFLAGS="$CROSS_RUSTFLAGS" START=1 sh "$ROOT/scripts/deploy-portcullis.sh" >"$work/deploy.log" 2>&1 \
    || grep -q "atrium-jaild  *RUNNING" "$work/deploy.log" \
    || { tail -20 "$work/deploy.log"; die "deploy-portcullis"; }
g "pkill -f prelum-print-demo; pkill -x atrium-write; pkill -x atrium-pdf; pkill -x atrium-sheets; pkill -x atrium-slides; pkill -f atrium-prelum-sheet; pkill -x limend; pkill -x frescod; pkill -f 'prelumd --socket $P/prelum.sock'; sleep 1; rm -rf $P; mkdir -p $P/bin; chmod 755 $P; true"
for b in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render atrium-prelum-sheet prelum-print-demo frescod limend atrium-pdf atrium-write atrium-sheets atrium-slides; do put "$B/$b" "$P/bin/$b"; done
put "$ROOT/prelum/prelum-ipp-codec/examples/min.pdf" $P/min.pdf
put "$ROOT/scratch/Ben.owrite" $P/Ben.owrite
# A sheet (CSV, read in the app) and a small deck with photos.
[ -s "$ROOT/scratch/prelum-r4/Q3.csv" ] || { mkdir -p "$ROOT/scratch/prelum-r4"; python3 -I -c "
import sys
rows=['Region,Quarter,Units,Revenue,Margin']+[f'Region {i%7},Q{i%4+1},{100+i*13},{(100+i*13)*42.5:.2f},{(i*7)%30/100:.2f}' for i in range(120)]
open(sys.argv[1],'w').write(chr(10).join(rows)+chr(10))" "$ROOT/scratch/prelum-r4/Q3.csv"; }
[ -s "$ROOT/scratch/prelum-r4/Deck.oslides" ] || (cd "$ROOT/atrium-slides" && CARGO_TARGET_DIR="$HOME/ct" cargo run -q --release --features local-decode --example big_deck -- gen "$ROOT/scratch/prelum-r4/Deck.oslides" 3 2) || die "the test deck"
put "$ROOT/scratch/prelum-r4/Q3.csv" $P/Q3.csv
put "$ROOT/scratch/prelum-r4/Deck.oslides" $P/Deck.oslides
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" "/usr/local/share/fonts/ibm-plex/$(basename "$f")"
done
. "$ROOT/scripts/vm-fonts.sh"; vm_install_fonts
REG=/var/run/atrium/app-registry; APPS=/var/lib/atrium/apps
g 'sh -s' <<EOS || die "setup"
set -e
for b in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render; do install -m 0555 $P/bin/\$b /usr/local/bin/\$b; done
chmod 0555 $P/bin/*; chmod 0444 $P/min.pdf $P/Ben.owrite $P/Q3.csv $P/Deck.oslides
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
id _prelum-sheet >/dev/null 2>&1 || pw useradd _prelum-sheet -u 65046 -d /nonexistent -s /usr/sbin/nologin
for u in "printdemo 50621" "printwrite 50622" "printpdf 50623" "printsheets 50624" "printslides 50625"; do
  set -- \$u
  id \$1 >/dev/null 2>&1 || pw useradd \$1 -u \$2 -d /nonexistent -s /usr/sbin/nologin
done
mkdir -p /var/run/atrium $APPS
sed -i '' '/ org.test.printdemo\$/d; / org.atrium.write\$/d; / org.atrium.pdf\$/d; / org.atrium.sheets\$/d; / org.atrium.slides\$/d' $REG 2>/dev/null || true
printf '50621 alice org.test.printdemo\n50622 alice org.atrium.write\n50623 alice org.atrium.pdf\n50624 alice org.atrium.sheets\n50625 alice org.atrium.slides\n' >> $REG
for a in org.test.printdemo org.atrium.write org.atrium.pdf org.atrium.sheets org.atrium.slides; do
  mkdir -p $APPS/\$a; printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/app"\n\n[capabilities]\ngraphics = "fresco"\n' \$a \$a > $APPS/\$a/atrium.toml
done
# A third-party app names the roles it embeds (platform apps may embed any).
printf 'embeds = ["print", "picker"]\n' >> $APPS/org.test.printdemo/atrium.toml
mkdir -p $P/home-write $P/home-pdf $P/home-sheets $P/home-slides; chown printwrite $P/home-write; chown printpdf $P/home-pdf; chown printsheets $P/home-sheets; chown printslides $P/home-slides
EOS
check "binaries, users, the registry and manifests" 0 ""

STAGE=printer
say "$STAGE (ippeveprinter on this host)"
command -v ippeveprinter >/dev/null || die "ippeveprinter not installed (cups-ipp-utils)"
if ! avahi-daemon -c 2>/dev/null; then
    mkdir -p /run/dbus
    dbus-daemon --system --fork 2>/dev/null || { rm -f /run/dbus/pid /run/dbus/system_bus_socket; dbus-daemon --system --fork; }
    rm -f /var/run/avahi-daemon/pid; avahi-daemon -D; sleep 1
fi
preload=""
if [ ! -e /proc/net/if_inet6 ]; then
    cc -shared -fPIC -o "$work/noipv6.so" "$ROOT/scripts/prelum-oracle-noipv6.c" -ldl
    preload="$work/noipv6.so"
fi
pkill -f "ippeveprinter -p 8661" 2>/dev/null
mkdir -p "$work/a" "$work/keys-a"
LD_PRELOAD=$preload ippeveprinter -p 8661 -n 127.0.0.1 -d "$work/a" -K "$work/keys-a" -k -f application/pdf,image/pwg-raster -M Atrium -m "VM R4" "Prelum VM R4" >"$work/a.log" 2>&1 &
pids="$pids $!"
for i in $(seq 50); do port_up 8661 && break; sleep 0.2; done
port_up 8661 || die "printer did not start"
printed() { grep -c "Job [0-9]* .*completed\|job-state=completed\|Job .* completed" "$work/a.log" 2>/dev/null || true; }
jobs_got() { ls "$work/a" 2>/dev/null | grep -c . ; }

STAGE=prelumd
say "$STAGE (the sheet may act for apps)"
g "rm -rf $P/state $P/spool; daemon -f -o $P/prelumd.log atrium-prelumd --socket $P/prelum.sock --state $P/state --spool $P/spool --sheet-uid 65046 --registry $REG </dev/null >/dev/null 2>&1; for i in \$(seq 120); do [ -S $P/prelum.sock ] && break; sleep 0.5; done; chmod 666 $P/prelum.sock; test -S $P/prelum.sock" || die "prelumd did not start: $(g "cat $P/prelumd.log")"
actl() { g "su -m alice -c 'prelumctl --socket $P/prelum.sock $*'"; }
out=$(actl add ipps://$HOSTIP:8661/ipp/print 2>&1)
check "alice adds the printer" $? "$(echo "$out" | head -1)"
out=$(g "prelumctl --socket $P/prelum.sock --for 50621 print $P/min.pdf --printer 1 2>&1")
echo "$out" | grep -qi "only the print sheet" ; check "root can't act for an app (not the sheet)" $? "$(echo "$out" | head -1)"
out=$(g "su -m printdemo -c 'prelumctl --socket $P/prelum.sock --for 50621 jobs' 2>&1")
echo "$out" | grep -qi "only the print sheet" ; check "nor can the app itself" $? "$(echo "$out" | head -1)"

inj() { g "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $P/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep ${GAP:-6}; }
key() { inj "KEY $1 1 ${2:-0}"; sleep 1; inj "KEY $1 0 ${2:-0}"; sleep ${GAP:-6}; }
shot() { sleep ${2:-4}; get $P/screen.png "$OUT/$1.png" || get $P/screen "$OUT/$1.png"; [ -s "$OUT/$1.png" ] && echo "  screenshot $OUT/$1.png" || echo "  (no screenshot for $1)"; }
waitfor() { # FILE PATTERN SECONDS
    i=0; while [ $i -lt "$3" ] && ! g "grep -q '$2' $1"; do sleep 3; i=$((i+3)); done
    g "grep -q '$2' $1"
}
start_desk() { # INJECT_ACTIVATION (0/1)
    DESK=$1
    g "pkill -f atrium-prelum-sheet; pkill -x limend; pkill -x frescod; sleep 1; rm -f $P/*.sock.* $P/fresco.sock $P/limend.sock $P/activation.sock $P/inject.sock $P/wm.sock"
    # Under TCG a press takes seconds to reach the sheet: tokens live longer.
    act=""; [ "$1" = 1 ] && act="FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=15000"
    g "cd $P && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$P/screen FRESCOD_SOCK=$P/fresco.sock FRESCOD_WM_SOCK=$P/wm.sock \
       FRESCOD_INPUT_SOCK=$P/inject.sock FRESCOD_ACTIVATION_SOCK=$P/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all $act \
       daemon -f -o $P/frescod.log $P/bin/frescod </dev/null"
    i=0; while [ $i -lt 60 ] && ! g "test -S $P/fresco.sock && test -S $P/activation.sock"; do sleep 2; i=$((i+1)); done
    g "chmod 666 $P/fresco.sock $P/activation.sock $P/inject.sock" || die "frescod: $(g "tail -5 $P/frescod.log")"
    g "cd $P && env LIMEN_SOCKET=$P/limend.sock LIMEN_FRESCOD_ACTIVATION=$P/activation.sock LIMEN_APP_REGISTRY=$REG daemon -f -o $P/limend.log $P/bin/limend </dev/null"
    i=0; while [ $i -lt 20 ] && ! g "test -S $P/limend.sock"; do sleep 1; i=$((i+1)); done
    g "chmod 666 $P/limend.sock" || die "limend: $(g "tail -5 $P/limend.log")"
    g ": > $P/sheet.log; daemon -f -o $P/sheet.log -u _prelum-sheet env HOME=/nonexistent LIMEN_SOCKET=$P/limend.sock FRESCO_SOCKET=$P/fresco.sock PRELUM_SOCKET=$P/prelum.sock PRELUM_SHEET_ACTIVATION=$P/activation.sock PRELUM_SHEET_NEARBY=0 $P/bin/atrium-prelum-sheet"
    waitfor $P/sheet.log "providing print" 30 || die "the sheet: $(g "tail -5 $P/sheet.log $P/limend.log")"
}
app() { # USER LOG ENV... BINARY ARGS
    u=$1 l=$2; shift 2
    g ": > $P/$l.log; daemon -f -o $P/$l.log -u $u env FRESCO_SOCKET=$P/fresco.sock LIMEN_SOCKET=$P/limend.sock $*"
}
# frescod centres a sheet across its parent, 48 points down; the app's
# window is at 0,0. The sheet is 760 x 540; Print is at its bottom right.
SW=760; SH=540; TOP=48
print_at() { echo "$(( ($1 - SW) / 2 + SW - 80 )) $(( TOP + SH - 36 ))"; }
DEMO_W=900

if want press; then
STAGE="press rule"
say "$STAGE: presses frescod doesn't vouch for"
start_desk 0
check "frescod, limend and the sheet (uid 65046) up" 0 ""
app printdemo demo-forged "PRINT_TITLE=Unvouched $P/bin/prelum-print-demo"
waitfor $P/sheet.log "sheet .* for org.test.printdemo" 90; check "the demo asked; the sheet opened for org.test.printdemo" $? "$(g "grep 'for org.test' $P/sheet.log | tail -1")"
sleep ${WAIT:-40}
shot sheet-unvouched
press $(print_at $DEMO_W)
waitfor $P/sheet.log "not counted" 20; check "a press frescod doesn't vouch for is not counted" $? "$(g "grep 'not counted' $P/sheet.log | tail -1")"
before=$(jobs_got)
sleep 10
check "nothing reached the printer" "$([ "$(jobs_got)" = "$before" ] && ! g "grep -q queued $P/demo-forged.log"; echo $?)" ""
g "pkill -f prelum-print-demo"; sleep 3
fi

if want print; then
STAGE="print"
say "$STAGE: real presses"
start_desk 1
app printdemo demo "PRINT_TITLE=Quarterly $P/bin/prelum-print-demo"
waitfor $P/sheet.log "sheet .* for org.test.printdemo" 90; check "the sheet opened over the demo" $? ""
sleep ${WAIT:-40}
shot sheet
g "grep -q 'previews: [1-9]' $P/sheet.log" ; check "previews drawn in a render jail" $? "$(g "grep 'previews:' $P/sheet.log | tail -1")"
press 20 600                                  # the app, beside the sheet
press 450 20                                  # the app, above the sheet
g "grep -q 'the app got a press' $P/demo.log" && check "presses beside the sheet don't reach the app" 1 "" || check "presses beside the sheet don't reach the app" 0 ""
before=$(jobs_got)
press $(print_at $DEMO_W)
waitfor $P/demo.log "print-demo: queued" 120; check "a click on Print: the app hears queued" $? "$(g "tail -1 $P/demo.log")"
i=0; while [ $i -lt 180 ] && [ "$(jobs_got)" = "$before" ]; do sleep 3; i=$((i+3)); done
check "the printer got the job" "$([ "$(jobs_got)" != "$before" ]; echo $?)" "$(ls "$work/a" | tail -1)"
j=$(actl jobs 2>&1 | grep -c "Quarterly")
check "it is alice's job, titled by the app" "$([ "$j" -ge 1 ]; echo $?)" "$(actl jobs 2>&1 | grep Quarterly | head -1)"
shot queued
g "pkill -f prelum-print-demo"; sleep 3
fi

if want cancel; then
STAGE=cancel
[ "${DESK:-}" = 1 ] || start_desk 1
say "$STAGE"
app printdemo demo-escape "PRINT_TITLE=Escape $P/bin/prelum-print-demo"
waitfor $P/sheet.log "sheet .* for org.test.printdemo" 90; sleep ${WAIT:-40}
before=$(jobs_got)
key 0x29
waitfor $P/demo-escape.log "print-demo: cancelled" 30; check "Escape: the app hears cancelled" $? "$(g "tail -1 $P/demo-escape.log")"
check "and nothing printed" "$([ "$(jobs_got)" = "$before" ]; echo $?)" ""
g "pkill -f prelum-print-demo"; sleep 3
fi

if want save; then
STAGE="save as pdf"
[ "${DESK:-}" = 1 ] || start_desk 1
say "$STAGE"
app printdemo demo-save "PRINT_TITLE=Saved $P/bin/prelum-print-demo"
waitfor $P/sheet.log "sheet .* for org.test.printdemo" 90; sleep ${WAIT:-40}
# The destinations are a list under the title; Save as PDF is its last row.
press $(( (DEMO_W - SW) / 2 + ${SX:-450} )) $(( TOP + ${SY:-95} ))
shot save-chosen 2
press $(print_at $DEMO_W)
waitfor $P/sheet.log "saved as PDF, to the app" 120; check "Save as PDF: the sheet hands the app the rewritten PDF" $? "$(g "grep -E 'saved|not counted|Failed' $P/sheet.log | tail -1")"
waitfor $P/demo-save.log "print-demo: \(saved\|failed\)" 30
g "grep -q \"failed The picker didn't open\" $P/demo-save.log"; check "and asks the picker where it goes (none here: refused)" $? "$(g "tail -1 $P/demo-save.log")"
g "pkill -f prelum-print-demo"; sleep 3
fi

if want rate; then
STAGE="rate"
[ "${DESK:-}" = 1 ] || start_desk 1
say "$STAGE"
app printdemo demo-rate "PRINT_AGAIN=6 PRINT_DROP=1 PRINT_SECS=60 $P/bin/prelum-print-demo"
waitfor $P/demo-rate.log "refused" 120
n=$(g "grep -c 'and closed it' $P/demo-rate.log"); r=$(g "grep 'refused' $P/demo-rate.log | head -1")
check "a window opening sheets as fast as it can: refused after three" "$([ "$n" = 3 ] && echo "$r" | grep -q 'Refused'; echo $?)" "$n opened; $r"
g "pkill -f prelum-print-demo"; sleep 3
fi

if want write; then
STAGE=write
[ "${DESK:-}" = 1 ] || start_desk 1
say "$STAGE"
app printwrite write "HOME=$P/home-write $P/bin/atrium-write $P/Ben.owrite"
sleep ${WAIT_WRITE:-90}
shot write-0
key 0x13 0x01                                 # Ctrl+P
waitfor $P/sheet.log "sheet .* for org.atrium.write" 120; check "Ctrl+P in Write opens the sheet" $? ""
sleep ${WAIT:-40}
shot write-sheet
before=$(jobs_got)
press $(print_at 1180)
i=0; while [ $i -lt 240 ] && [ "$(jobs_got)" = "$before" ]; do sleep 3; i=$((i+3)); done
check "Write's document printed" "$([ "$(jobs_got)" != "$before" ]; echo $?)" "$(g "grep 'sheet .*: queued' $P/sheet.log | tail -1")"
shot write-done
g "pkill -x atrium-write"; sleep 3
fi

if want pdf; then
STAGE=pdf
[ "${DESK:-}" = 1 ] || start_desk 1
say "$STAGE"
app printpdf pdf "HOME=$P/home-pdf $P/bin/atrium-pdf --file $P/min.pdf"
sleep 30
key 0x13 0x01
waitfor $P/sheet.log "sheet .* for org.atrium.pdf" 120; check "Ctrl+P in Atrium PDF opens the sheet" $? ""
sleep ${WAIT:-40}
shot pdf-sheet
before=$(jobs_got)
press $(print_at 1000)
waitfor $P/pdf.log "print: Queued" 120; check "Atrium PDF hears queued" $? "$(g "grep 'print:' $P/pdf.log | tail -1")"
i=0; while [ $i -lt 180 ] && [ "$(jobs_got)" = "$before" ]; do sleep 3; i=$((i+3)); done
check "the PDF printed" "$([ "$(jobs_got)" != "$before" ]; echo $?)" ""
fi

# Sheets and Slides (officina-pdf's sheet and deck output): a page per
# 48 rows of the CSV, a page per slide; previews say how many.
if want sheets; then
STAGE=sheets
[ "${DESK:-}" = 1 ] || start_desk 1
say "$STAGE"
app printsheets sheets "HOME=$P/home-sheets $P/bin/atrium-sheets $P/Q3.csv"
sleep ${WAIT_SHEETS:-90}
shot sheets-0
key 0x13 0x01                                 # Ctrl+P
waitfor $P/sheet.log "sheet .* for org.atrium.sheets" 120; check "Ctrl+P in Sheets opens the sheet" $? ""
sleep ${WAIT:-40}
shot sheets-sheet
s=$(g "grep 'previews:' $P/sheet.log | tail -1")
echo "$s" | grep -q "of 3 sheets"; check "the CSV's 121 rows are 3 pages" $? "$s"
before=$(jobs_got)
press $(print_at 1100)
i=0; while [ $i -lt 240 ] && [ "$(jobs_got)" = "$before" ]; do sleep 3; i=$((i+3)); done
check "Sheets' pages printed" "$([ "$(jobs_got)" != "$before" ]; echo $?)" "$(g "grep 'sheet .*: queued' $P/sheet.log | tail -1")"
shot sheets-done
g "pkill -x atrium-sheets"; sleep 3
fi

if want slides; then
STAGE=slides
[ "${DESK:-}" = 1 ] || start_desk 1
say "$STAGE"
app printslides slides "HOME=$P/home-slides $P/bin/atrium-slides $P/Deck.oslides"
sleep ${WAIT_SLIDES:-120}
shot slides-0
key 0x13 0x01                                 # Ctrl+P
waitfor $P/sheet.log "sheet .* for org.atrium.slides" 180; check "Ctrl+P in Slides opens the sheet" $? ""
sleep ${WAIT:-40}
shot slides-sheet
s=$(g "grep 'previews:' $P/sheet.log | tail -1")
echo "$s" | grep -q "of 3 sheets"; check "a page for each of the 3 slides" $? "$s"
before=$(jobs_got)
press $(print_at 1200)
i=0; while [ $i -lt 300 ] && [ "$(jobs_got)" = "$before" ]; do sleep 3; i=$((i+3)); done
check "Slides' pages printed" "$([ "$(jobs_got)" != "$before" ]; echo $?)" "$(g "grep 'sheet .*: queued' $P/sheet.log | tail -1")"
shot slides-done
g "pkill -x atrium-slides"; sleep 3
fi

echo
if [ "$FAILS" = 0 ]; then echo "PASS: Prelum R4 in the VM"; else echo "FAILED: $FAILS"; exit 1; fi
