#!/bin/sh
# The Workspaces app (docs/spec/homes.md §5), driven by clicks in the amd64
# cloud VM with frescod headless: list, new (name, network), open a
# terminal, switch, uses, network, delete (press-confirmed). Runs on the
# HOST after cloud-vm.sh up; needs the VM's jaild. Ends
# "workspaces-app: N failed". PROBE=1 stops after the first screenshot.
#
#   sh scripts/vm-workspaces-app-check.sh [out-dir]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/workspaces-app-shots}
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
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "workspaces-app: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe"
xb stoa "-p stoa --bin stoad"
xb atrium-workspaces "--bin atrium-workspaces"

echo "=== install"
B=/var/tmp/wsapp
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe \
         stoa/target/$TARGET/release/stoad atrium-workspaces/target/$TARGET/release/atrium-workspaces; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "workspaces-app: 1 failed"; exit 1; }
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
[ $? = 0 ] && ok "portcullisd and the four apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "workspaces-app: $fails failed"; exit 1; }
V "su -m alice -c 'portcullis policy grant $STAY; portcullis policy grant $SYS' >/dev/null 2>&1"
for a in $LAUNCHER $TERM_ID $WS_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "workspaces-app: $fails failed"; exit 1; }
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

echo "=== the app"
# A jailed app gets no environment from the launch, so the checks read the
# host's answer (portcullis workspace list) and the screenshots.
V "daemon -f -o $B/ws.log su -m alice -c 'portcullis launch --no-prompt $WS_ID'"
L() { V "su -m alice -c 'portcullis workspace list'" 2>/dev/null; }
row() { L | awk -F'\t' -v n="$1" '$1 == n'; }
waitfor() { # command pattern [secs]
    i=0; while [ $i -lt ${3:-30} ] && ! eval "$1" | grep -q -- "$2"; do sleep 2; i=$((i+2)); done
    eval "$1" | grep -q -- "$2"
}
waitfor "V 'grep -c workspaces $B/portcullisd.log'" "[1-9]" 60 && ok "the app asked portcullisd" || bad "the app never asked: $(V "tail -3 $B/ws.log")"
waitfor L "^scratch" 30 && ok "Scratch is made on first list" || bad "no scratch: $(L)"
shot app-1
[ "${PROBE:-0}" = 1 ] && { echo "workspaces-app: probe"; exit 0; }

# Layout at scale 1 (lib.rs): list 260 wide, rows from y 64 every 52;
# the right side starts at x 292.
rowy() { echo $((88 + 52 * $1)); }
echo "=== new"
press 130 560
shot new-1
press 400 104
typ "web"
press 457 284          # Network: None
shot new-2
press 372 340          # Create and open
waitfor "row web" "web" 60 && ok "web is made" || bad "no web: $(L)"
check "with no network" "none" "$(row web | cut -f4)"
waitfor "V 'jls name'" "^ws_alice_web$" 60 && ok "and its terminal opened it" || bad "web's jail: $(V 'jls name')"
waitfor "V 'ps ax -o args='" "atrium-term-socket" 30 && ok "a terminal runs" || bad "no terminal"
shot new-3
V "pkill -f atrium-term-socket"; sleep 6

echo "=== switch and uses"
press 130 $(rowy 0)    # Scratch
shot switch-1
press 330 270         # Uses: Web
waitfor "row scratch | cut -f5" "uses=web" 30 && ok "Scratch uses Web" || bad "uses: $(row scratch)"
press 130 $(rowy 1)    # Web
shot switch-2
press 377 374
check "Web, being used, stays" "web" "$(row web | cut -f1)"
press 130 $(rowy 0)
press 330 270
waitfor "row scratch | cut -f5" "^uses=$" 30 && ok "and stops using it" || bad "uses: $(row scratch)"

echo "=== network"
press 457 182
waitfor "row scratch | cut -f4" "none" 30 && ok "Scratch: None" || bad "network: $(row scratch)"
press 347 182
waitfor "row scratch | cut -f4" "outbound" 30 && ok "Scratch: Outbound again" || bad "network: $(row scratch)"
# Registries (dev-tools.md §5.2): the proxy's host list, edited in place.
# Layout: the network row's top is 166; the list starts at y 228, a row
# every 26 (8 default hosts); the field and Add sit below it.
press 402 182          # Network: Registries
waitfor "row scratch | cut -f4" "registries" 30 && ok "Scratch: Registries" || bad "network: $(row scratch)"
check "the default hosts listed" "8 (default)" "$(row scratch | cut -f8 | sed 's/^hosts=//' | awk -F' ' '{n=split($1,a,","); print n, $2}')"
shot registries-1
press 407 452          # the host field
typ "proxy.golang.org"
press 572 452          # Add
waitfor "row scratch | cut -f8" "proxy.golang.org" 30 && ok "a host added" || bad "hosts: $(row scratch | cut -f8)"
check "the list is its own now" "9" "$(row scratch | cut -f8 | sed 's/^hosts=//' | awk -F, '{print NF}')"
shot registries-2
press 572 232          # Remove crates.io
waitfor "row scratch | cut -f8 | grep -c 'hosts=crates.io,' || true" "^0$" 30 && ok "a host removed" || bad "hosts: $(row scratch | cut -f8)"
press 697 452          # Use the defaults
waitfor "row scratch | cut -f8" "(default)" 30 && ok "back to the defaults" || bad "hosts: $(row scratch | cut -f8)"
press 347 182
waitfor "row scratch | cut -f4" "outbound" 30 && ok "Scratch: Outbound once more" || bad "network: $(row scratch)"

echo "=== open and stop"
press 367 104
waitfor "V 'jls name'" "^ws_alice_scratch$" 60 && ok "Open terminal starts Scratch" || bad "scratch's jail: $(V 'jls name')"
V "pkill -f atrium-term-socket"; sleep 8
shot open-1
press 499 104         # Stop
waitfor "V 'jls name | grep -c ^ws_alice_scratch$ || true'" "^0$" 30 && ok "Stop stops it" || bad "still running: $(V 'jls name')"

echo "=== delete"
press 130 $(rowy 1)    # Web, no one uses it now
press 377 374          # Delete workspace…
shot delete-1
press 347 396          # Delete (takes the press)
waitfor "L | cut -f1 | grep -c '^web$' || true" "^0$" 30 && ok "Web is deleted" || bad "web: $(L)"
check "its folder is gone" "" "$(V "test -e $W/web && echo there")"
check "and its jail" "" "$(V "jls name | grep '^ws_alice_web$'")"
shot delete-2

echo "workspaces-app: $fails failed"
[ $fails = 0 ]
