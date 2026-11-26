#!/bin/sh
# Record frescod sessions for the Fenestra stream gate (docs/spec/fenestra.md
# §5.6, §19 R1): a Pergola app (pergola's `form` example), Navigator
# (navigator-ui's scenes) and Officina (atrium-sheets on a small sheet), each
# on a headless in-process frescod built with `fenestra-record`, driven
# through frescod's input injector (clicks, keys, scrolls). The recordings
# land in OUT (default ~/atrium-vm/fenestra-rec) for
# scripts/fenestra-stream-check.sh, which replays them on the host.
#
#   sh scripts/cloud-vm.sh up        # or the Mac VM, with the coordinator's OK
#   sh scripts/fenestra-record-vm.sh [out-dir]
#   APPS="sheets" WAIT=30 sh scripts/fenestra-record-vm.sh
#
# A recording holds everything the apps drew; only these synthetic sessions
# are recorded, and only by a development frescod (the feature is off in
# every other build).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$HOME/atrium-vm/fenestra-rec}"
APPS="${APPS:-pergola navigator sheets}"
WAIT="${WAIT:-45}"
GAP="${GAP:-4}"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p "${VM_SSH_PORT:-2222}" root@localhost "$@"; }
put() { scp $SSHOPT -P "${VM_SSH_PORT:-2222}" -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P "${VM_SSH_PORT:-2222}" -q "root@localhost:$1" "$2"; }
die() { echo "fenestra-record-vm: $*" >&2; exit 1; }

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2); }
xb frescod "--features fenestra-record --bin frescod"
BINS="$ROOT/frescod/target/$TARGET/release/frescod"
for a in $APPS; do
    case $a in
        pergola) xb pergola "--example form"; BINS="$BINS $ROOT/pergola/target/$TARGET/release/examples/form" ;;
        navigator) xb navigator-ui "--bin navigator-ui"; BINS="$BINS $ROOT/navigator-ui/target/$TARGET/release/navigator-ui" ;;
        sheets) xb atrium-sheets "--bin atrium-sheets"; BINS="$BINS $ROOT/atrium-sheets/target/$TARGET/release/atrium-sheets" ;;
        *) die "unknown app $a (pergola, navigator, sheets)" ;;
    esac
done

D=/opt/fenestra-rec
S=/tmp/fenrec
g "pkill -x frescod; pkill -x form; pkill -x navigator-ui; pkill -x atrium-sheets; sleep 1; rm -rf $D $S; mkdir -p $D $S; chmod 777 $S; true"
for b in $BINS; do
    put "$b" "$D/"
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || die "hash mismatch for $(basename "$b")"
done
. "$ROOT/scripts/vm-fonts.sh"; vm_install_fonts
mkdir -p "$OUT"

inj() { g "printf '%s\\n' \"$1\" | nc -N -U $S/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2"; inj "BTN 1 1"; inj "BTN 1 0"; sleep "$GAP"; }
key() { inj "KEY $1 1 ${2:-0}"; inj "KEY $1 0 ${2:-0}"; sleep 1; }
# HID usages: a=4 ... z=29, 1=30 ... 0=39, Enter 40, Tab 43, Space 44,
# Backspace 42, right/left/down/up 79-82, Page Down 78.
typeword() { for k in "$@"; do key "$k"; done; sleep "$GAP"; }
scroll() { inj "MOVE $1 $2"; for _ in 1 2 3 4 5; do inj "SCROLL $3"; sleep 1; done; sleep "$GAP"; }

start_frescod() { # start_frescod <name>
    g "rm -f $S/$1.frec $S/fresco.sock $S/inject.sock; cd $S && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock \
        FRESCOD_WM_SOCK=$S/wm.sock FRESCOD_INPUT_SOCK=$S/inject.sock FRESCO_RECORD=$S/$1.frec daemon -f -o $S/frescod-$1.log $D/frescod </dev/null"
    i=0; while [ $i -lt 90 ] && ! g "test -S $S/fresco.sock && test -S $S/inject.sock"; do sleep 2; i=$((i+1)); done
    g "test -S $S/fresco.sock" || { g "tail -20 $S/frescod-$1.log"; die "frescod did not listen"; }
    g "chmod 666 $S/fresco.sock"
}
finish() { # finish <name> <app binary name>
    g "pkill -x $2; sleep 3; pkill -x frescod; sleep 2; true"
    get "$S/$1.frec" "$OUT/$1.frec" || die "no recording for $1"
    g "cp $S/screen.png $S/$1.png 2>/dev/null; true"
    get "$S/$1.png" "$OUT/$1.png" 2>/dev/null || true
    echo "  $OUT/$1.frec ($(wc -c <"$OUT/$1.frec") bytes)"
}

for a in $APPS; do
    echo "=== $a"
    start_frescod "$a"
    case $a in
        pergola)
            g "cd $S && env HOME=$S FRESCO_SOCKET=$S/fresco.sock daemon -f -o $S/app-$a.log $D/form </dev/null"
            sleep "$WAIT"
            press 200 120; typeword 11 8 15 15 18 44 26 18 21 15 7    # "hello world"
            key 43; typeword 30 31 32; key 42; key 42                 # Tab, "123", two Backspaces
            press 120 220; press 300 220; key 40
            scroll 200 200 -120; scroll 200 200 120
            sleep "$WAIT"
            finish "$a" form
            ;;
        navigator)
            g "cd $S && env FRESCO_SOCKET=$S/fresco.sock NAVIGATOR_UI_SIZE=1280x720 daemon -f -o $S/app-$a.log $D/navigator-ui --light </dev/null"
            sleep "$WAIT"
            press 700 254; sleep "$GAP"                               # a link on the stand-in page
            scroll 640 400 -240; scroll 640 400 240
            press 400 20; typeword 7 18 6 8 55 4 21 23 9 18 16 40   # the address bar: "docs.atrium"
            sleep "$WAIT"
            finish "$a" navigator-ui
            ;;
        sheets)
            CSV=$(mktemp)
            { echo "Region,Q1,Q2,Q3,Q4,Total"; i=2
              for r in North South East West Central Islands Coast Hills; do
                  echo "$r,$((i*1130+17)),$((i*1207+3)),$((i*990+411)),$((i*1402+9)),=SUM(B$i:E$i)"; i=$((i+1)); done; } >"$CSV"
            put "$CSV" "$S/sales.csv"; rm -f "$CSV"
            g "cd $S && env HOME=$S FRESCO_SOCKET=$S/fresco.sock daemon -f -o $S/app-$a.log $D/atrium-sheets $S/sales.csv </dev/null"
            sleep "$WAIT"
            press 300 200; typeword 30 31 32 40                        # 123, Enter
            key 79; key 79; key 81; typeword 37 39 40                   # move, 80
            press 500 260; key 78; key 78                               # Page Down
            scroll 600 400 -360; scroll 600 400 360
            sleep "$WAIT"
            finish "$a" atrium-sheets
            ;;
    esac
done
echo "=== recordings in $OUT"
ls -l "$OUT"
