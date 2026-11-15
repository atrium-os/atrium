#!/bin/sh
# Homes W2 (docs/spec/homes.md §4, §8): app consoles, in the amd64 cloud VM,
# with frescod headless and real presses. Runs on the HOST after
# cloud-vm.sh up and the open-files gate's users (alice). Ends
# "console: N failed".
#
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX \
#     QEMU_EXTRA="-device qemu-xhci" sh scripts/cloud-vm.sh up
#   sh scripts/vm-console-check.sh [out-dir]
#
# Apps (all signed by the gate's publisher key):
#   org.test.stay      home_probe as stay_probe: a third-party app that keeps
#                      running and leaves /var/console-mark in its overlay
#   org.test.sys       the same, asking for status-item: part of the system
#   org.test.launcher  console_probe with app-launch: asks for consoles on
#                      clicks in its window (and once with no press)
#   org.atrium.terminal  atrium-term-socket with `consoles`, tracing screens
#
#   - no press: refused; a click: the console opens, as a window of the
#     terminal's, titled for the app;
#   - in it: the app's uid, its overlay (the mark), its root, no person's
#     files, the toolbox read-only, no host;
#   - Ctrl+Shift+N in the console: a second window, from portcullisd;
#   - a system app: refused, naming why;
#   - Stoa (stoad runs, as root here): Ctrl+Shift+T is a tab (another shell
#     in the session), Ctrl+PageUp goes back; the terminal going away only
#     detaches (the shells run on); Terminal on the app again reattaches,
#     with the screen as it was;
#   - `exit`: that shell ends and is reaped.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/console-shots}
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
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "console: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe"
xb stoa "-p stoa --bin stoad"

echo "=== install"
B=/var/tmp/console
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe \
         stoa/target/$TARGET/release/stoad; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "console: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
STAY=org.test.stay; SYS=org.test.sys; LAUNCHER=org.test.launcher; TERM_ID=org.atrium.terminal
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
mk $LAUNCHER Launcher console_probe console_probe 'graphics = "fresco"\napp-launch = true' '$STAY\n$SYS\n$STAY\n' targets
mk $TERM_ID Terminal atrium-term-socket atrium-term-socket 'graphics = "fresco"\nconsoles = true' 'on\n' .trace
mkdir -p /atrium/sockets/portcullis /var/run/atrium; rm -f $PSOCK /var/run/atrium/stoad.sock
: > $LOG
daemon -f -o $B/stoad.log env STOA_CTL=/var/run/atrium/stoad.sock STOA_STATE=off $B/stoad
sleep 1; chmod 600 /var/run/atrium/stoad.sock
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd and the four apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "console: $fails failed"; exit 1; }
V "su -m alice -c 'portcullis policy grant $STAY; portcullis policy grant $SYS' >/dev/null 2>&1"
for a in $LAUNCHER $TERM_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "console: $fails failed"; exit 1; }
inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $B/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-4}; }
shot() { sleep 3; get $B/screen.png "$OUT/$1.png" 2>/dev/null || get $B/screen "$OUT/$1.png" 2>/dev/null; [ -s "$OUT/$1.png" ] && echo "  screenshot $1.png" || echo "  (no screenshot for $1)"; }
# Type text into the focused window (the terminal tracks Shift by key).
typ() {
    python3 -I - "$1" > /tmp/console-keys.txt <<'PY'
import sys
lo = {c: 0x04 + i for i, c in enumerate("abcdefghijklmnopqrstuvwxyz")}
lo.update({c: 0x1e + i for i, c in enumerate("1234567890")})
lo.update({" ": 0x2c, "-": 0x2d, "=": 0x2e, "/": 0x38, ".": 0x37, ";": 0x33, "'": 0x34, ",": 0x36, "\n": 0x28})
hi = {">": 0x37, "<": 0x36, "_": 0x2d, "|": 0x31, ":": 0x33, '"': 0x34, "&": 0x24}
for c in sys.argv[1]:
    if c in lo:
        print(f"KEY {lo[c]:#x} 1"); print(f"KEY {lo[c]:#x} 0")
    elif c in hi:
        print("KEY 0xe1 1 2"); print(f"KEY {hi[c]:#x} 1 2"); print(f"KEY {hi[c]:#x} 0 2"); print("KEY 0xe1 0 0")
PY
    scp -q $K -P 2222 /tmp/console-keys.txt root@localhost:$B/keys.txt
    V "nc -N -U $B/inject.sock < $B/keys.txt" >/dev/null
    sleep ${GAP:-4}
}
screen() { V "grep 'atrium-term: screen' $LOG | tail -1"; }
waitscreen() { # pattern [secs]
    i=0; while [ $i -lt ${2:-30} ] && ! screen | grep -q -- "$1"; do sleep 2; i=$((i+2)); done
    screen | grep -q -- "$1"
}

echo "=== the app runs; the launcher asks"
V "daemon -f -o $B/stay.log su -m alice -c 'portcullis launch --no-prompt $STAY'"
i=0; while [ $i -lt 30 ] && ! V "jls -j app-$(echo $STAY | tr . -) jid" >/dev/null 2>&1 && ! V "jls name | grep -q stay"; do sleep 2; i=$((i+2)); done
SU=$(V "grep 'launching $STAY as uid' $LOG | tail -1" | sed -n 's/.* as uid \([0-9]*\) .*/\1/p')
[ -n "$SU" ] && ok "$STAY runs as uid $SU" || bad "$STAY did not start: $(V "tail -3 $B/stay.log")"
V "daemon -f -o $B/launcher.log su -m alice -c 'portcullis launch --no-prompt $LAUNCHER'"
i=0; while [ $i -lt 40 ] && ! V "grep -q 'console-probe: nopress' $B/launcher.log"; do sleep 2; i=$((i+2)); done
L=$(V "grep 'console-probe: nopress' $B/launcher.log")
case "$L" in *"-> err"*press*) ok "no press, no console" ;; *) bad "without a press: $L" ;; esac
sleep 4
press 60 60
i=0; while [ $i -lt 30 ] && ! V "grep -q 'console-probe: $STAY ->' $B/launcher.log"; do sleep 2; i=$((i+2)); done
L=$(V "grep 'console-probe: $STAY ->' $B/launcher.log | tail -1")
check "a click on Terminal opens one" "ok Terminal: Stay" "$(echo "$L" | sed 's/.*-> //')"
waitscreen "org.test.stay \\$" 60 && ok "its window shows the prompt" || bad "no prompt: $(screen) / $(V "grep -i 'term\|console' $LOG | tail -5")"
check "the window is the terminal's" "1" "$(V "grep -c 'atrium-term-socket: window .*Terminal: Stay' $LOG" | tr -d ' ')"
screen | grep -qi "termcap" && bad "the shell found no terminal description: $(screen | head -c 200)" || ok "the shell knows its terminal"
shot console-1

echo "=== inside"
press 600 500     # focus the terminal
typ "id
"
waitscreen "uid=$SU" 20 && ok "the app's uid ($SU)" || bad "id: $(screen)"
typ "cat /var/console-mark
"
waitscreen "stay-mark" 20 && ok "its overlay (the mark the app left)" || bad "mark: $(screen)"
typ "ls /
"
waitscreen "bin" 20 && ok "its root" || bad "ls /: $(screen)"
screen | grep -q "Documents" && bad "a person's folder is in reach" || ok "no person's folders"
typ "ls /home/alice /usr/local/bin
"
waitscreen "No such file" 20 && ok "not the host" || bad "host paths: $(screen)"
typ "cp /var/console-mark /atrium/console/x
"
waitscreen "Read-only" 20 && ok "the toolbox is read-only" || bad "toolbox write: $(screen)"
shot console-2

# The jail id, from the first shell stoad took (jls -j can miss a name).
SH1=$(V "sed -n 's/.*took shell pid \\([0-9]*\\).*/\\1/p' $B/stoad.log | head -1")
JID=$(V "ps -o jid= -p $SH1" | tr -d ' ')
echo "  (jail $JID, first shell $SH1)"
shells() { V "ps -ax -o jid= -o args= | awk '\$1==$JID && \$2==\"sh\"' | wc -l" | tr -d ' '; }
check "the shell is stoad's (session console.alice:$STAY:1)" "1" "$(V "grep -c 'console \"console.alice:$STAY:1\" ($STAY) took shell' $B/stoad.log" | tr -d ' ')"

echo "=== a tab: Ctrl+Shift+T, Ctrl+PageUp"
inj "KEY 0xe0 1 1" "KEY 0xe1 1 3" "KEY 0x17 1 3" "KEY 0x17 0 3" "KEY 0xe1 0 1" "KEY 0xe0 0 0"
i=0; while [ $i -lt 30 ] && ! V "grep -q 'took shell pid [0-9]* as window 1' $B/stoad.log"; do sleep 2; i=$((i+2)); done
V "grep -q 'took shell pid [0-9]* as window 1' $B/stoad.log" && ok "another shell in the session (window 1)" || bad "no tab: $(V "tail -3 $B/stoad.log") / $(V "grep 'REFUSED' $LOG | tail -1")"
sleep 3
screen | grep -q "stay-mark" && bad "the tab shows the first shell: $(screen)" || ok "the tab is a new screen"
typ "echo tabtwo
"
waitscreen "tabtwo" 20 && ok "it takes input" || bad "tab input: $(screen)"
check "two shells in the app's jail" "2" "$(shells)"
inj "KEY 0xe0 1 1" "KEY 0x4b 1 1" "KEY 0x4b 0 1" "KEY 0xe0 0 0"
waitscreen "stay-mark" 20 && ok "Ctrl+PageUp: back to the first" || bad "previous tab: $(screen)"
sleep 1; screen | grep -q "tabtwo" && bad "the first tab still shows the second's text: $(screen)" || ok "and only the first's text"
shot console-3

echo "=== another window: Ctrl+Shift+N"
inj "KEY 0xe0 1 1" "KEY 0xe1 1 3" "KEY 0x11 1 3" "KEY 0x11 0 3" "KEY 0xe1 0 1" "KEY 0xe0 0 0"
i=0; while [ $i -lt 30 ] && [ "$(V "grep -c 'atrium-term-socket: window .*Terminal: Stay' $LOG" | tr -d ' ')" -lt 2 ]; do sleep 2; i=$((i+2)); done
check "a second window, from portcullisd" "2" "$(V "grep -c 'atrium-term-socket: window .*Terminal: Stay' $LOG" | tr -d ' ')"
check "in a session of its own" "1" "$(V "grep -c 'console \"console.alice:$STAY:2\" ($STAY) took shell' $B/stoad.log" | tr -d ' ')"
check "three shells now" "3" "$(shells)"

echo "=== a system app"
press 60 60
i=0; while [ $i -lt 30 ] && ! V "grep -q 'console-probe: $SYS ->' $B/launcher.log"; do sleep 2; i=$((i+2)); done
L=$(V "grep 'console-probe: $SYS ->' $B/launcher.log | tail -1")
case "$L" in *"part of the system (status-item)"*) ok "refused: $(echo "$L" | sed 's/.*-> err //')" ;; *) bad "system app: $L" ;; esac

echo "=== the terminal goes: detached, not ended"
V "pkill -f atrium-term-socket"
i=0; while [ $i -lt 20 ] && [ "$(V "grep -c 'detached' $B/stoad.log" | tr -d ' ')" -lt 2 ]; do sleep 2; i=$((i+2)); done
check "both sessions detached" "2" "$(V "grep -c 'detached' $B/stoad.log" | tr -d ' ')"
check "the shells run on" "3" "$(shells)"
sleep 12   # the terminal's jail goes before it is started again

echo "=== Terminal again: reattached"
press 60 60
i=0; while [ $i -lt 30 ] && [ "$(V "grep -c 'console-probe: $STAY ->' $B/launcher.log" | tr -d ' ')" -lt 2 ]; do sleep 2; i=$((i+2)); done
check "the press opens it" "ok Terminal: Stay" "$(V "grep 'console-probe: $STAY ->' $B/launcher.log | tail -1" | sed 's/.*-> //')"
V "grep -q 'reattached for alice' $LOG" && ok "a session left running is reattached" || bad "not reattached: $(V "grep console $LOG | tail -3")"
check "no new shell for it" "3" "$(shells)"
waitscreen "stay-mark" 60 && ok "the screen as it was (the mark, from before)" || bad "screen after reattach: $(screen)"
shot console-4

echo "=== exit"
press 600 500
typ "exit
"
i=0; while [ $i -lt 20 ] && [ "$(shells)" != 2 ]; do sleep 2; i=$((i+2)); done
check "the shell ended" "2" "$(shells)"
check "no zombie shells" "0" "$(V "ps -ax -o jid= -o stat= | awk '\$1==$JID && \$2 ~ /^Z/' | wc -l" | tr -d ' ')"

V "pkill -f console_probe; pkill -f stay_probe; pkill -f atrium-term-socket; pkill -x stoad" >/dev/null 2>&1
echo "console: $fails failed"
