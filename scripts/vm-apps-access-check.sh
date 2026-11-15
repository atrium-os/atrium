#!/bin/sh
# Curia's Apps page (docs/spec/curia.md §13) in the cloud VM: portcullisd's
# Access, ApproveAccess and RemoveAccess, with frescod headless and real
# presses. Runs on the HOST after `sh scripts/cloud-vm.sh up`. Ends
# "apps-access: N failed".
#
#   sh scripts/vm-apps-access-check.sh [out-dir]
#
# Apps (all signed by the gate's publisher key, installed with opifex):
#   org.test.new        asks for the clipboard and notifications: alice's
#                       to allow
#   org.test.sys        asks for audio-control (restricted): the system's
#                       grant only
#   org.atrium.curia    Settings, trimmed to graphics + app-grants, granted
#                       by the system, launched for alice
#
#   - a caller that is not a launched app (root over nc): refused;
#   - Curia lists the three for alice;
#   - Allow on org.test.new: portcullisd spends the press with frescod and
#     writes alice's grant;
#   - a forged press (root's own token) can't allow anything;
#   - org.test.sys has no Allow; nothing is written for it;
#   - Remove access, then Remove: the grant is gone.
# WX/WY: where frescod puts Curia's window (0 0 with no window manager).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/apps-access}
mkdir -p "$OUT"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
WX=${WX:-0}; WY=${WY:-0}

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "apps-access: 1 failed"; exit 1; }; }
[ -n "${NO_BUILD:-}" ] || {
    xb frescod "--bin frescod"
    xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
    xb curia "--bin curia"
}

echo "=== install"
B=/var/tmp/apps-access
V "pkill -x portcullisd; pkill -x frescod; pkill -x curia; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe curia/target/$TARGET/release/curia; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "apps-access: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
POL=/var/db/atrium/alice/policy.toml
NEW=org.test.new; SYS=org.test.sys; CURIA=org.atrium.curia
V 'sh -s' <<EOS
set -e
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
mk() { # id name binary-in-B entry-name capabilities
  opifex uninstall \$1 >/dev/null 2>&1 || true
  O=$B/b-\$1; rm -rf \$O; mkdir -p \$O/bin; cp $B/\$3 \$O/bin/\$4
  printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/%s"\n\n[capabilities]\n%b\n' \$1 "\$2" \$4 "\$5" > \$O/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
}
mk $NEW New home_probe stay_probe 'clipboard = true\nnotify = true'
mk $SYS Sys home_probe stay_probe 'audio-control = true'
mk $CURIA Settings curia curia 'graphics = "fresco"\napp-grants = true'
rm -f $POL
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
portcullis policy grant --system $SYS >/dev/null 2>&1
portcullis policy grant --system $CURIA >/dev/null 2>&1
EOS
[ $? = 0 ] && ok "portcullisd and the three apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "apps-access: $fails failed"; exit 1; }

echo "=== not a launched app"
R=$(V "printf '{\"op\":\"hello\",\"version\":1}\n{\"op\":\"access\"}\n{\"op\":\"approve_access\",\"app_id\":\"$NEW\",\"activation\":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]}\n' | nc -N -U $PSOCK")
case "$R" in *"not a launched app"*"not a launched app"*) ok "root over the socket: refused, both" ;; *) bad "root: $R" ;; esac

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "apps-access: $fails failed"; exit 1; }
inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $B/inject.sock" >/dev/null; }
press() { inj "MOVE $(($1 + WX)) $(($2 + WY))" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-6}; }
shot() { sleep 4; get $B/screen.png "$OUT/$1.png" 2>/dev/null || get $B/screen "$OUT/$1.png" 2>/dev/null; [ -s "$OUT/$1.png" ] && echo "  screenshot $OUT/$1.png" || echo "  (no screenshot for $1)"; }
count() { V "grep -c '$1' $LOG" | tr -d ' '; }

echo "=== Settings, the Apps page"
V "daemon -f -o $B/curia.log su -m alice -c 'portcullis launch --no-prompt $CURIA'"
i=0; while [ $i -lt 60 ] && ! V "grep -q 'launching $CURIA as uid' $LOG"; do sleep 2; i=$((i+2)); done
sleep 20
shot apps-0
press 70 225
i=0; while [ $i -lt 40 ] && [ "$(count "access -> $CURIA")" = 0 ]; do sleep 2; i=$((i+2)); done
L=$(V "grep 'access -> $CURIA' $LOG | tail -1")
case "$L" in *"for alice: "*" app(s)"*) ok "Curia lists the apps for alice ($(echo "$L" | sed 's/.*: //'))" ;; *) bad "no list: $L / $(V "tail -5 $LOG")" ;; esac
shot apps-1

echo "=== Allow"
# The first row (New): Allow is its rightmost button.
press 906 124
i=0; while [ $i -lt 30 ] && [ "$(count "alice allowed $NEW")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "alice allowed $NEW (from $CURIA)")" = 1 ] && ok "a click on Allow: alice's grant" || bad "no grant: $(V "grep -i 'access\|REFUSED' $LOG | tail -3")"
V "grep -q 'org.test.new' $POL" && ok "written to her policy" || bad "policy: $(V "cat $POL")"
shot apps-2
V "daemon -f -o $B/new.log su -m alice -c 'portcullis launch --no-prompt $NEW'"
i=0; while [ $i -lt 30 ] && ! V "grep -q 'launching $NEW as uid' $LOG"; do sleep 2; i=$((i+2)); done
V "grep -q 'launching $NEW as uid' $LOG" && ok "New now launches without asking" || bad "launch: $(V "tail -3 $LOG")"
V "pkill -f stay_probe" >/dev/null 2>&1

echo "=== the system's app"
# The third row (Sys): no Allow; a press there allows nothing.
press 906 244
[ "$(count "allowed $SYS")" = 0 ] && ok "Sys has no Allow" || bad "Sys was allowed"
V "grep -q 'org.test.sys' $POL" && bad "Sys in alice's policy" || ok "nothing written for Sys"
shot apps-3

echo "=== Remove"
press 906 124
shot apps-4
press 800 171
i=0; while [ $i -lt 30 ] && [ "$(count "alice removed $NEW")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "alice removed $NEW")" = 1 ] && ok "Remove access, then Remove: gone" || bad "not removed: $(V "grep -i 'access\|REFUSED' $LOG | tail -3")"
V "grep -q 'org.test.new' $POL" && bad "still in her policy" || ok "not in her policy"
shot apps-5

V "pkill -x curia" >/dev/null 2>&1
echo "apps-access: $fails failed"
