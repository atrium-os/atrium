#!/bin/sh
# Prelum R5 in the FreeBSD VM (docs/spec/printing.md §6.4–§6.6, §11): Curia's
# Printers page, the Forum status item and Praeco notices, and history. Runs
# on the HOST after `sh scripts/cloud-vm.sh up`.
#
#   sh scripts/prelum-r5-vm.sh [out-dir]       # screenshots land in out-dir
#   ONLY="curia status" sh scripts/prelum-r5-vm.sh   (access, curia, status, history)
#
# Printers, both on this host (test oracles, never shipped), which the guest
# reaches at 10.0.2.2:
#   8661  CUPS's ippeveprinter, IPPS
#   8662  scripts/prelum-fake-printer.py, plain IPP, out of paper on cue
#
# Apps, each its own uid in the launch registry, all alice's:
#   50621 org.test.printdemo        any other app: refused by prelumd
#   50626 org.atrium.curia          Settings, the Printers page
#   50627 org.atrium.prelum-status  the status item and notices
#
# Checks:
#   - only Curia and prelum-status act for their person, each with its own
#     ops; any other app is refused;
#   - Curia's Printers page, by clicks and typing: add two printers by
#     address, identify one, rename it, share it, remove the other (asked
#     twice);
#   - out of paper: the job waits with the alert, prelum-status puts the
#     status item up (Forum's forum-wm lists it) and posts a notice to
#     praecod; paper back, the job prints, the notice is withdrawn, "Printed"
#     is posted quietly and the item goes;
#   - offline: told only once it has lasted (PRELUM_OFFLINE_AFTER, 30 s
#     here); Cancel pressed in the status item cancels the job;
#   - history: the jobs are in it with their results; Curia's Clear (asked
#     twice) empties it.
# Ends "PASS: Prelum R5 in the VM" or "FAILED: N".
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/scratch/prelum-r5}"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
TARGET=aarch64-unknown-freebsd
HOSTIP=10.0.2.2
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
put() {
    for _try in 1 2 3; do
        $SCP "$1" "root@localhost:$2" >/dev/null && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$(g "sha256 -q $2")" ] && return 0
    done
    die "$1 arrived damaged"
}
get() { $SCP "root@localhost:$1" "$2" >/dev/null 2>&1; }
P=/tmp/pr5
stop_all() { g "pkill -x curia; pkill -x prelum-status; pkill -x forum-wm; pkill -x praecod; pkill -x frescod; pkill -f 'prelumd --socket $P/prelum.sock'" >/dev/null 2>&1; }
cleanup() {
    stop_all
    for f in frescod prelumd praecod curia curia2 status wm; do get $P/$f.log "$OUT/$f.log"; done
    cp "$work/fake.log" "$OUT/fake.log" 2>/dev/null
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
    xb curia --bin curia
    xb prelum-status --bin prelum-status
    xb praeco --bin praecod
    xb forum-wm --bin forum-wm
    xb forum-ctl --bin forum-ctl
    (cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TARGET -p jaild) || die "jaild cross-build"
}
B="$XT/$TARGET/release"

STAGE=deploy
say "$STAGE"
g true || die "no ssh to the VM (sh scripts/cloud-vm.sh up)"
CROSS_RUSTFLAGS="$CROSS_RUSTFLAGS" RUSTFLAGS="$CROSS_RUSTFLAGS" START=1 sh "$ROOT/scripts/deploy-portcullis.sh" >"$work/deploy.log" 2>&1 \
    || grep -q "atrium-jaild  *RUNNING" "$work/deploy.log" \
    || { tail -20 "$work/deploy.log"; die "deploy-portcullis"; }
stop_all; g "sleep 1; rm -rf $P; mkdir -p $P/bin; chmod 755 $P; true"
for b in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render frescod curia prelum-status praecod forum-wm forum-ctl; do put "$B/$b" "$P/bin/$b"; done
put "$ROOT/prelum/prelum-ipp-codec/examples/min.pdf" $P/min.pdf
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" "/usr/local/share/fonts/ibm-plex/$(basename "$f")"
done
. "$ROOT/scripts/vm-fonts.sh"; vm_install_fonts
REG=/var/run/atrium/app-registry; APPS=/var/lib/atrium/apps
g 'sh -s' <<EOS || die "setup"
set -e
for b in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render; do install -m 0555 $P/bin/\$b /usr/local/bin/\$b; done
chmod 0555 $P/bin/*; chmod 0444 $P/min.pdf
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
for u in "printdemo 50621" "printcuria 50626" "printstatus 50627"; do
  set -- \$u
  id \$1 >/dev/null 2>&1 || pw useradd \$1 -u \$2 -d /nonexistent -s /usr/sbin/nologin
done
mkdir -p /var/run/atrium $APPS
sed -i '' '/ org.test.printdemo\$/d; / org.atrium.curia\$/d; / org.atrium.prelum-status\$/d' $REG 2>/dev/null || true
printf '50621 alice org.test.printdemo\n50626 alice org.atrium.curia\n50627 alice org.atrium.prelum-status\n' >> $REG
for a in org.test.printdemo org.atrium.curia org.atrium.prelum-status; do
  mkdir -p $APPS/\$a; printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/app"\n\n[capabilities]\ngraphics = "fresco"\n' \$a \$a > $APPS/\$a/atrium.toml
done
EOS
check "binaries, users, the registry and manifests" 0 ""

STAGE=printers
say "$STAGE (on this host)"
command -v ippeveprinter >/dev/null || die "ippeveprinter not installed (cups-ipp-utils)"
preload=""
if [ ! -e /proc/net/if_inet6 ]; then
    cc -shared -fPIC -o "$work/noipv6.so" "$ROOT/scripts/prelum-oracle-noipv6.c" -ldl
    preload="$work/noipv6.so"
fi
pkill -f "ippeveprinter -p 8661" 2>/dev/null; pkill -f "prelum-fake-printer.py --port 8662" 2>/dev/null
mkdir -p "$work/a" "$work/keys-a"
LD_PRELOAD=$preload ippeveprinter -p 8661 -n 127.0.0.1 -d "$work/a" -K "$work/keys-a" -k -f application/pdf,image/pwg-raster -M Atrium -m "VM R5" "Prelum VM R5" >"$work/a.log" 2>&1 &
pids="$pids $!"
echo idle > "$work/fstate"
fake_up() {
    python3 -I "$ROOT/scripts/prelum-fake-printer.py" --port 8662 --state "$work/fstate" --spool "$work/fspool" --name "Bench" >>"$work/fake.log" 2>&1 &
    fakepid=$!; pids="$pids $fakepid"
    for i in $(seq 50); do port_up 8662 && break; sleep 0.2; done
}
fake_up
for i in $(seq 50); do port_up 8661 && break; sleep 0.2; done
port_up 8661 && port_up 8662 || die "the printers did not start"

STAGE=services
say "$STAGE (prelumd, praecod)"
g "rm -rf $P/state $P/spool; daemon -f -o $P/prelumd.log atrium-prelumd --socket $P/prelum.sock --state $P/state --spool $P/spool --sheet-uid 65046 --registry $REG </dev/null >/dev/null 2>&1; for i in \$(seq 120); do [ -S $P/prelum.sock ] && break; sleep 0.5; done; chmod 666 $P/prelum.sock; test -S $P/prelum.sock" || die "prelumd did not start: $(g "cat $P/prelumd.log")"
g "cd $P && env PRAECO_SOCKET=$P/praeco.sock PRAECO_APP_REGISTRY=$REG PRAECO_SUBSCRIBERS=0 daemon -f -o $P/praecod.log $P/bin/praecod </dev/null; for i in \$(seq 20); do [ -S $P/praeco.sock ] && break; sleep 0.5; done; chmod 666 $P/praeco.sock; test -S $P/praeco.sock" || die "praecod did not start"
actl() { g "su -m alice -c 'prelumctl --socket $P/prelum.sock $*'"; }
as() { u=$1; shift; g "su -m $u -c 'prelumctl --socket $P/prelum.sock $*'" 2>&1; }
check "prelumd and praecod up" 0 ""

if want access; then
STAGE=access
say "$STAGE: who acts for alice"
out=$(as printdemo printers); echo "$out" | grep -q "apps print through the print sheet"; check "an app is refused (it prints through the sheet)" $? "$(echo "$out" | head -1)"
out=$(as printstatus printers); [ $? = 0 ] && ! echo "$out" | grep -q prelumctl; check "prelum-status lists alice's printers" $? "$(echo "$out" | head -1)"
out=$(as printstatus add ipp://$HOSTIP:8662/ipp/print); echo "$out" | grep -q "not for this app"; check "but may not add one" $? "$(echo "$out" | head -1)"
out=$(as printstatus history); echo "$out" | grep -q "not for this app"; check "nor read history" $? "$(echo "$out" | head -1)"
out=$(as printcuria history); [ $? = 0 ]; check "Curia reads alice's history" $? "$(echo "$out" | head -1)"
out=$(g "su -m printcuria -c 'prelumctl --socket $P/prelum.sock print $P/min.pdf --printer 1'" 2>&1); echo "$out" | grep -q "not for this app"; check "Curia may not print" $? "$(echo "$out" | head -1)"
fi

inj() { g "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $P/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep ${GAP:-6}; }
# Text, as HID usages (shift for ':'), all in one go.
typ() {
    lines=$(python3 -I -c '
import sys
m = {**{c: 4 + i for i, c in enumerate("abcdefghijklmnopqrstuvwxyz")}, **{c: 0x1e + i for i, c in enumerate("123456789")}, "0": 0x27, ".": 0x37, "/": 0x38, "-": 0x2d, " ": 0x2c}
for c in sys.argv[1]:
    k, s = (0x33, 2) if c == ":" else (m[c.lower()], 2 if c.isupper() else 0)
    print("KEY %d 1 %d" % (k, s)); print("KEY %d 0 %d" % (k, s))' "$1")
    printf '%s\n' "$lines" | g "nc -N -U $P/inject.sock" >/dev/null; sleep ${GAP:-6}
}
shot() { sleep ${2:-4}; get $P/screen.png "$OUT/$1.png" || get $P/screen "$OUT/$1.png"; [ -s "$OUT/$1.png" ] && echo "  screenshot $OUT/$1.png" || echo "  (no screenshot for $1)"; }
waitfor() { # FILE PATTERN SECONDS
    i=0; while [ $i -lt "$3" ] && ! g "grep -q '$2' $1"; do sleep 3; i=$((i+3)); done
    g "grep -q '$2' $1"
}
start_desk() {
    g "pkill -x curia; pkill -x prelum-status; pkill -x forum-wm; pkill -x frescod; sleep 1; rm -f $P/fresco.sock $P/inject.sock $P/activation.sock $P/wm.sock $P/ctl.sock $P/status.sock"
    g "cd $P && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$P/screen FRESCOD_SOCK=$P/fresco.sock FRESCOD_WM_SOCK=$P/wm.sock \
       FRESCOD_INPUT_SOCK=$P/inject.sock FRESCOD_ACTIVATION_SOCK=$P/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all FRESCOD_INJECT_ACTIVATION=1 \
       daemon -f -o $P/frescod.log $P/bin/frescod </dev/null"
    i=0; while [ $i -lt 60 ] && ! g "test -S $P/fresco.sock && test -S $P/inject.sock"; do sleep 2; i=$((i+1)); done
    g "chmod 666 $P/fresco.sock $P/inject.sock" || die "frescod: $(g "tail -5 $P/frescod.log")"
}
curia() { # LOG
    g ": > $P/$1.log; daemon -f -o $P/$1.log -u printcuria env HOME=/nonexistent FRESCO_SOCKET=$P/fresco.sock CURIA_PRINT_SOCK=$P/prelum.sock CURIA_NET_DIR=$P/none $P/bin/curia"
    sleep ${WAIT:-40}
}
# Curia's Printers page at 100% text: the page's left edge is at 232 and
# its list starts 96 down; each printer is a card 104 apart, its buttons
# in slots 120 wide on its fourth line (Identify, Rename, Share, Remove).
CARD0=96; CARDH=104
slot() { echo "$((232 + 16 + $2 * 120 + 30)) $(($CARD0 + $1 * $CARDH + 56 + 15))"; } # CARD SLOT
add_field() { echo "$((232 + 100)) $(($CARD0 + $1 * $CARDH + ${2:-0} + 12 + 36 + 16))"; } # CARDS [EMPTY 28]
add_button() { echo "$((232 + 368 + 20)) $(($CARD0 + $1 * $CARDH + ${2:-0} + 12 + 36 + 17))"; }

if want curia; then
STAGE=curia
say "$STAGE: the Printers page"
start_desk
curia curia
press 100 339                                       # Printers, the 8th in the sidebar
shot curia-empty
press $(add_field 0 28); typ "ipps://$HOSTIP:8661/ipp/print"; press $(add_button 0 28)
waitfor $P/curia.log "curia: printers: added" 90; check "Add by address (IPPS)" $? "$(g "grep 'curia: printers: add' $P/curia.log | tail -1")"
sleep 8
press $(add_field 1); typ "ipp://$HOSTIP:8662/ipp/print"; press $(add_button 1)
waitfor $P/curia.log "added Bench" 90; check "Add by address (plain IPP)" $? "$(g "grep 'curia: printers: add' $P/curia.log | tail -1")"
n=$(actl printers 2>&1 | grep -c '^ *[0-9]  ')
check "alice has the two printers" "$([ "$n" = 2 ]; echo $?)" "$n"
sleep 8; shot curia-two
before=$(grep -c "Identify-Printer" "$work/a.log")
press $(slot 0 0)
waitfor $P/curia.log "curia: printers: identified" 60; check "Identify" $? "$(g "grep 'identified' $P/curia.log | tail -1")"
sleep 3; check "the printer was asked to identify itself" "$([ "$(grep -c "Identify-Printer" "$work/a.log")" -gt "$before" ]; echo $?)" ""
press $(slot 0 1)                                   # Rename: the field opens under the card
press $((232 + 100)) $(($CARD0 + 96 + 16)); typ "Hall"; press $((232 + 16 + 248 + 20)) $(($CARD0 + 96 + 17))
waitfor $P/curia.log "renamed .* to Hall" 60; check "Rename" $? "$(g "grep 'renamed' $P/curia.log | tail -1")"
actl printers 2>&1 | grep -q "1  Hall"; check "prelumd has the new name" $? "$(actl printers 2>&1 | grep '^ *1  ')"
sleep 8
press $(slot 0 2)                                   # Share opens confirmation under the card
press $((232 + 16 + 30)) $(($CARD0 + 98 + 15))
waitfor $P/curia.log "Hall is shared" 60; check "Share" $? "$(g "grep 'shared' $P/curia.log | tail -1")"
actl printers 2>&1 | grep "1  Hall" | grep -q shared; check "prelumd has it shared" $? "$(actl printers 2>&1 | grep '^ *1  ')"
sleep 8
press $(slot 1 3)
n=$(actl printers 2>&1 | grep -c '^ *[0-9]  '); check "one press on Remove asks first" "$([ "$n" = 2 ]; echo $?)" "$n printers"
shot curia-remove-asks 1
press $(slot 1 3)
waitfor $P/curia.log "removed Bench" 60; check "Remove, pressed again" $? "$(g "grep 'removed' $P/curia.log | tail -1")"
n=$(actl printers 2>&1 | grep -c '^ *[0-9]  '); check "alice has one printer left" "$([ "$n" = 1 ]; echo $?)" "$n"
sleep 8; shot curia-done
g "pkill -x curia"
fi

if want status; then
STAGE=status
say "$STAGE: the status item and notices"
start_desk
g "cd $P && env FRESCO_SOCKET=$P/fresco.sock FRESCO_WM_SOCKET=$P/wm.sock FORUM_CTL_SOCKET=$P/ctl.sock FORUM_STATUS_SOCKET=$P/status.sock daemon -f -o $P/wm.log $P/bin/forum-wm </dev/null"
i=0; while [ $i -lt 30 ] && ! g "test -S $P/status.sock"; do sleep 2; i=$((i+1)); done
g "chmod 666 $P/status.sock $P/ctl.sock" || die "forum-wm: $(g "tail -5 $P/wm.log")"
fctl() { g "FORUM_CTL_SOCKET=$P/ctl.sock $P/bin/forum-ctl $*" 2>&1; }
g ": > $P/status.log; daemon -f -o $P/status.log -u printstatus env PRELUM_SOCKET=$P/prelum.sock PRAECO_SOCKET=$P/praeco.sock FORUM_STATUS_SOCKET=$P/status.sock PRELUM_OFFLINE_AFTER=30 $P/bin/prelum-status"
waitfor $P/status.log "prelum-status: up" 30; check "prelum-status up as alice's app" $? ""
id=$(actl printers 2>&1 | grep Bench | awk '{print $1}')
[ -n "$id" ] || { actl add ipp://$HOSTIP:8662/ipp/print >/dev/null 2>&1; id=$(actl printers 2>&1 | grep Bench | awk '{print $1}'); }
echo "stopped media-empty-error" > "$work/fstate"
actl print $P/min.pdf --printer $id --title Paper >/dev/null 2>&1
waitfor $P/status.log "posted .*out of paper" 90; check "out of paper: a notice posted to praecod" $? "$(g "grep 'out of paper' $P/status.log | tail -1")"
actl jobs 2>&1 | grep Paper | grep -q "alert OutOfPaper"; check "the job waits, alert out of paper" $? "$(actl jobs 2>&1 | grep Paper)"
waitfor $P/status.log "item: Printing" 30; check "the status item is up" $? "$(g "grep 'item:' $P/status.log | tail -1")"
out=$(fctl status); echo "$out" | grep -q "prelum-status"; check "forum-wm lists it" $? "$(echo "$out" | grep -i prelum | head -1)"
echo idle > "$work/fstate"
waitfor $P/status.log "withdrew" 120; check "paper back: the notice is withdrawn" $? "$(g "grep withdrew $P/status.log | tail -1")"
waitfor $P/status.log "posted .*(0): Printed" 120; check "\"Printed\" posted quietly (LOW)" $? "$(g "grep 'Printed' $P/status.log | tail -1")"
waitfor $P/status.log "item removed" 60; check "and the item goes" $? ""
# Offline: told only after it has lasted; Cancel from the status item.
kill $fakepid 2>/dev/null; sleep 1
actl print $P/min.pdf --printer $id --title Offline >/dev/null 2>&1
sleep 15
g "grep -q 'posted .*is offline' $P/status.log"; check "offline for 15 s: not told yet" "$([ $? = 1 ]; echo $?)" ""
waitfor $P/status.log "posted .*is offline" 90; check "offline past 30 s: told" $? "$(g "grep 'is offline' $P/status.log | tail -1")"
job=$(actl jobs 2>&1 | grep Offline | awk '{print $1}')
out=$(fctl status-press org.atrium.prelum-status $job);
waitfor $P/status.log "cancelled job $job" 60; check "Cancel in the status item cancels it" $? "$(echo "$out" | head -1)"
actl jobs 2>&1 | grep Offline | grep -q Cancelled; check "the job is cancelled" $? "$(actl jobs 2>&1 | grep Offline)"
waitfor $P/status.log "item removed" 60; check "the item goes again" $? ""
g "grep -q 'praecod:' $P/status.log"; check "praecod took every notice (an app of alice's)" "$([ $? = 1 ]; echo $?)" "$(g "grep 'praecod:' $P/status.log | head -1")"
fake_up
g "pkill -x prelum-status; pkill -x forum-wm"
fi

if want history; then
STAGE=history
say "$STAGE"
out=$(actl history 2>&1)
echo "$out" | grep -q "Done .*Paper"; check "history: Paper printed" $? "$(echo "$out" | grep Paper)"
echo "$out" | grep -q "Cancelled .*Offline"; check "history: Offline cancelled" $? "$(echo "$out" | grep Offline)"
g "ls $P/state/*/history.toml >/dev/null && ! grep -q '%PDF' $P/state/*/history.toml"; check "kept on disk, without the document" $? ""
start_desk
curia curia2
press 100 339
n=$(actl printers 2>&1 | grep -c '^ *[0-9]  ')
y=$(($CARD0 + n * $CARDH + 12 + 36 + 44 + 40 + 20 + 15))
shot history 4
press $((232 + 120 + 40)) $y
press $((232 + 120 + 40)) $y
waitfor $P/curia2.log "history cleared" 60; check "Clear history, pressed twice" $? "$(g "grep 'history' $P/curia2.log | tail -1")"
n=$(actl history 2>&1 | grep -c .); check "history is empty" "$([ "$n" = 0 ]; echo $?)" "$n lines"
shot history-cleared
fi

echo
if [ "$FAILS" = 0 ]; then echo "PASS: Prelum R5 in the VM"; else echo "FAILED: $FAILS"; exit 1; fi
