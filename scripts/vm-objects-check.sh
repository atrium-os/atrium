#!/bin/sh
# Compiled objects shared within a group (docs/spec/homes.md §5.4, W9) in
# the amd64 cloud VM with frescod headless: liba, appb (uses liba) and
# stranger turn the object cache on (appb with the Workspaces app's
# checkbox), loner uses liba with it off. liba and appb share
# /cache/objects (CCACHE_DIR, SCCACHE_DIR point there); stranger has its
# own; loner has none; the folder is the files group's, 2770. When appb
# turns it off the group changes: liba starts again on a new, empty
# folder and the old one is removed. Runs on the HOST after cloud-vm.sh
# up; needs the VM's jaild. Ends "objects: N failed".
#
#   sh scripts/vm-objects-check.sh [out-dir]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/objects-shots}
mkdir -p "$OUT"
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "objects: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe"
xb stoa "-p stoa --bin stoad"
xb atrium-workspaces "--bin atrium-workspaces"

echo "=== install"
B=/var/tmp/objchk
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe \
         stoa/target/$TARGET/release/stoad atrium-workspaces/target/$TARGET/release/atrium-workspaces; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "objects: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
STAY=org.test.stay; SYS=org.test.sys; LAUNCHER=org.test.launcher; TERM_ID=org.atrium.terminal; WS_ID=org.atrium.workspaces
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
mk() { # id name binary-in-B entry-name extra-capabilities
  opifex uninstall \$1 >/dev/null 2>&1 || true
  rm -rf /home/alice/.atrium/apps/\$1
  O=$B/b-\$1; rm -rf \$O; mkdir -p \$O/bin; cp $B/\$3 \$O/bin/\$4
  printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/%s"\n' \$1 "\$2" \$4 > \$O/atrium.toml
  [ -n "\$5" ] && printf '\n[capabilities]\n%b\n' "\$5" >> \$O/atrium.toml
  [ -n "\${6:-}" ] && printf '%b' "\$6" > \$O/\$7
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
}
mk $STAY Stay home_probe stay_probe ''
mk $SYS Sys home_probe stay_probe 'status-item = true'
mk $LAUNCHER Launcher console_probe console_probe 'graphics = "fresco"\napp-launch = true' 'workspace/scratch\nworkspace/liba\nworkspace/appb\nworkspace/scratch\n' targets
mk $TERM_ID Terminal atrium-term-socket atrium-term-socket 'graphics = "fresco"\nconsoles = true' 'on\n' .trace
mk $WS_ID Workspaces atrium-workspaces atrium-workspaces 'graphics = "fresco"\nworkspaces = true'
mkdir -p /atrium/sockets/portcullis /var/run/atrium; rm -f $PSOCK /var/run/atrium/stoad.sock
: > $LOG
daemon -f -o $B/stoad.log env STOA_CTL=/var/run/atrium/stoad.sock STOA_STATE=off $B/stoad
sleep 1; chmod 600 /var/run/atrium/stoad.sock
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd and the four apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "objects: $fails failed"; exit 1; }
V "su -m alice -c 'portcullis policy grant $STAY; portcullis policy grant $SYS' >/dev/null 2>&1"
for a in $LAUNCHER $TERM_ID $WS_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "objects: $fails failed"; exit 1; }
inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $B/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-4}; }
shot() { sleep 3; get $B/screen.png "$OUT/$1.png" 2>/dev/null || get $B/screen "$OUT/$1.png" 2>/dev/null; [ -s "$OUT/$1.png" ] && echo "  screenshot $1.png" || echo "  (no screenshot for $1)"; }
# Type text into the focused window (the terminal tracks Shift by key).
typ() {
    python3 -I - "$1" > /tmp/workspace-keys.txt <<'PY'
import sys
lo = {c: 0x04 + i for i, c in enumerate("abcdefghijklmnopqrstuvwxyz")}
lo.update({c: 0x1e + i for i, c in enumerate("1234567890")})
lo.update({" ": 0x2c, "-": 0x2d, "=": 0x2e, "/": 0x38, ".": 0x37, ";": 0x33, "'": 0x34, ",": 0x36, "\n": 0x28})
hi = {">": 0x37, "<": 0x36, "_": 0x2d, "|": 0x31, ":": 0x33, '"': 0x34, "&": 0x24, "(": 0x26, ")": 0x27, "*": 0x25}
hi.update({c.upper(): 0x04 + i for i, c in enumerate("abcdefghijklmnopqrstuvwxyz")})
for c in sys.argv[1]:
    if c in lo:
        print(f"KEY {lo[c]:#x} 1"); print(f"KEY {lo[c]:#x} 0")
    elif c in hi:
        print("KEY 0xe1 1 2"); print(f"KEY {hi[c]:#x} 1 2"); print(f"KEY {hi[c]:#x} 0 2"); print("KEY 0xe1 0 0")
PY
    scp -q $K -P 2222 /tmp/workspace-keys.txt root@localhost:$B/keys.txt
    V "nc -N -U $B/inject.sock < $B/keys.txt" >/dev/null
    sleep ${GAP:-4}
}
screen() { V "grep 'atrium-term: screen' $LOG | tail -1"; }
waitscreen() { # pattern [secs]
    i=0; while [ $i -lt ${2:-30} ] && ! screen | grep -q -- "$1"; do sleep 2; i=$((i+2)); done
    screen | grep -q -- "$1"
}

echo "=== jaild"
V "service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1; sleep 1; test -S /var/run/atrium/jaild.sock" && ok "jaild is up" || bad "no jaild"
W=/home/alice/.atrium/workspaces
for w in scratch web lib orig copy first viaapp proj other liba appb stranger loner; do V "su -m alice -c 'portcullis workspace stop $w'" >/dev/null 2>&1; done
V "pkill -f atrium-workspaces; pkill -f atrium-term-socket; pkill -f ide_probe; sleep 2"
V "for m in \$(mount -p | awk '\$2 ~ /workspaces\/alice/ {print \$2}' | sort -r); do umount -f \$m; done; rm -rf $W /home/alice/.atrium/cache /var/lib/atrium/workspaces/alice /var/db/atrium/alice/workspace-picks"
X() { V "su -m alice -c \"portcullis workspace exec $1 -- sh -c '$2'\"" 2>&1; }
OBJ=/home/alice/.atrium/cache/objects

echo "=== four workspaces"
V "su -m alice -c 'portcullis workspace list'" >/dev/null 2>&1
V "su -m alice -c 'portcullis workspace new liba --network none; portcullis workspace new appb --network none --uses liba; portcullis workspace new stranger --network none; portcullis workspace new loner --network none --uses liba'" >/dev/null 2>&1
V "su -m alice -c 'portcullis workspace set liba --object-cache on; portcullis workspace set stranger --object-cache on'" >/dev/null 2>&1
check "liba and stranger have it on, appb not yet" "liba objects=on stranger objects=on appb objects=off loner objects=off" \
  "$(V "su -m alice -c 'portcullis workspace list'" | awk '{print $1, $7}' | grep -E '^(liba|stranger|appb|loner) ' | sort -k1,1 | awk '{a[$1]=$2} END {print "liba", a["liba"], "stranger", a["stranger"], "appb", a["appb"], "loner", a["loner"]}')"

echo "=== appb turns it on in the app"
V "daemon -f -o $B/ws.log su -m alice -c 'portcullis launch --no-prompt $WS_ID'"
i=0; while [ $i -lt 60 ] && ! V "grep -q 'is app $WS_ID' $LOG"; do sleep 2; i=$((i+2)); done
sleep 8
press 130 140            # appb (Scratch, appb, liba, loner, stranger)
shot obj-0
press 420 245            # Share compiled objects with them
sleep 3
shot obj-1
check "the checkbox turns appb's on" "objects=on" "$(V "su -m alice -c 'portcullis workspace list'" | awk '$1 == "appb" {print $7}')"
V "pkill -f atrium-workspaces"

echo "=== who shares what"
L=$(X liba 'echo \$CCACHE_DIR \$SCCACHE_DIR; echo from-liba > /cache/objects/x && echo wrote')
check "liba's tools point at /cache/objects" "/cache/objects/ccache /cache/objects/sccache wrote" "$(echo $L)"
L=$(X appb 'cat /cache/objects/x; echo from-appb > /cache/objects/y && echo wrote')
check "appb sees liba's objects and adds its own" "from-liba wrote" "$(echo $L)"
check "liba sees appb's" "from-appb" "$(X liba 'cat /cache/objects/y')"
L=$(X stranger 'cat /cache/objects/x 2>&1; echo \$CCACHE_DIR')
echo "$L" | grep -q "No such file" && echo "$L" | grep -q "/cache/objects/ccache" && ok "stranger has its own, without theirs" || bad "stranger: $L"
L=$(X loner 'ls /cache/objects 2>&1; echo [\$CCACHE_DIR]')
echo "$L" | grep -q "No such file" && echo "$L" | grep -q "\[\]" && ok "loner (off) has none" || bad "loner: $L"
check "the group's folder: root's, the files group's, 2770" "root files-alice drwxrws---" "$(V "stat -f '%Su %Sg %Sp' $OBJ/appb+liba")"
check "two group folders" "appb+liba stranger" "$(V "ls $OBJ | tr '\n' ' ' | sed 's/ \$//'")"

echo "=== a group that changes starts empty"
V "su -m alice -c 'portcullis workspace set appb --object-cache off'" >/dev/null 2>&1
check "liba was stopped for it" "stopped" "$(V "su -m alice -c 'portcullis workspace list'" | awk '$1 == "liba" {print $6}')"
check "the old group's folder is gone (liba's comes at its start)" "stranger" "$(V "ls $OBJ | tr '\n' ' ' | sed 's/ \$//'")"
L=$(X liba 'ls -A /cache/objects | wc -l')
check "liba starts again on an empty one" "0" "$(echo $L)"
L=$(X appb 'ls -A /cache/objects 2>/dev/null | wc -l; echo [\$CCACHE_DIR]')
check "appb has none now (nothing at /cache/objects, no CCACHE_DIR)" "0 []" "$(echo $L)"
check "and liba's new folder" "liba stranger" "$(V "ls $OBJ | tr '\n' ' ' | sed 's/ \$//'")"

echo "objects: $fails failed"
[ $fails = 0 ]
