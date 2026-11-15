#!/bin/sh
# An IDE in a workspace (docs/spec/homes.md §5.3, W6) in the amd64 cloud VM
# with frescod headless: a stand-in IDE (ide_probe, `workspace-exec`) lists
# the workspaces, is refused a command in one it wasn't given and a pick
# with no press; a click's press picks `proj`, whose `work` appears at
# /work/proj, read-write, and the IDE builds and runs hello.c there as
# proj's uid (stdin and output on pipes, a cwd out of work refused, another
# workspace refused, a dropped command ended); it comes back at the next
# launch; deleting proj in the Workspaces app takes it out of the IDE.
# Runs on the HOST after cloud-vm.sh up; needs the VM's jaild. Ends
# "ide: N failed".
#
#   sh scripts/vm-ide-check.sh [out-dir]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/ide-shots}
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
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "ide: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe --example ide_probe"
xb stoa "-p stoa --bin stoad"
xb atrium-workspaces "--bin atrium-workspaces"

echo "=== install"
B=/var/tmp/idechk
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe atrium-term-socket/target/$TARGET/release/examples/ide_probe \
         stoa/target/$TARGET/release/stoad atrium-workspaces/target/$TARGET/release/atrium-workspaces; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "ide: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
STAY=org.test.stay; SYS=org.test.sys; LAUNCHER=org.test.launcher; TERM_ID=org.atrium.terminal; WS_ID=org.atrium.workspaces; IDE=org.test.ide
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
mk $IDE Ide ide_probe ide_probe 'graphics = "fresco"\nworkspace-exec = true' 'proj\n' targets
mkdir -p /atrium/sockets/portcullis /var/run/atrium; rm -f $PSOCK /var/run/atrium/stoad.sock
: > $LOG
daemon -f -o $B/stoad.log env STOA_CTL=/var/run/atrium/stoad.sock STOA_STATE=off $B/stoad
sleep 1; chmod 600 /var/run/atrium/stoad.sock
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd and the four apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "ide: $fails failed"; exit 1; }
V "su -m alice -c 'portcullis policy grant $STAY; portcullis policy grant $SYS; portcullis policy grant $IDE' >/dev/null 2>&1"
for a in $LAUNCHER $TERM_ID $WS_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "ide: $fails failed"; exit 1; }
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
for w in scratch web lib orig copy first viaapp proj other; do V "su -m alice -c 'portcullis workspace stop $w'" >/dev/null 2>&1; done
V "pkill -f atrium-workspaces; pkill -f atrium-term-socket; pkill -f ide_probe; sleep 2"
V "for m in \$(mount -p | awk '\$2 ~ /workspaces\/alice|jails\/org.test.ide/ {print \$2}' | sort -r); do umount -f \$m; done; rm -rf $W /var/lib/atrium/workspaces/alice /var/db/atrium/alice/workspace-picks /var/run/atrium/ide"
uidof() { V "awk '\$2 == \"alice\" && \$3 == \"workspace/$1\" {print \$1}' /var/run/atrium/app-registry | head -1"; }
ilog() { V "grep 'ide-probe: $1' $B/ide.log | tail -1"; }
waitlog() { # pattern [secs]
    i=0; while [ $i -lt ${2:-60} ] && ! V "grep -q 'ide-probe: $1' $B/ide.log"; do sleep 2; i=$((i+2)); done
}

echo "=== two workspaces"
V "su -m alice -c 'portcullis workspace list'" >/dev/null 2>&1
for w in proj other; do V "su -m alice -c 'portcullis workspace new $w --network none'" >/dev/null 2>&1; done
V 'sh -s' <<EOS
set -e
mkdir -p $W/proj/work/app
cat > $W/proj/work/app/hello.c <<'C'
#include <stdio.h>
#include <unistd.h>
int main(void) { printf("hi uid=%d\n", (int)getuid()); return 0; }
C
chown -R alice:files-alice $W/proj/work/app; chmod -R g+rwX $W/proj/work/app
EOS
check "proj and other made" "1" "$(V "test -f $W/proj/work/app/hello.c && test -d $W/other/work && echo 1")"

echo "=== the IDE"
V ": > $B/ide.log; daemon -f -o $B/ide.log su -m alice -c 'portcullis launch --no-prompt $IDE'"
waitlog "nopress"
L=$(ilog "list"); echo "$L" | grep -q "proj" && echo "$L" | grep -q "other" && ok "it lists the workspaces ($L)" || bad "list: $L"
L=$(ilog "unpicked"); echo "$L" | grep -q "wasn't given" && ok "a command in one it wasn't given is refused" || bad "unpicked: $L"
L=$(ilog "nopress"); echo "$L" | grep -q "err" && ok "a pick with no press is refused" || bad "nopress: $L"
check "nothing picked yet" "" "$(V "cat /var/db/atrium/alice/workspace-picks 2>/dev/null")"
shot ide-0
press 1100 500
L=$(ilog "pick proj")
if echo "$L" | grep -q "lost focus\|focus"; then press 1100 500; fi
waitlog "dropped sleep\|sleep ->" 120
L=$(ilog "pick proj"); check "a click's press picks proj" "ide-probe: pick proj -> ok" "$L"
check "remembered" "org.test.ide	proj" "$(V "cat /var/db/atrium/alice/workspace-picks")"
L=$(ilog "ls /work"); echo "$L" | grep -q 'Ok("proj")' && ok "/work holds proj alone ($L)" || bad "ls: $L"
L=$(ilog "build proj"); echo "$L" | grep -q "^ide-probe: build proj -> ok" && ok "cc builds in proj" || bad "build: $L"
PU=$(uidof proj)
L=$(ilog "run proj"); check "it runs as proj's uid" "ide-probe: run proj -> ok hi uid=$PU" "$L"
check "the build is proj's" "$PU" "$(V "stat -f %u $W/proj/work/app/hello")"
L=$(ilog "stdin proj"); check "stdin and output on pipes" "ide-probe: stdin proj -> ok through the pipe" "$L"
L=$(ilog "escape proj"); echo "$L" | grep -q "not a folder in work" && ok "a cwd out of work is refused" || bad "escape: $L"
L=$(ilog "write"); echo "$L" | grep -q 'Ok("ok")' && ok "the IDE writes in /work/proj" || bad "write: $L"
L=$(ilog "cat-from-ws"); check "and the workspace sees it" "ide-probe: cat-from-ws proj -> ok edited in the IDE" "$L"
L=$(ilog "other"); echo "$L" | grep -q "wasn't given" && ok "a workspace it wasn't given is refused" || bad "other: $L"
sleep 3
check "a dropped command ends" "" "$(V "ps -ax -o args= | grep '^sleep 77' ")"
shot ide-1

echo "=== at the next launch"
V "pkill -f ide_probe; sleep 4"
V ": > $B/ide.log; daemon -f -o $B/ide.log su -m alice -c 'portcullis launch --no-prompt $IDE'"
waitlog "nopress"
L=$(ilog "unpicked"); echo "$L" | grep -q "^ide-probe: unpicked proj -> ok hi\|^ide-probe: unpicked proj -> ok" && ok "proj is still its own ($L)" || bad "relaunch: $L"
check "proj's work is mounted at launch" "1" "$(V "mount -p | awk '\$2 == \"/var/lib/atrium/jails/$IDE/work/proj\"' | grep -c work")"

echo "=== deleting proj takes it out of the IDE"
V "daemon -f -o $B/ws.log su -m alice -c 'portcullis launch --no-prompt $WS_ID'"
i=0; while [ $i -lt 60 ] && ! V "grep -q 'is app $WS_ID' $LOG"; do sleep 2; i=$((i+2)); done
sleep 8
press 130 192            # proj (Scratch, other, proj)
shot ide-2
press 377 402            # Delete workspace… (under two uses rows)
shot ide-3
press 347 424            # Delete (a press)
i=0; while [ $i -lt 30 ] && V "test -d $W/proj"; do sleep 2; i=$((i+2)); done
shot ide-4
check "proj is gone" "" "$(V "ls -d $W/proj 2>/dev/null")"
check "out of the IDE's jail" "0" "$(V "mount -p | grep -c 'jails/$IDE/work/proj'")"
check "and its pick" "" "$(V "cat /var/db/atrium/alice/workspace-picks 2>/dev/null")"

echo "ide: $fails failed"
[ $fails = 0 ]
