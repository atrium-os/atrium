#!/bin/sh
# Duplicate workspace (docs/spec/homes.md §5.4, W8) in the amd64 cloud VM
# with frescod headless: a copy brings the work and the root (build output
# and installed tools) along, re-owned to the copy's own uid, the home in
# its root renamed; the original is unchanged; a name in use or a bad name
# is refused; the Workspaces app's "Duplicate…" does the same with its
# form. Runs on the HOST after cloud-vm.sh up; needs the VM's jaild. Ends
# "duplicate: N failed".
#
#   sh scripts/vm-duplicate-check.sh [out-dir]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/duplicate-shots}
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
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "duplicate: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe"
xb stoa "-p stoa --bin stoad"
xb atrium-workspaces "--bin atrium-workspaces"

echo "=== install"
B=/var/tmp/dupchk
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe \
         stoa/target/$TARGET/release/stoad atrium-workspaces/target/$TARGET/release/atrium-workspaces; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "duplicate: 1 failed"; exit 1; }
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
[ $? = 0 ] && ok "portcullisd and the four apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "duplicate: $fails failed"; exit 1; }
V "su -m alice -c 'portcullis policy grant $STAY; portcullis policy grant $SYS' >/dev/null 2>&1"
for a in $LAUNCHER $TERM_ID $WS_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "duplicate: $fails failed"; exit 1; }
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
for w in scratch web lib orig copy first viaapp; do V "su -m alice -c 'portcullis workspace stop $w'" >/dev/null 2>&1; done
V "for m in \$(mount -p | awk '\$2 ~ /workspaces\/alice/ {print \$2}' | sort -r); do umount -f \$m; done; rm -rf $W /var/lib/atrium/workspaces/alice"
V "pkill -f atrium-workspaces; pkill -f atrium-term-socket; sleep 1"
uidof() { V "awk '\$2 == \"alice\" && \$3 == \"workspace/$1\" {print \$1}' /var/run/atrium/app-registry | head -1"; }

echo "=== orig, with a build and a tool in its root"
V "su -m alice -c 'portcullis workspace list'" >/dev/null 2>&1
out=$(V "su -m alice -c 'portcullis workspace new orig --network none'" 2>&1)
echo "$out" | grep -qi orig && ok "orig made" || bad "new orig: $out"
# A workspace's uid is registered when it first starts or is copied: a
# first copy (of an empty workspace) gives orig its uid.
out=$(V "su -m alice -c 'portcullis workspace duplicate orig first'" 2>&1)
check "an empty workspace duplicates" "1" "$(V "test -d $W/first/work && echo 1")"
OU=$(uidof orig)
V 'sh -s' <<EOS
set -e
P=$W/orig/work/hello; mkdir -p \$P/target
printf 'int main(void){return 0;}\n' > \$P/hello.c
cc -o \$P/target/hello \$P/hello.c
echo plan > \$P/notes.txt
chown -R alice:files-alice $W/orig/work
chown $OU \$P/target/hello
ln -s target/hello \$P/latest; chown -h $OU \$P/latest
R=$W/orig/.state/root; mkdir -p \$R/usr/local/bin \$R/home/ws-orig
echo tool > \$R/usr/local/bin/tool; chown $OU \$R/usr/local/bin/tool
echo rc > \$R/home/ws-orig/.profile
EOS
[ $? = 0 ] && ok "a build in orig's work and a tool in its root (uid $OU)" || bad "orig's build"
OHOME=$(V "awk -F: -v u=$OU '\$3 == u {print \$1}' /etc/passwd | head -1")
[ -n "$OHOME" ] && V "mv $W/orig/.state/root/home/ws-orig $W/orig/.state/root/home/$OHOME" 2>/dev/null

echo "=== duplicate from the CLI"
out=$(V "su -m alice -c 'portcullis workspace duplicate orig copy'" 2>&1)
check "duplicate succeeds" "0" "$?"
CU=$(uidof copy)
[ -n "$CU" ] && [ "$CU" != "$OU" ] && ok "copy has its own uid ($CU, orig $OU)" || bad "copy uid [$CU] orig [$OU]: $out"
check "work came along" "plan" "$(V "cat $W/copy/work/hello/notes.txt")"
check "build output too" "1" "$(V "test -x $W/copy/work/hello/target/hello && echo 1")"
check "re-owned to copy's uid" "$CU" "$(V "stat -f %u $W/copy/work/hello/target/hello")"
check "a link copied as a link, re-owned" "target/hello $CU" "$(V "readlink $W/copy/work/hello/latest; stat -f %u $W/copy/work/hello/latest" | tr '\n' ' ' | sed 's/ $//')"
check "alice's files stay alice's" "alice" "$(V "stat -f %Su $W/copy/work/hello/notes.txt")"
check "the root's tool came, re-owned" "tool $CU" "$(V "cat $W/copy/.state/root/usr/local/bin/tool; stat -f %u $W/copy/.state/root/usr/local/bin/tool" | tr '\n' ' ' | sed 's/ $//')"
CHOME=$(V "awk -F: -v u=$CU '\$3 == u {print \$1}' /etc/passwd | head -1")
if [ -n "$OHOME" ]; then
  check "its home in the root renamed" "rc" "$(V "cat $W/copy/.state/root/home/$CHOME/.profile 2>/dev/null")"
fi
check "the record is named copy" "1" "$(V "grep -c '^name = \"copy\"' $W/copy/.state/workspace.toml")"
check "orig is unchanged" "$OU plan" "$(V "stat -f %u $W/orig/work/hello/target/hello; cat $W/orig/work/hello/notes.txt" | tr '\n' ' ' | sed 's/ $//')"
V "su -m alice -c 'portcullis workspace list'" 2>&1 | grep -q copy && ok "list shows copy" || bad "list: $(V "su -m alice -c 'portcullis workspace list'" 2>&1)"
out=$(V "su -m alice -c 'portcullis workspace duplicate orig copy'" 2>&1)
echo "$out" | grep -q "already" && ok "a name in use is refused" || bad "in use: $out"
out=$(V "su -m alice -c 'portcullis workspace duplicate orig ../x'" 2>&1)
echo "$out" | grep -q "a name is" && ok "a bad name is refused" || bad "bad name: $out"
out=$(V "su -m alice -c 'portcullis workspace duplicate nosuch y'" 2>&1)
echo "$out" | grep -q "no workspace" && ok "a missing workspace is refused" || bad "missing: $out"
check "and nothing made for them" "" "$(V "ls -d $W/x $W/y 2>/dev/null")"

echo "=== duplicate from the app"
V "daemon -f -o $B/ws.log su -m alice -c 'portcullis launch --no-prompt $WS_ID'"
i=0; while [ $i -lt 60 ] && ! V "grep -q 'is app $WS_ID' $LOG"; do sleep 2; i=$((i+2)); done
sleep 8
shot dup-0
press 130 140            # copy (Scratch first, then copy, first, orig)
shot dup-1
press 539 430            # Duplicate… (under three uses rows)
shot dup-2
press 400 104            # the name field
typ "viaapp"
shot dup-3a
press 372 220            # Duplicate
i=0; while [ $i -lt 60 ] && ! V "test -f $W/viaapp/.state/workspace.toml"; do sleep 2; i=$((i+2)); done
shot dup-3
check "the app's Duplicate makes viaapp" "1" "$(V "grep -c '^name = \"viaapp\"' $W/viaapp/.state/workspace.toml 2>/dev/null")"
VU=$(uidof viaapp)
check "from copy, re-owned to its uid" "$VU" "$(V "stat -f %u $W/viaapp/work/hello/target/hello 2>/dev/null")"
V "grep -q 'duplicated as viaapp' $LOG" && ok "portcullisd logged it" || bad "log: $(V "grep -i dupl $LOG | tail -2")"

echo "duplicate: $fails failed"
[ $fails = 0 ]
