#!/bin/sh
# Homes W3 (docs/spec/homes.md §5, §8): workspaces, in the amd64 cloud VM,
# with frescod headless and real presses. Runs on the HOST after
# cloud-vm.sh up and the open-files gate's users (alice); needs the VM's
# jaild (a workspace's outbound network). Ends "workspace: N failed".
#
#   sh scripts/vm-workspace-check.sh [out-dir]
#
#   - `portcullis workspace` as alice: Scratch is there at the first ask;
#     liba (no network) and appb (uses liba) are made; a toolchain not yet
#     available, an unknown link and a bad name are refused;
#   - a click (the launcher, org.test.launcher) opens Scratch's terminal:
#     the workspace's uid with the files group, the base system read-only,
#     ~/work; a file handed in (as the file manager would) builds with
#     `cc hello.c && ./a.out`, and what it makes is the files group's;
#   - Scratch has a routed interface (outbound), liba only lo0;
#   - liba builds a static library; appb sees liba's work read-only at
#     /workspaces/liba and links against it;
#   - settings from outside: appb's link removed stops it; `stop` takes
#     Scratch's jail and mounts away, its files stay.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/workspace-shots}
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
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "workspace: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe"
xb stoa "-p stoa --bin stoad"

echo "=== install"
B=/var/tmp/workspace
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe \
         stoa/target/$TARGET/release/stoad; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "workspace: 1 failed"; exit 1; }
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
mk $LAUNCHER Launcher console_probe console_probe 'graphics = "fresco"\napp-launch = true' 'workspace/scratch\nworkspace/liba\nworkspace/appb\nworkspace/scratch\n' targets
mk $TERM_ID Terminal atrium-term-socket atrium-term-socket 'graphics = "fresco"\nconsoles = true' 'on\n' .trace
mkdir -p /atrium/sockets/portcullis /var/run/atrium; rm -f $PSOCK /var/run/atrium/stoad.sock
: > $LOG
daemon -f -o $B/stoad.log env STOA_CTL=/var/run/atrium/stoad.sock STOA_STATE=off $B/stoad
sleep 1; chmod 600 /var/run/atrium/stoad.sock
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd and the four apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "workspace: $fails failed"; exit 1; }
V "su -m alice -c 'portcullis policy grant $STAY; portcullis policy grant $SYS' >/dev/null 2>&1"
for a in $LAUNCHER $TERM_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "workspace: $fails failed"; exit 1; }
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

echo "=== jaild (a workspace's network)"
V "service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1; sleep 1; test -S /var/run/atrium/jaild.sock" && ok "jaild is up" || bad "no jaild"

W=/home/alice/.atrium/workspaces
wsl() { V "su -m alice -c 'portcullis workspace $1'" 2>&1; }
echo "=== workspaces, as alice"
for w in scratch liba appb; do V "su -m alice -c 'portcullis workspace stop $w'" >/dev/null 2>&1; done
V "for j in \$(jls name | grep '^ws_alice_'); do jail -r \$j; done; for m in \$(mount -p | awk '\$2 ~ /workspaces\/alice/ {print \$2}' | sort -r); do umount -f \$m; done; rm -rf $W /var/lib/atrium/workspaces/alice"
L=$(wsl list)
case "$L" in scratch*c*personal*outbound*stopped*) ok "Scratch is there at the first ask" ;; *) bad "list: $L" ;; esac
check "work is alice's, the files group's" "alice files-alice 2770" "$(V "stat -f '%Su %Sg %Mp%Lp' $W/scratch/work")"
check "the state is root's alone" "root 700" "$(V "stat -f '%Su %Lp' $W/scratch/.state")"
wsl "new liba --network none" >/dev/null
L=$(wsl "new appb --network none --uses liba")
echo "$L" | grep -q "^appb.*uses=liba" && ok "appb uses liba" || bad "new appb: $L"
case "$(wsl 'new rusty --toolchain rust')" in *"needs its toolchain layer"*) ok "a toolchain without its layer is refused" ;; *) bad "rust toolchain accepted" ;; esac
case "$(wsl 'new c2 --uses nosuch')" in *"no workspace"*) ok "an unknown link is refused" ;; *) bad "unknown link accepted" ;; esac
case "$(wsl 'new ../x')" in *"a name is"*) ok "a bad name is refused" ;; *) bad "bad name accepted" ;; esac
check "the project file is written" "1" "$(V "grep -c 'language = \"c\"' $W/liba/work/.atrium/devjail.toml" | tr -d ' ')"

# Files handed in, as the file manager would (alice's, the files group's).
put() { # workspace file content
    printf '%b' "$3" > /tmp/ws-file; scp -q $K -P 2222 /tmp/ws-file root@localhost:$W/$1/work/$2
    V "chown alice:files-alice $W/$1/work/$2; chmod 664 $W/$1/work/$2"
}
put scratch hello.c '#include <stdio.h>\nint main(void) { puts("hello from scratch"); return 0; }\n'
put liba greet.c '#include <stdio.h>\n#include "greet.h"\nvoid greet(void) { puts("hello from liba"); }\n'
put liba greet.h 'void greet(void);\n'
put appb main.c '#include "greet.h"\nint main(void) { greet(); return 0; }\n'

# A click on the launcher for the next workspace: the first click on it
# when another window has focus may only focus it, so once more if nothing
# was asked.
ask_ws() {
    # Headless frescod has no window manager: a click doesn't move focus,
    # and a press on an unfocused window is refused. Closing the terminal
    # (its sessions detach) gives the focus back to the launcher.
    V "pkill -f atrium-term-socket"; sleep 6
    press 60 60
    i=0; while [ $i -lt 10 ] && ! V "grep -q 'console-probe: workspace/$1 ->' $B/launcher.log"; do sleep 2; i=$((i+2)); done
    if ! V "grep -q 'console-probe: workspace/$1 -> ok' $B/launcher.log"; then
        # The probe asks for the next target on each click: put this one back.
        V "grep -q 'console-probe: workspace/$1 -> err' $B/launcher.log" && echo "  (the first click only focused the launcher)"
        press 60 60
    fi
    i=0; while [ $i -lt 40 ] && ! V "grep -q 'console-probe: workspace/$1 -> ok' $B/launcher.log"; do sleep 2; i=$((i+2)); done
}

echo "=== Scratch's terminal"
V "daemon -f -o $B/launcher.log su -m alice -c 'portcullis launch --no-prompt $LAUNCHER'"
i=0; while [ $i -lt 40 ] && ! V "grep -q 'console-probe: nopress' $B/launcher.log"; do sleep 2; i=$((i+2)); done
case "$(V "grep 'console-probe: nopress' $B/launcher.log")" in *"-> err"*press*) ok "no press, no terminal" ;; *) bad "without a press: $(V "grep nopress $B/launcher.log")" ;; esac
sleep 4
press 60 60
i=0; while [ $i -lt 40 ] && ! V "grep -q 'console-probe: workspace/scratch ->' $B/launcher.log"; do sleep 2; i=$((i+2)); done
check "a click opens it" "ok Workspace: scratch" "$(V "grep 'console-probe: workspace/scratch ->' $B/launcher.log | head -1" | sed 's/.*-> //')"
waitscreen "scratch \\$" 60 && ok "its prompt" || bad "no prompt: $(screen) / $(V "grep -i workspace $LOG | tail -5")"
WU=$(V "grep 'workspace scratch of alice started' $LOG | tail -1" | sed -n 's/.*uid \([0-9]*\),.*/\1/p')
[ -n "$WU" ] && ok "Scratch runs as its own uid ($WU)" || bad "no start line: $(V "grep -i workspace $LOG | tail -3")"
shot workspace-1
press 600 500
typ "id; pwd
"
waitscreen "uid=$WU" 20 && ok "the workspace's uid" || bad "id: $(screen)"
screen | grep -q "files" && ok "with the files group" || bad "no files group: $(screen)"
screen | grep -q "/work" && ok "it starts in ~/work" || bad "pwd: $(screen)"
typ "cc hello.c && ./a.out
"
waitscreen "hello from scratch" 90 && ok "cc hello.c && ./a.out" || bad "build: $(screen)"
check "what it made is in work, the files group's" "files-alice" "$(V "stat -f %Sg $W/scratch/work/a.out" 2>&1)"
typ "touch /usr/bin/x; ls /home
"
waitscreen "Read-only" 20 && ok "the base system is read-only" || bad "base write: $(screen)"
screen | grep -q "alice" && bad "a person's home is in reach" || ok "no person's home"
shot workspace-2
check "Scratch is routed (outbound)" "1" "$(V "jexec ws_alice_scratch ifconfig -l | grep -c epair" | tr -d ' ')"

echo "=== liba: a library"
ask_ws liba
waitscreen "liba \\$" 60 && ok "liba's terminal" || bad "liba: $(screen)"
press 640 520
typ "cc -c greet.c && ar rcs libgreet.a greet.o && ls
"
waitscreen "libgreet.a" 60 && ok "it builds libgreet.a" || bad "ar: $(screen)"
check "liba has only lo0 (no network)" "lo0" "$(V "jexec ws_alice_liba ifconfig -l" | tr -d '\r')"

echo "=== appb uses liba"
ask_ws appb
waitscreen "appb \\$" 60 && ok "appb's terminal" || bad "appb: $(screen)"
press 680 540
typ "cc -I/workspaces/liba main.c -L/workspaces/liba -lgreet && ./a.out
"
waitscreen "hello from liba" 90 && ok "appb links liba's library" || bad "link: $(screen)"
typ "touch /workspaces/liba/x
"
waitscreen "Read-only" 20 && ok "and can't change liba" || bad "liba writable from appb: $(screen)"
shot workspace-3

echo "=== settings, from outside"
L=$(wsl "set appb --no-uses")
echo "$L" | grep "^appb" | grep -qv "uses=liba" && ok "appb's link removed" || bad "set: $L"
check "a change stops it" "" "$(V "jls -j ws_alice_appb jid 2>/dev/null")"
check "and its mounts go" "0" "$(V "mount -p | grep -c 'workspaces/alice/appb'" | tr -d ' ')"
wsl "stop scratch" >/dev/null
check "stop: Scratch's jail is gone" "" "$(V "jls -j ws_alice_scratch jid 2>/dev/null")"
check "its mounts too" "0" "$(V "mount -p | grep -c 'workspaces/alice/scratch'" | tr -d ' ')"
check "its files stay" "1" "$(V "test -f $W/scratch/work/a.out && echo 1")"

echo "=== Terminal on Scratch again starts it"
V "pkill -f atrium-term-socket"; sleep 6
press 60 60
sleep 6
[ "$(V "grep -c 'console-probe: workspace/scratch ->' $B/launcher.log" | tr -d ' ')" -lt 2 ] && press 60 60
i=0; while [ $i -lt 40 ] && [ "$(V "grep -c 'console-probe: workspace/scratch ->' $B/launcher.log" | tr -d ' ')" -lt 2 ]; do sleep 2; i=$((i+2)); done
check "the jail is back" "1" "$(V "jls -j ws_alice_scratch jid >/dev/null 2>&1 && echo 1")"

echo "workspace: $fails failed"
[ "$fails" -eq 0 ]
