#!/bin/sh
# Terminal on its own (docs/spec/homes.md §5.3, W7), in the amd64 cloud VM
# with frescod headless: opened from nothing it gets the empty console (a
# shell that reaches nothing: no home, no network, no app's files, its own
# uid); its switcher moves the window into a workspace with a press, and
# back, reattaching. Runs on the HOST after cloud-vm.sh up; needs the VM's
# jaild. Ends "terminal: N failed".
#
#   sh scripts/vm-terminal-check.sh [out-dir]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/terminal-shots}
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
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "terminal: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe"
xb stoa "-p stoa --bin stoad"

echo "=== install"
B=/var/tmp/termchk
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe \
         stoa/target/$TARGET/release/stoad; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "terminal: 1 failed"; exit 1; }
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
mkdir -p /atrium/sockets/portcullis /var/run/atrium; rm -f $PSOCK /var/run/atrium/stoad.sock
: > $LOG
daemon -f -o $B/stoad.log env STOA_CTL=/var/run/atrium/stoad.sock STOA_STATE=off $B/stoad
sleep 1; chmod 600 /var/run/atrium/stoad.sock
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd and the apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "terminal: $fails failed"; exit 1; }
V "su -m alice -c 'portcullis policy grant $STAY; portcullis policy grant $SYS' >/dev/null 2>&1"
for a in $LAUNCHER $TERM_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "terminal: $fails failed"; exit 1; }
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

echo "=== the terminal"
# Launched by the CLI, the terminal's stderr is its own log.
screen() { V "grep 'atrium-term: screen' $B/term.log | tail -1"; }
V "pkill -f atrium-term-socket; sleep 2; for j in \$(jls name | grep '^org_atrium_terminal\$'); do jail -r \$j; done"
V "for j in \$(jls name | grep '^empty_alice\$'); do jail -r \$j; done; for m in \$(mount -p | awk '\$2 ~ /atrium\/empty\/alice/ {print \$2}' | sort -r); do umount -f \$m; done"
V "su -m alice -c 'portcullis workspace list'" >/dev/null 2>&1   # makes Scratch

chord() { # a key with Ctrl+Shift
    inj "KEY 0xe0 1 1" "KEY 0xe1 1 3" "KEY $1 1 3" "KEY $1 0 3" "KEY 0xe1 0 1" "KEY 0xe0 0 0"; sleep ${GAP:-4}
}
k1() { inj "KEY $1 1" "KEY $1 0"; sleep ${GAP:-4}; }

echo "=== on its own"
V "daemon -f -o $B/term.log su -m alice -c 'portcullis launch --no-prompt $TERM_ID'"
waitscreen "\[Empty" 60 && ok "Terminal alone opens the empty console" || bad "no empty console: $(screen) $(V "tail -5 $LOG")"
waitscreen "empty \\$" 20 && ok "with its prompt" || bad "prompt: $(screen)"
typ "id -u; ls /; ls /home; echo end-of-looking
"
waitscreen "| end-of-looking |" 20
S=$(screen)
echo "$S" | grep -q "No such file" && ok "no home to look at" || bad "home: $S"
echo "$S" | grep -Eq "\| atrium +dev +etc +root +tmp \|" && ok "its root holds only the toolbox, dev, etc, root and tmp" || bad "root: $S"
U=$(V "ps -J \$(jls -j empty_alice jid) -o uid= | sort -u | tr -d ' '")
case "$U" in ''|0) bad "the shell's uid: [$U]";; *) [ "$(V "id -u alice")" != "$U" ] && ok "its own uid ($U), not alice's" || bad "runs as alice";; esac
check "no network but loopback" "lo0" "$(V "ifconfig -j empty_alice -l 2>/dev/null")"
check "its jail is the empty one" "1" "$(V "jls name | grep -c '^empty_alice\$'")"
shot term-1

echo "=== switch to Scratch"
chord 0x12
waitscreen "menu=Empty,Workspace: scratch" 20 && ok "the switcher lists Empty and Scratch" || bad "menu: $(screen)"
shot term-2
k1 0x51
k1 0x28
waitscreen "\[Workspace: scratch\]" 60 && ok "Enter moves the window to Scratch" || bad "switch: $(screen) $(V "grep REFUSED $LOG | tail -2")"
waitscreen "scratch \\$" 20 && ok "its prompt" || bad "prompt: $(screen)"
typ "pwd
"
waitscreen "/work" 20 && ok "in ~/work" || bad "pwd: $(screen)"
check "Scratch is running" "1" "$(V "jls name | grep -c '^ws_alice_scratch\$'")"
shot term-3

echo "=== back to Empty"
chord 0x12
k1 0x52
k1 0x28
waitscreen "\[Empty" 30 && ok "back in the empty console" || bad "back: $(screen)"
waitscreen "| end-of-looking |" 20 && ok "reattached: what it showed before is there" || bad "reattach: $(screen)"

echo "=== another window"
chord 0x11
i=0; while [ $i -lt 30 ] && [ "$(V "grep -c 'atrium-term-socket: window' $LOG")" -lt 2 ]; do sleep 2; i=$((i+2)); done
[ "$(V "grep -c 'atrium-term-socket: window [0-9]* “' $B/term.log")" -ge 2 ] && ok "Ctrl+Shift+N opens a second empty window" || bad "windows: $(V "grep 'atrium-term-socket: window' $B/term.log")"
shot term-4

echo "terminal: $fails failed"
[ $fails = 0 ]
