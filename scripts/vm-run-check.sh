#!/bin/sh
# Run (docs/spec/homes.md §5.3, W5) in the amd64 cloud VM with frescod
# headless: a program built with cc in Scratch runs as its own dev app
# (dev.<person>.<workspace>.<id>), signed by the machine's dev publisher,
# with its own uid and none of the workspace's files; a manifest asking for
# a system-only capability is refused; a graphical build Run from the
# Workspaces app's button gets frescod's amber "dev build" frame; deleting
# the workspace takes its dev builds away. Runs on the HOST after
# cloud-vm.sh up; needs the VM's jaild. Ends "run: N failed".
#
#   sh scripts/vm-run-check.sh [out-dir]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/run-shots}
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
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "run: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe"
xb stoa "-p stoa --bin stoad"
xb atrium-workspaces "--bin atrium-workspaces"

echo "=== install"
B=/var/tmp/runchk
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe \
         stoa/target/$TARGET/release/stoad atrium-workspaces/target/$TARGET/release/atrium-workspaces; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "run: 1 failed"; exit 1; }
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
[ $? = 0 ] && ok "portcullisd and the four apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "run: $fails failed"; exit 1; }
V "su -m alice -c 'portcullis policy grant $STAY; portcullis policy grant $SYS' >/dev/null 2>&1"
for a in $LAUNCHER $TERM_ID $WS_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "run: $fails failed"; exit 1; }
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
for w in scratch web lib; do V "su -m alice -c 'portcullis workspace stop $w'" >/dev/null 2>&1; done
V "for m in \$(mount -p | awk '\$2 ~ /workspaces\/alice/ {print \$2}' | sort -r); do umount -f \$m; done; rm -rf $W /var/lib/atrium/workspaces/alice"

echo "=== a build in Scratch"
V "pkill -f atrium-workspaces; pkill -f atrium-term-socket; rm -rf /var/lib/atrium/apps/dev.alice.* /var/db/atrium/dev-publisher; sleep 1"
V "su -m alice -c 'portcullis workspace list'" >/dev/null 2>&1
WSU=$(V "awk '\$2 == \"alice\" && \$3 == \"workspace/scratch\" {print \$1}' /var/run/atrium/app-registry | head -1")
# What a build in Scratch leaves in work: a program (cc, static: an app's
# jail has no libc) and its manifest; one asking for too much; a window.
V 'sh -s' <<EOS
set -e
P=$W/scratch/work/hello; mkdir -p \$P
cat > \$P/hello.c <<'C'
#include <stdio.h>
#include <unistd.h>
#include <sys/stat.h>
int main(void) {
    struct stat st;
    printf("dev-hello uid=%d work=%s tools=%s\n", (int)getuid(),
           stat("/home", &st) == 0 ? "seen" : "none", stat("/usr/bin/cc", &st) == 0 ? "cc" : "nocc");
    return 0;
}
C
cc -static -o \$P/hello \$P/hello.c
printf '[app]\nid = "org.example.hello"\nname = "Hello"\nversion = "1"\nentry = "hello"\n' > \$P/atrium.toml
mkdir -p $W/scratch/work/greedy
cp \$P/hello $W/scratch/work/greedy/
printf '[app]\nid = "org.example.greedy"\nname = "Greedy"\nversion = "1"\nentry = "hello"\n\n[capabilities]\nworkspaces = true\n' > $W/scratch/work/greedy/atrium.toml
mkdir -p $W/scratch/work/gui
cp $B/atrium-workspaces $W/scratch/work/gui/gui
printf '[app]\nid = "org.example.gui"\nname = "Gui"\nversion = "1"\nentry = "gui"\n\n[capabilities]\ngraphics = "fresco"\n' > $W/scratch/work/gui/atrium.toml
chown -R alice:files-alice $W/scratch/work
chmod -R g+rwX $W/scratch/work
EOS
[ $? = 0 ] && ok "a build in Scratch's work" || bad "the build"

echo "=== Run from the CLI"
out=$(V "su -m alice -c 'portcullis workspace run scratch hello'" 2>&1)
echo "$out" | grep -q "^dev.alice.scratch.org.example.hello" && ok "Run installs dev.alice.scratch.org.example.hello" || bad "run: $out"
i=0; while [ $i -lt 30 ] && ! V "grep -q dev-hello $LOG"; do sleep 2; i=$((i+2)); done
H=$(V "grep dev-hello $LOG | tail -1")
echo "$H" | grep -q "work=none" && ok "it sees no home ($H)" || bad "home: $H"
echo "$H" | grep -q "tools=nocc" && ok "nor the workspace's tools" || bad "tools: $H"
DU=$(echo "$H" | sed -n 's/.*uid=\([0-9]*\).*/\1/p')
[ -n "$DU" ] && [ "$DU" != "$WSU" ] && [ "$DU" != "$(V "id -u alice")" ] && ok "its own uid ($DU), not the workspace's ($WSU) or alice's" || bad "uid $DU (workspace $WSU)"
check "signed by the dev publisher, its key root's" "1" "$(V "test -s /var/lib/atrium/apps/dev.alice.scratch.org.example.hello/atrium.toml.sig && test -s /var/db/atrium/dev-publisher/public.pem && test \$(stat -f %Lp /var/db/atrium/dev-publisher/private.pem) = 600 && echo 1")"
check "the dev key is no publisher" "" "$(V "grep -l \"\$(sed -n 2p /var/db/atrium/dev-publisher/public.pem)\" /etc/atrium/publishers/*.pem 2>/dev/null")"
out=$(V "su -m alice -c 'portcullis workspace run scratch greedy'" 2>&1)
echo "$out" | grep -q "can't have workspaces" && ok "a build asking for a system-only capability is refused" || bad "greedy: $out"
check "and nothing is installed for it" "" "$(V "ls -d /var/lib/atrium/apps/dev.alice.scratch.org.example.greedy 2>/dev/null")"
out=$(V "su -m alice -c 'portcullis workspace run scratch ../../..'" 2>&1)
echo "$out" | grep -q "not a path inside" && ok "a path out of work is refused" || bad "escape: $out"
V "ln -sf /etc $W/scratch/work/out; chown -h alice $W/scratch/work/out"
out=$(V "su -m alice -c 'portcullis workspace run scratch out'" 2>&1)
echo "$out" | grep -q "Capabilities insufficient" && ok "a link out of work is refused ($out)" || bad "link: $out"
V "rm -f $W/scratch/work/out"
# A dev id signed by a publisher, not the dev key.
V 'sh -s' <<EOS
O=/var/lib/atrium/apps/dev.alice.scratch.org.example.forged; rm -rf \$O; mkdir -p \$O
cp $W/scratch/work/hello/hello \$O/hello
printf '[app]\nid = "dev.alice.scratch.org.example.forged"\nname = "Forged"\nversion = "1"\nentry = "hello"\n' > \$O/atrium.toml
openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
EOS
out=$(V "su -m alice -c 'portcullis launch --no-prompt dev.alice.scratch.org.example.forged'" 2>&1; V "grep forged $LOG | tail -2")
echo "$out" | grep -qi "dev publisher" && ok "a dev id signed by a publisher doesn't launch" || bad "forged: $out"
check "and never ran" "" "$(V "grep -c 'dev-hello' $LOG | grep -v '^1\$'")"
V "rm -rf /var/lib/atrium/apps/dev.alice.scratch.org.example.forged"

echo "=== Run from the app"
V "daemon -f -o $B/ws.log su -m alice -c 'portcullis launch --no-prompt $WS_ID'"
i=0; while [ $i -lt 60 ] && ! V "grep -q 'is app $WS_ID' $LOG"; do sleep 2; i=$((i+2)); done
sleep 8
press 130 88            # Scratch
shot run-0
press 400 454           # the project field
typ "gui"
press 609 454           # Run (a press: purpose APPROVE)
i=0; while [ $i -lt 60 ] && ! V "jls name | grep -q example_gui"; do sleep 2; i=$((i+2)); done
V "jls name" | grep -q "example_gui" && ok "Run on the app's button starts it" || bad "gui: $(V "jls name | tr '\n' ' '") $(V "grep -i 'run' $LOG | tail -3")"
sleep 10
shot run-1
# frescod's amber frame (ISLAND_COLOR) along the dev window's bottom edge.
amber=$(python3 -I -c "
from PIL import Image
im = Image.open('$OUT/run-1.png').convert('RGB')
w, h = im.size
hits = sum(1 for x in range(0, min(w, 900), 7) for y in range(590, min(h, 640))
           if (lambda p: p[0] > 230 and 180 < p[1] < 230 and 60 < p[2] < 130)(im.getpixel((x, y))))
print('yes' if hits > 20 else 'no:%d' % hits)
" 2>&1)
check "frescod frames it in amber as a dev build" "yes" "$amber"

echo "=== deleting the workspace takes its builds"
V "for j in \$(jls name | grep example_gui); do jail -r \$j; done" >/dev/null 2>&1
sleep 3
V "su -m alice -c 'portcullis workspace new doomed --network none'" >/dev/null
V "mkdir -p $W/doomed/work/h && cp $W/scratch/work/hello/hello $W/scratch/work/hello/atrium.toml $W/doomed/work/h/ && chown -R alice:files-alice $W/doomed/work/h"
V "su -m alice -c 'portcullis workspace run doomed h'" >/dev/null 2>&1
check "doomed's build is installed" "1" "$(V "test -d /var/lib/atrium/apps/dev.alice.doomed.org.example.hello && echo 1")"
check "and granted" "1" "$(V "grep -c 'dev.alice.doomed.org.example.hello' /var/db/atrium/alice/policy.toml 2>/dev/null | grep -c '[1-9]'")"
sleep 8                 # the app's list shows doomed (after Scratch)
press 130 140
press 377 374           # Delete workspace…
press 347 396           # Delete (a press)
i=0; while [ $i -lt 30 ] && V "test -d /var/lib/atrium/apps/dev.alice.doomed.org.example.hello"; do sleep 2; i=$((i+2)); done
check "its dev build is gone" "" "$(V "ls -d /var/lib/atrium/apps/dev.alice.doomed.* 2>/dev/null")"
check "and its grant" "0" "$(V "grep -c 'dev.alice.doomed' /var/db/atrium/alice/policy.toml 2>/dev/null || true")"
check "Scratch's stays" "1" "$(V "test -d /var/lib/atrium/apps/dev.alice.scratch.org.example.hello && echo 1")"
shot run-2

echo "run: $fails failed"
[ $fails = 0 ]
