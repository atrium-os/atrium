#!/bin/sh
# Share sheet gate (docs/spec/share-sheet.md §6): an app shares a note and a
# picture through the system's share sheet to an app the person picks, on a
# live headless frescod in the cloud amd64 VM, with real (injected) clicks.
# Runs on the HOST after the VM is up:
#
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX \
#     sh scripts/cloud-vm.sh up
#   sh scripts/vm-share-check.sh [out-dir]
#
# (frescod's compute renderer is x86-64-v3: on the default Nehalem CPU it
# dies of SIGILL. The atrium-os/qemu fork boots a Haswell guest.)
#
# limend is limend-dev (providers started unconfined, as the Navigator's
# live gate does); frescod spends presses with its real activation table and
# lets only _share (65045) spend SHARE.
#
# Checks:
#   - an app that opens the sheet without a press gets Failed;
#   - a click on Share: the sheet (as _share) spends it, lists the two
#     installed targets that take a picture or text (not Maps), and lands in
#     a sheet slot over the app;
#   - a press beside the sheet doesn't reach the app;
#   - a click on Scraps: the app hears Chosen, limend starts Scraps, which
#     gets the title, the text inline and the picture through a read-only
#     descriptor; a click on Keep: the app hears Accepted;
#   - a second share, Cancel: the app hears Cancelled and nothing starts.
# Screenshots land in out-dir. Ends "share: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/share}
WAIT=${WAIT:-60}
GAP=${GAP:-12}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { ssh $K -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp -q $K -P 2222 "$1" "root@localhost:$2"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
mkdir -p "$OUT"
V true >/dev/null 2>&1 || { echo "the VM does not answer over ssh"; echo "share: 1 failed"; exit 1; }

if [ "${BUILD:-1}" = 1 ]; then
    echo "== cross-build"
    xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "share: 1 failed"; exit 1; }; }
    xb frescod "--bin frescod"
    xb limen "--example limend-dev"
    xb share-sheet "--bins"
fi

D=/opt/share-check
S=/tmp/sh
R="$TARGET/release"
V "pkill -x frescod; pkill -x limend-dev; pkill -f share-; sleep 1; rm -rf $S; mkdir -p $D $S $S/apps; chmod 777 $S; true"
for b in "$ROOT/frescod/target/$R/frescod" "$ROOT/limen/target/$R/examples/limend-dev" \
         "$ROOT/share-sheet/target/$R/share-sheet" "$ROOT/share-sheet/target/$R/share-demo" "$ROOT/share-sheet/target/$R/share-target-demo"; do
    h=$(sha256sum "$b" | cut -d' ' -f1)
    [ "$h" = "$(V "sha256 -q $D/$(basename "$b") 2>/dev/null || true")" ] && continue
    put "$b" "$D/"
    # TCG scp sometimes lands a run of zeros: check every binary.
    [ "$h" = "$(V "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: $(basename "$b") arrived damaged"; exit 1; }
done
V "chmod 755 $D/*; mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "== users, registry, manifests"
V 'sh -s' <<EOS
for u in "_share 65045" "shareapp 50911" "sharetgt 50912"; do
  set -- \$u
  pw usershow \$1 >/dev/null 2>&1 || pw useradd \$1 -u \$2 -d /nonexistent -s /usr/sbin/nologin
done
printf '50911 alice org.test.notes\n' > $S/registry
mk() { mkdir -p $S/apps/\$1; printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/x"\n%b' "\$1" "\$2" "\$3" > $S/apps/\$1/atrium.toml; }
mk org.test.scraps Scraps '\n[roles]\nimplements = ["share-target"]\n\n[roles.share-target]\nmime = ["text/*", "image/*"]\n'
mk org.test.photos Photos '\n[roles]\nimplements = ["share-target"]\n\n[roles.share-target]\nmime = ["image/*"]\n'
mk org.test.maps Maps '\n[roles]\nimplements = ["map"]\n'
mk org.test.notes Notes '\n[roles]\nimplements = ["share-target"]\n'
printf '\211PNG\r\n\032\n-a-picture-' > $S/lake.png; chmod 644 $S/lake.png
chmod -R a+rX $S/apps $S/registry
EOS
[ $? = 0 ] && ok "users _share, shareapp, sharetgt; the registry; four manifests" || bad "setup"

echo "== frescod, limend, the share sheet"
V "cd $S && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock FRESCOD_WM_SOCK=$S/wm.sock \
   FRESCOD_INPUT_SOCK=$S/inject.sock FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=15000 \
   FRESCOD_ACTIVATION_SOCK=$S/activation.sock FRESCOD_ACTIVATION_UIDS=65045=share FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   daemon -f -o $S/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S $S/fresco.sock && test -S $S/activation.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $S/fresco.sock $S/activation.sock"
sleep 5
V "pgrep -x frescod >/dev/null && test -S $S/fresco.sock" && ok "frescod up" || { bad "frescod: $(V "tail -3 $S/frescod.log | tr '\n' ';'; dmesg | grep frescod | tail -1")"; echo "share: $fails failed"; exit $fails; }
V "cd $S && env LIMEN_SOCKET=$S/limend.sock LIMEN_FRESCOD_ACTIVATION=$S/activation.sock LIMEN_DEV_UID=50912 LIMEN_DEV_SHARE_UID=65045 \
   LIMEN_DEV_APP=org.test.notes LIMEN_DEV_APPS=org.test.scraps=$D/share-target-demo,org.test.photos=$D/share-target-demo \
   FRESCO_SOCKET=$S/fresco.sock daemon -f -o $S/limend.log $D/limend-dev </dev/null"
i=0; while [ $i -lt 20 ] && ! V "test -S $S/limend.sock"; do sleep 1; i=$((i+1)); done
V "test -S $S/limend.sock" && ok "limend-dev up" || bad "limend: $(V "tail -5 $S/limend.log | tr '\n' ';'")"
V "cd $S && daemon -f -o $S/sheet.log -u _share env FRESCO_SOCKET=$S/fresco.sock LIMEN_SOCKET=$S/limend.sock \
   SHARE_FRESCOD_ACTIVATION=$S/activation.sock SHARE_APP_REGISTRY=$S/registry SHARE_APPS=$S/apps $D/share-sheet </dev/null"
i=0; while [ $i -lt 30 ] && ! V "grep -q 'providing share' $S/sheet.log"; do sleep 1; i=$((i+1)); done
V "grep -q 'providing share' $S/sheet.log" && ok "the share sheet provides share, as _share" || bad "sheet: $(V "tail -5 $S/sheet.log $S/limend.log | tr '\n' ';'")"

inj() { V "printf '%s\n' '$1' | nc -N -U $S/inject.sock"; }
press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep $GAP; }
shot() { sleep ${1:-6}; get $S/screen.png "$OUT/$2.png" 2>/dev/null || get "$S/screen" "$OUT/$2.png" 2>/dev/null; [ -s "$OUT/$2.png" ] && echo "  screenshot $2.png" || echo "  (no screenshot for $2)"; }
demo() { # log extra-env
    V "cd $S && daemon -f -o $S/$1.log -u shareapp env FRESCO_SOCKET=$S/fresco.sock LIMEN_SOCKET=$S/limend.sock $2 $D/share-demo </dev/null"
}

echo "== no press, no sheet"
demo unasked "SHARE_UNASKED=1"
i=0; while [ $i -lt $WAIT ] && ! V "grep -q 'sheet failed' $S/unasked.log"; do sleep 2; i=$((i+2)); done
V "grep -q 'share-demo: sheet failed no fresh press' $S/unasked.log" && ok "a sheet without a press: $(V "grep 'sheet failed' $S/unasked.log")" || bad "unasked: $(V "tail -3 $S/unasked.log $S/sheet.log | tr '\n' ';'")"
V "pkill -f share-demo"; sleep 3

echo "== share a note and a picture"
demo demo "SHARE_FILE=$S/lake.png"
sleep $WAIT
shot 2 share-app
# The app's window is at 0,0, 760 x 540; Share is at its top right.
press 688 28
V "grep -q 'asked for the share sheet' $S/demo.log" && ok "a click on Share asks for the sheet" || bad "share click: $(V "tail -3 $S/demo.log | tr '\n' ';'")"
V "grep -q '2 types, 2 targets for alice' $S/sheet.log" && ok "the sheet spent the press and found Photos and Scraps (not Maps, not Notes itself)" || bad "targets: $(V "tail -4 $S/sheet.log | tr '\n' ';'")"
sleep 10
shot 2 share-sheet
# frescod puts the 480 x 400 sheet at 140,48: rows from y 80, 48 each,
# sorted by app id (Photos, Scraps).
press 60 300
V "grep -q 'chosen\|cancelled' $S/demo.log" && bad "a press beside the sheet did something: $(V "tail -2 $S/demo.log")" || ok "a press beside the sheet did nothing"
press 340 196
V "grep -q 'share-demo: chosen' $S/demo.log" && ok "a click on Scraps: the app hears Chosen" || bad "choose: $(V "tail -3 $S/demo.log $S/sheet.log $S/limend.log | tr '\n' ';'")"
V "grep -q 'chose org.test.scraps' $S/sheet.log" && ok "the sheet chose Scraps" || bad "sheet choice: $(V "tail -2 $S/sheet.log")"
sleep $WAIT
V "grep -q 'started org.test.scraps' $S/limend.log" && ok "limend started Scraps" || bad "start: $(V "tail -3 $S/limend.log | tr '\n' ';'")"
V "grep -q 'share-target-demo: got \"Groceries: rice, lentils, coriander\" 2 formats image/png=19 text/plain=35' $S/limend.log" \
  && ok "Scraps got the title, the picture by descriptor (19 bytes) and the text" || bad "item: $(V "grep share-target-demo $S/limend.log | tr '\n' ';'")"
V "grep -q 'share-target-demo: text Groceries' $S/limend.log" && ok "and shows the text" || bad "text: $(V "grep 'share-target-demo: text' $S/limend.log")"
shot 2 share-target
# The 560 x 440 target sheet at 100,48: Keep at its bottom right.
press 588 452
V "grep -q 'share-demo: accepted' $S/demo.log" && ok "a click on Keep: the app hears Accepted" || bad "keep: $(V "tail -3 $S/demo.log; grep share-target-demo $S/limend.log | tail -2" | tr '\n' ';')"
shot 4 share-done

echo "== share again, Cancel"
n=$(V "grep -c 'started org.test' $S/limend.log")
press 688 28
sleep 10
# Cancel at the 480 x 400 sheet's bottom right (140,48 + 408,364).
press 548 412
V "grep -q 'share-demo: cancelled' $S/demo.log" && ok "Cancel: the app hears Cancelled" || bad "cancel: $(V "tail -3 $S/demo.log $S/sheet.log | tr '\n' ';'")"
[ "$(V "grep -c 'started org.test' $S/limend.log")" = "$n" ] && ok "and nothing was started" || bad "a target started after a cancel"
shot 4 share-cancelled

echo "== clean up"
V "pkill -f share-; pkill -x limend-dev; pkill -x frescod; true"
for f in frescod limend sheet unasked demo; do get $S/$f.log "$OUT/$f.log" 2>/dev/null; done
echo "share: $fails failed"
exit $fails
